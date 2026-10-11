//! Brush-primitive overlap for volume brushes (retail `AActor::IsOverlapping`).
//!
//! UE2's `AActor::IsOverlapping` (Engine.dll `?IsOverlapping@AActor@@QAEHPAV1@PAUFCheckResult@@@Z`
//! @0x10352bd0) dispatches to a **primitive** test when either actor is a volume brush:
//!
//! * `AActor::IsVolumeBrush` @0x1030f0d0 (`this->Brush` field `0x144` non-null **and**
//!   `this->IsA(AVolume::StaticClass())` at `0x10551c00`).
//! * The dispatch block @0x10352c8d..0x10352cf1 selects the brush actor into `edi` and the other
//!   actor into `esi`, then the primitive path @0x10352cf3..0x10352d9c:
//!   - `esi->GetPrimitive()` twice and `edi->GetPrimitive()` (`AActor::GetPrimitive` @0x1034c570);
//!   - `esi`'s primitive `GetEncroachExtent` @vtable+0x84 (`UPrimitive::GetEncroachExtent`
//!     @0x103c4ca0) and `GetEncroachCenter` @vtable+0x88 (`UPrimitive::GetEncroachCenter`
//!     @0x103c4890) — the other actor's collision box;
//!   - `[edi primitive]->PointCheck(Hit, edi, center, extent, 0)` through
//!     `UPrimitive::PointCheck` @vtable+0x6c (`0x103c4cd0`); for a volume brush the primitive is
//!     the `UModel` of its `Brush` (`UModel::PointCheck` @0x10418ab0). The result is inverted
//!     (`negl`/`sbbl`/`incl` @0x10352d8f): `IsOverlapping` is true when the brush primitive
//!     reports the box inside the brush solid.
//! * Otherwise the vertical-cylinder test @0x10352d9e (`Location` `0xcc/0xd0/0xd4`,
//!   `CollisionRadius` `0x18c`, `CollisionHeight` `0x190`).
//! * The top check @0x10352bdc..0x10352c0f returns 0 when **both** actors are brushes
//!   (`Brush != null && IsA(ABrush::StaticClass())` at `0x10551900`).
//!
//! This module stores the brush's `UModel` BSP in **world (Unreal) space**, precomputed by the
//! host with the same effective placement the world importer applies. The BSP walk mirrors the
//! engine's `RootOutside`/CSG rule (the `xiii-tool bsp --point` diagnostic); a box/cylinder
//! overlaps the brush when any of its sampled points is inside the brush solid.

/// One BSP node of a brush primitive, already transformed to world (Unreal) space.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BrushNode {
    /// Splitting plane (`dot(n, p) = w`), world space.
    pub plane: [f32; 4],
    /// True when the node is a CSG boundary (engine rule: `NumVertices > 0` and neither
    /// `NF_NotCsg` (0x01) nor `NF_IsNew` (0x20) is set); only those nodes update the CSG side.
    pub csg: bool,
    /// Front child node index (`-1` = leaf).
    pub front: i16,
    /// Back child node index (`-1` = leaf).
    pub back: i16,
}

/// A brush's `UModel` BSP in world (Unreal) space.
#[derive(Debug, Clone, PartialEq)]
pub struct BrushPrimitive {
    /// Root node index (the level-model convention: node 0 when nothing references it).
    pub root: usize,
    /// `UModel::RootOutside`: the CSG side at the root.
    pub root_outside: bool,
    /// Nodes.
    pub nodes: Vec<BrushNode>,
}

impl BrushPrimitive {
    /// True when the world point is inside the brush solid: the BSP walk from [`Self::root`]
    /// updating `outside` at each CSG node, then `!outside` (engine `UModel::PointCheck`).
    pub fn contains_point(&self, p: [f32; 3]) -> bool {
        let mut i = self.root;
        let mut outside = self.root_outside;
        // The walk visits each node at most once; the bound is the engine's node count.
        for _ in 0..=self.nodes.len() {
            let Some(n) = self.nodes.get(i) else {
                break;
            };
            let d = n.plane[0] * p[0] + n.plane[1] * p[1] + n.plane[2] * p[2] - n.plane[3];
            let front = d >= 0.0;
            if n.csg {
                outside = front;
            }
            let child = if front { n.front } else { n.back };
            if child < 0 {
                break;
            }
            i = child as usize;
        }
        !outside
    }

    /// True when any of the 27-point box lattice (center, corners and face/edge midpoints) is
    /// inside the brush solid. The other actor's collision box (`center` +/- `extent`, Unreal
    /// axes) is small against a volume brush, so the lattice detects the overlap; a box entirely
    /// outside every sample is outside the solid.
    pub fn overlaps_box(&self, center: [f32; 3], extent: [f32; 3]) -> bool {
        for sx in [-1.0f32, 0.0, 1.0] {
            for sy in [-1.0f32, 0.0, 1.0] {
                for sz in [-1.0f32, 0.0, 1.0] {
                    let p = [
                        center[0] + sx * extent[0],
                        center[1] + sy * extent[1],
                        center[2] + sz * extent[2],
                    ];
                    if self.contains_point(p) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// The other actor's collision **cylinder** as `UModel::PointCheck`'s box. Engine.dll:
    /// `UPrimitive::GetEncroachCenter` @0x103c4890 returns `Owner->Location` (`0xcc`) and
    /// `UPrimitive::GetEncroachExtent` @0x103c4ca0 returns `(CollisionRadius 0x18c,
    /// CollisionRadius, CollisionHeight 0x190)`, so the box is `(center, [R, R, H])`.
    pub fn overlaps_cylinder(&self, center: [f32; 3], radius: f32, half_height: f32) -> bool {
        self.overlaps_box(center, [radius, radius, half_height])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A unit cube `[0,1]^3` as six axis-aligned planes. `RootOutside` 1; every node is a CSG
    /// boundary and its back child is the next node (inside), the last back child is the leaf.
    /// The planes are stored facing **outward** (inside = back), matching the engine's walk.
    fn unit_cube() -> BrushPrimitive {
        // Outward normals with d such that inside is the back side.
        let planes = [
            [1.0, 0.0, 0.0, 1.0],  // x <= 1
            [-1.0, 0.0, 0.0, 0.0], // x >= 0
            [0.0, 1.0, 0.0, 1.0],  // y <= 1
            [0.0, -1.0, 0.0, 0.0], // y >= 0
            [0.0, 0.0, 1.0, 1.0],  // z <= 1
            [0.0, 0.0, -1.0, 0.0], // z >= 0
        ];
        let nodes = planes
            .iter()
            .enumerate()
            .map(|(i, p)| BrushNode {
                plane: *p,
                csg: true,
                front: -1,
                back: if i + 1 < planes.len() {
                    (i + 1) as i16
                } else {
                    -1
                },
            })
            .collect();
        BrushPrimitive {
            root: 0,
            root_outside: true,
            nodes,
        }
    }

    #[test]
    fn point_inside_and_outside_a_box_brush() {
        let b = unit_cube();
        assert!(b.contains_point([0.5, 0.5, 0.5]));
        assert!(!b.contains_point([1.5, 0.5, 0.5]));
        assert!(!b.contains_point([-0.5, 0.5, 0.5]));
        assert!(!b.contains_point([0.5, 0.5, 2.0]));
    }

    /// The task's acceptance for the primitive: a cylinder whose centre is inside the brush
    /// overlaps even though it is far outside a class cylinder anchored on `other`; a cylinder
    /// whose box is entirely outside the brush does not.
    #[test]
    fn cylinder_inside_brush_overlaps_one_outside_does_not() {
        let b = unit_cube();
        // A thin cylinder centred inside the cube.
        assert!(b.overlaps_cylinder([0.5, 0.5, 0.5], 0.05, 0.05));
        // The same cylinder moved entirely outside every face.
        assert!(!b.overlaps_cylinder([2.0, 2.0, 2.0], 0.05, 0.05));
        // A degenerate zero-extent point still overlaps when inside and not when outside.
        assert!(b.overlaps_cylinder([0.5, 0.5, 0.5], 0.0, 0.0));
        assert!(!b.overlaps_cylinder([1.5, 0.5, 0.5], 0.0, 0.0));
    }

    /// A concave (L-shaped) solid: two boxes joined around a notch. A sample in the notch must
    /// be outside, so the primitive test does not over-fire like a world-AABB substitute.
    #[test]
    fn concave_brush_notch_is_outside() {
        // Build an L: [0,2]x[0,1]x[0,1] minus nothing, but the walk is exercised with a solid
        // whose bounding box `[0,2]x[0,2]` includes an empty notch at x>1,y>1. The BSP splits:
        // node0 plane x=1; front(x>=1) -> node1 plane y=1; front(y>=1) is the notch leaf.
        let nodes = vec![
            BrushNode {
                plane: [1.0, 0.0, 0.0, 1.0],
                csg: true,
                front: 1,
                back: 2,
            },
            // x >= 1: inside only when y < 1. front (y>=1) -> outside, back -> inside.
            BrushNode {
                plane: [0.0, 1.0, 0.0, 1.0],
                csg: true,
                front: -1,
                back: -1,
            },
            // x < 1: always inside for this test (back).
            BrushNode {
                plane: [1.0, 0.0, 0.0, 1.0],
                csg: false,
                front: -1,
                back: -1,
            },
        ];
        let b = BrushPrimitive {
            root: 0,
            root_outside: true,
            nodes,
        };
        // Inside the left arm.
        assert!(b.contains_point([0.5, 1.5, 0.5]));
        // In the notch (x>1, y>1): the second node's front sets outside.
        assert!(!b.contains_point([1.5, 1.5, 0.5]));
        // A point only slightly into the bounding box but in the notch is still outside.
        assert!(!b.contains_point([1.999, 1.999, 0.5]));
    }

    /// Boundary/degenerate failure mode: an empty node list and a root that leaves the tree
    /// immediately must never panic and must report outside.
    #[test]
    fn degenerate_primitives_do_not_panic() {
        // No nodes: the CSG side stays `RootOutside` (the engine convention); with
        // `RootOutside` true the empty primitive contains nothing and does not panic.
        let empty = BrushPrimitive {
            root: 3,
            root_outside: true,
            nodes: Vec::new(),
        };
        assert!(!empty.contains_point([0.0, 0.0, 0.0]));
        assert!(!empty.overlaps_box([0.0; 3], [1.0; 3]));
        // A single outward-facing plane: inside is the back side.
        let single = BrushPrimitive {
            root: 0,
            root_outside: false,
            nodes: vec![BrushNode {
                plane: [0.0, 0.0, 1.0, 0.0],
                csg: true,
                front: -1,
                back: -1,
            }],
        };
        assert!(single.contains_point([0.0, 0.0, -1.0]));
        assert!(!single.contains_point([0.0, 0.0, 1.0]));
    }
}
