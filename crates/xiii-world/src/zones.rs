//! BSP zone classification (`BspNode::zone` / leaf) and sky-zone metadata.
//!
//! ## Method (measured layout, documented rule)
//!
//! The level [`Model`] stores a `zones` table (`FZoneProperties`, one entry per zone) and each
//! BSP node records the zone on each side of its split plane (`BspNode::zone`, front/back) and
//! the leaf reached on each side (`BspNode::leaf`). This module converts a source-space point
//! to a zone without instancing the whole world:
//!
//! 1. find the tree root: node `0` when no other node references it through `front`/`back`,
//!    otherwise the single unreferenced node (a forest would fall back to node `0`);
//! 2. walk the tree comparing the point against each node plane (`dot(n, p) = w`): the
//!    positive side follows `front`, the negative side `back`, until a child is `-1`;
//! 3. take that side's leaf index and look it up in [`Model::leaf_zones`] (leaf -> zone).
//!
//! **Measured pairing:** although the node plane's positive side is `front`, the decoded
//! `leaf` slots are paired the other way round on this data: the positive side uses
//! `leaf[1]`, the negative side `leaf[0]`. Cross-checking against the engine-computed
//! `Region.iLeaf` of 336 Plage00 actors, the swapped pairing matches **331** leaves and the
//! unswapped pairing **0**; the five mismatches are editor-set/orphan actors (`Camera`,
//! `PhysicsVolume`, an unused `SkyZoneInfo5` whose `Region` names an out-of-range zone).
//! [`Model::leaf_zones`] pairs `leaf[k]` with `zone[k]`, so the zone lookup stays consistent
//! with either slot order.
//!
//! A BSP polygon belongs to the zone its front face opens into. Its centroid lies on its own
//! node plane, so [`ZoneMap::zone_of_polygon`] nudges the centroid a small distance along the
//! node plane normal before classifying it (otherwise a rounding error could flip the side).
//!
//! Static-mesh actors and the player start are classified from their **effective** placement
//! `Location` in source (Unreal) units, in [`crate::import_map`]. BSP polygons are classified
//! per node. Terrain is left zone-less (drawn by the main view). The engine-computed
//! `Region.ZoneNumber` actor property is not used as the source of truth; it is only read for
//! diagnostics (see `zones.region_mismatches` in the importer).
//!
//! ## Sky zones
//!
//! A zone is a sky zone when its `ZoneActor` class path ends in `skyzoneinfo` (case
//! insensitive). The actor's tagged `Location` is exposed in Bevy space so the viewer can put
//! the sky camera there. `bDistanceFog`/`DistanceFog*` and the ambient byte properties are
//! exposed as decoded; the `Color` channel order is not verified (see the `xiii-package`
//! open questions), so [`ZoneFog::color`] keeps the raw bytes.

use xiii_decode::common::{Props, to_bevy_position};
use xiii_decode::model::{BspNode, BspPolygon, Model};
use xiii_package::{Limits, ObjectRef, Package, PropertyValue, StructValue};

/// `bDistanceFog` / `DistanceFog*` settings of one zone actor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ZoneFog {
    /// `bDistanceFog` when present, else `true` (fog properties were present).
    pub enabled: bool,
    /// Raw `DistanceFogColor` bytes (channel order unverified).
    pub color: [u8; 4],
    /// `DistanceFogStart` in Unreal units (`0.0` when absent).
    pub start: f32,
    /// `DistanceFogEnd` in Unreal units (`0.0` when absent).
    pub end: f32,
}

/// One BSP zone: actor identity, sky flag, actor placement and geometry counts.
#[derive(Debug, Clone, PartialEq)]
pub struct SceneZone {
    /// Zone index (position in the model's zone table).
    pub index: u32,
    /// `ZoneActor` export path (`None` for the null zone-0 actor).
    pub actor_path: Option<String>,
    /// `ZoneActor` class path.
    pub actor_class: Option<String>,
    /// True when the actor's class path ends in `skyzoneinfo`.
    pub is_sky: bool,
    /// Actor `Location` in Bevy space (metres), when it could be read.
    pub location: Option<[f32; 3]>,
    /// Actor `Rotation` (pitch, yaw, roll), or `[0; 3]` when absent.
    pub rotation: [i32; 3],
    /// Connectivity bitmask from the zone record.
    pub connectivity: u64,
    /// Second mask from the zone record (semantics unverified).
    pub visibility: u64,
    /// Distance-fog settings when any fog property was present.
    pub fog: Option<ZoneFog>,
    /// `AmbientBrightness` byte when present.
    pub ambient_brightness: Option<u8>,
    /// `AmbientHue` byte when present.
    pub ambient_hue: Option<u8>,
    /// `AmbientSaturation` byte when present.
    pub ambient_saturation: Option<u8>,
    /// Number of imported BSP polygons assigned to this zone.
    pub polygon_count: usize,
    /// Number of imported objects (static-mesh actors, BSP groups) assigned to this zone.
    pub object_count: usize,
}

/// A prepared BSP tree for source-space point/polygon zone classification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZoneMap {
    root: usize,
    zone_of_leaf: Vec<Option<u8>>,
    leaf_conflicts: u64,
}

impl ZoneMap {
    /// Builds a map from a decoded model's node/leaf tables.
    pub fn new(model: &Model) -> Self {
        let (zone_of_leaf, leaf_conflicts) = model.leaf_zones();
        Self {
            root: find_root(&model.nodes).unwrap_or(0),
            zone_of_leaf,
            leaf_conflicts,
        }
    }

    /// Builds a map from explicit parts (synthetic tests, non-model sources).
    pub fn from_parts(root: usize, zone_of_leaf: Vec<Option<u8>>) -> Self {
        Self {
            root,
            zone_of_leaf,
            leaf_conflicts: 0,
        }
    }

    /// Root node index (node `0` when it is not referenced as a child).
    pub fn root(&self) -> usize {
        self.root
    }

    /// Leaf/zone conflicts found by [`Model::leaf_zones`] (`0` in the whole GOG corpus).
    pub fn leaf_conflicts(&self) -> u64 {
        self.leaf_conflicts
    }

    /// Leaf containing a source-space point, by BSP traversal. `None` when the point leaves
    /// the tree or the reached node has no leaf on that side.
    pub fn leaf_of_point(&self, nodes: &[BspNode], point: [f32; 3]) -> Option<usize> {
        let mut i = self.root;
        for _ in 0..=nodes.len() {
            let n = nodes.get(i)?;
            let d =
                n.plane[0] * point[0] + n.plane[1] * point[1] + n.plane[2] * point[2] - n.plane[3];
            // Positive side follows `front` but reads `leaf[1]` (see the module docs: the
            // decoded leaf slots are paired opposite to the child sides on this data).
            let (child, leaf) = if d >= 0.0 {
                (n.front, n.leaf[1])
            } else {
                (n.back, n.leaf[0])
            };
            if child < 0 {
                return (leaf >= 0).then_some(leaf as usize);
            }
            i = child as usize;
        }
        None
    }

    /// Zone containing a source-space point, or `None` when the leaf has no zone.
    pub fn zone_of_point(&self, nodes: &[BspNode], point: [f32; 3]) -> Option<u8> {
        let leaf = self.leaf_of_point(nodes, point)?;
        self.zone_of_leaf.get(leaf).copied().flatten()
    }

    /// Zone a BSP polygon opens into (centroid nudged along its node plane normal).
    pub fn zone_of_polygon(&self, nodes: &[BspNode], poly: &BspPolygon) -> Option<u8> {
        let n = nodes.get(poly.node)?;
        if poly.vertices.is_empty() {
            return None;
        }
        let mut c = [0.0f32; 3];
        for v in &poly.vertices {
            for k in 0..3 {
                c[k] += v[k];
            }
        }
        let inv = 1.0 / poly.vertices.len() as f32;
        for x in &mut c {
            *x *= inv;
        }
        let eps = 0.05;
        c[0] += n.plane[0] * eps;
        c[1] += n.plane[1] * eps;
        c[2] += n.plane[2] * eps;
        self.zone_of_point(nodes, c)
    }
}

/// Root node of a BSP: node `0` when unreferenced (the conventional root), else the unique
/// unreferenced node. An ambiguous forest (node `0` referenced and several unreferenced nodes,
/// or none) is reported as `None`. `coplanar` links are not children.
pub fn find_root(nodes: &[BspNode]) -> Option<usize> {
    if nodes.is_empty() {
        return None;
    }
    let mut referenced = vec![false; nodes.len()];
    for n in nodes {
        for c in [n.front, n.back] {
            if c >= 0 && (c as usize) < nodes.len() {
                referenced[c as usize] = true;
            }
        }
    }
    if !referenced[0] {
        return Some(0);
    }
    let mut unref = (0..nodes.len()).filter(|&i| !referenced[i]);
    let first = unref.next()?;
    if unref.next().is_some() {
        return None;
    }
    Some(first)
}

/// Reads the zone metadata (actor, sky flag, Bevy location, fog/ambient) from a decoded model.
/// Geometry counts start at zero and are filled by the importer.
pub fn scene_zones(package: &Package, data: &[u8], model: &Model) -> Vec<SceneZone> {
    model
        .zones
        .iter()
        .enumerate()
        .map(|(i, z)| {
            let actor_path = match z.actor {
                ObjectRef::Null => None,
                other => package.object_path(other).map(str::to_owned),
            };
            let actor_class = model.zone_actor_class(package, z).map(str::to_owned);
            let is_sky = model.zone_actor_is_sky(package, z);
            let props = read_zone_actor(package, data, z.actor);
            SceneZone {
                index: i as u32,
                actor_path,
                actor_class,
                is_sky,
                location: props.location,
                rotation: props.rotation,
                connectivity: z.connectivity,
                visibility: z.visibility,
                fog: props.fog,
                ambient_brightness: props.ambient_brightness,
                ambient_hue: props.ambient_hue,
                ambient_saturation: props.ambient_saturation,
                polygon_count: 0,
                object_count: 0,
            }
        })
        .collect()
}

#[derive(Default)]
struct ActorZoneProps {
    location: Option<[f32; 3]>,
    rotation: [i32; 3],
    fog: Option<ZoneFog>,
    ambient_brightness: Option<u8>,
    ambient_hue: Option<u8>,
    ambient_saturation: Option<u8>,
}

/// Reads the tagged properties of a zone actor export. Actors that are not exports of the map
/// (or whose property block fails to decode) yield defaults; the importer counts that case.
fn read_zone_actor(package: &Package, data: &[u8], actor: ObjectRef) -> ActorZoneProps {
    let ObjectRef::Export(e) = actor else {
        return ActorZoneProps::default();
    };
    let Ok(props) = package.read_object_properties(data, e as usize, &Limits::default()) else {
        return ActorZoneProps::default();
    };
    let p = Props::new(package, &props);
    let color = |name: &str| -> Option<[u8; 4]> {
        match p.get(name).map(|x| &x.value) {
            Some(PropertyValue::Struct(StructValue::Color(c))) => Some(*c),
            _ => None,
        }
    };
    let fog = {
        let enabled = p.bool("bDistanceFog");
        let c = color("DistanceFogColor");
        let start = p.float("DistanceFogStart");
        let end = p.float("DistanceFogEnd");
        if enabled.is_none() && c.is_none() && start.is_none() && end.is_none() {
            None
        } else {
            Some(ZoneFog {
                enabled: enabled.unwrap_or(true),
                color: c.unwrap_or([0; 4]),
                start: start.unwrap_or(0.0),
                end: end.unwrap_or(0.0),
            })
        }
    };
    ActorZoneProps {
        location: p.vector("Location").map(to_bevy_position),
        rotation: p.rotator("Rotation").unwrap_or([0; 3]),
        fog,
        ambient_brightness: p.byte("AmbientBrightness"),
        ambient_hue: p.byte("AmbientHue"),
        ambient_saturation: p.byte("AmbientSaturation"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A node with a diagonal-free axis-aligned plane, for hand-built trees.
    fn node(plane: [f32; 4], front: i16, back: i16, leaf_front: i16, leaf_back: i16) -> BspNode {
        BspNode {
            plane,
            zone_mask: 0,
            flags: 0,
            vert_pool: 0,
            surf: 0,
            back,
            front,
            coplanar: -1,
            collision_bound: -1,
            render_bound: -1,
            unknown_a: 0,
            sphere: [0.0; 4],
            zone: [0, 0],
            num_vertices: 0,
            leaf: [leaf_front, leaf_back],
            unknown_b: 0,
            first_vertex: 0,
        }
    }

    fn poly(node: usize, vertices: Vec<[f32; 3]>) -> BspPolygon {
        BspPolygon {
            node,
            surf: 0,
            vertices,
        }
    }

    /// A tiny two-level tree for the measured slot pairing (`leaf[1]` on the positive side,
    /// `leaf[0]` on the negative side): plane X=0 sends the positive half to node 1's Z=0
    /// split and the negative half to leaf 0; node 1 sends its positive half to leaf 1 and its
    /// negative half to leaf 2.
    fn tiny_bsp() -> (Vec<BspNode>, ZoneMap) {
        let nodes = vec![
            // root: X = 0. Positive -> child node 1; negative -> leaf[0] = 0.
            node([1.0, 0.0, 0.0, 0.0], 1, -1, 0, 0),
            // front half: Z = 0. Positive -> leaf[1] = 1; negative -> leaf[0] = 2.
            node([0.0, 0.0, 1.0, 0.0], -1, -1, 2, 1),
        ];
        let map = ZoneMap::from_parts(0, vec![Some(1), Some(2), Some(3)]);
        (nodes, map)
    }

    #[test]
    fn point_in_zone_walks_the_tree() {
        let (nodes, map) = tiny_bsp();
        assert_eq!(map.root(), 0);
        // x > 0, z > 0 -> node 1 positive -> leaf 1 -> zone 2
        assert_eq!(map.zone_of_point(&nodes, [1.0, 0.0, 1.0]), Some(2));
        // x > 0, z < 0 -> node 1 negative -> leaf 2 -> zone 3
        assert_eq!(map.zone_of_point(&nodes, [1.0, 0.0, -1.0]), Some(3));
        // x < 0 -> root negative -> leaf 0 -> zone 1
        assert_eq!(map.zone_of_point(&nodes, [-1.0, 0.0, 0.0]), Some(1));
    }

    #[test]
    fn point_exactly_on_a_plane_goes_to_the_front() {
        let (nodes, map) = tiny_bsp();
        // On both planes -> positive/`front` side -> node 1 -> leaf 1 -> zone 2.
        assert_eq!(map.zone_of_point(&nodes, [0.0, 0.0, 0.0]), Some(2));
    }

    #[test]
    fn polygon_zone_uses_the_front_face() {
        let (nodes, map) = tiny_bsp();
        // A polygon on node 1 (plane Z=0, normal +Z) opens into the positive side (zone 2).
        let p = poly(1, vec![[1.0, 0.0, 0.0], [1.0, 0.0, 1.0], [2.0, 0.0, 0.5]]);
        assert_eq!(map.zone_of_polygon(&nodes, &p), Some(2));
        // Vertex order does not change the plane; the zone is still the positive side.
        let p = poly(1, vec![[1.0, 0.0, 1.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.5]]);
        assert_eq!(map.zone_of_polygon(&nodes, &p), Some(2));
        // A degenerate polygon with no vertices is rejected rather than silently zoned.
        assert_eq!(map.zone_of_polygon(&nodes, &poly(1, vec![])), None);
    }

    #[test]
    fn root_detection_handles_a_forest_and_a_cycle() {
        // Two unreferenced nodes but node 0 is one of them: node 0 is the conventional root.
        let a = node([1.0, 0.0, 0.0, 0.0], -1, -1, 0, 0);
        let b = node([0.0, 1.0, 0.0, 0.0], -1, -1, 0, 0);
        assert_eq!(find_root(&[a, b]), Some(0));
        // A cycle: every node referenced, no root.
        let c = node([1.0, 0.0, 0.0, 0.0], 1, -1, 0, 0);
        let d = node([0.0, 1.0, 0.0, 0.0], -1, 0, 0, 0);
        assert_eq!(find_root(&[c, d]), None);
        // Node 0 referenced and two unreferenced nodes: ambiguous forest.
        let e = node([1.0, 0.0, 0.0, 0.0], 1, -1, 0, 0);
        let f = node([0.0, 1.0, 0.0, 0.0], 0, -1, 0, 0);
        let g = node([0.0, 0.0, 1.0, 0.0], -1, -1, 0, 0);
        let h = node([0.0, 0.0, -1.0, 0.0], -1, -1, 0, 0);
        assert_eq!(find_root(&[e, f, g, h]), None);
    }

    #[test]
    fn missing_leaf_or_zone_is_none_not_zone_zero() {
        let nodes = vec![node([1.0, 0.0, 0.0, 0.0], -1, -1, -1, -1)];
        let map = ZoneMap::from_parts(0, vec![Some(0)]);
        assert_eq!(map.leaf_of_point(&nodes, [1.0, 0.0, 0.0]), None);
        assert_eq!(map.zone_of_point(&nodes, [1.0, 0.0, 0.0]), None);
        // A leaf referenced by the node but with no zone entry is also None.
        let nodes = vec![node([1.0, 0.0, 0.0, 0.0], -1, -1, -1, 5)];
        assert_eq!(map.zone_of_point(&nodes, [1.0, 0.0, 0.0]), None);
    }
}
