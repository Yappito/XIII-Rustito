//! Per-bone hit boxes (item14b): the decoded `SkeletalMesh` trailing hit-box array
//! (`xiii_decode::skeletal::mesh::RawSkeletalMesh::bone_boxes`, `{FBox, u16 mesh bone index}`)
//! posed by the **current animated skeleton** and the actor's placement, so a bullet ray can be
//! classified against the body part it actually passes through.
//!
//! Everything here is Unreal space (Unreal units, X forward / Y right / Z up), the space the
//! script VM traces in. The boxes are expressed in their bone's local space. A box's world pose
//! is the same chain the `--play` pawn renderer applies (`crates/xiii-app/src/play/pawns.rs`,
//! `root_transform`): `world = Location + R_actor * R_rotOrigin * (pose_point - MeshOrigin *
//! MeshScale)`, with `R` built from source rotators. `MeshScale` is deliberately **not** applied
//! to the geometry, matching the renderer, which keeps the root scale at one and uses
//! `MeshOrigin * MeshScale` only for the origin offset; if a mesh in the corpus ever has a
//! non-unit `MeshScale` this must be revisited with a rendered comparison.
//!
//! [`PosedHitZones`] is the host-side `xiii_script::physics::HitZones` provider. It is installed
//! on the VM by `--play`/`--home`; the VM asks it for the nearest box on the ray, and falls back
//! to the collision cylinder when the actor has no posed skeleton (or the ray misses every box).

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use xiii_decode::common::{Mat3, mat3_mul, mat3_transpose, unreal_rotator_matrix};
use xiii_decode::skeletal::math::{Quat, Transform, Vec3};
use xiii_decode::skeletal::normalize::{AnimSet, Clip, Skeleton, evaluate_pose};
use xiii_decode::skeletal::{MESH_ANIMATION_CLASS, SKELETAL_MESH_CLASS, decode_skeletal_mesh};
use xiii_script::ObjectId;
use xiii_script::physics::{CylinderZones, HitZones};

use crate::{Loaded, PackageCache};

/// Mesh-space constants of a decoded skeletal mesh that affect where its hit boxes sit.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshPlacement {
    /// `MeshOrigin` (Unreal units, mesh space).
    pub mesh_origin: [f32; 3],
    /// `MeshScale` (multiplies `MeshOrigin` only; see the module note).
    pub mesh_scale: [f32; 3],
    /// `RotOrigin` (pitch, yaw, roll rotator units).
    pub rot_origin: [i32; 3],
}

/// One hit box in its bone's local space.
#[derive(Debug, Clone, PartialEq)]
pub struct BoneBoxDef {
    /// Mesh bone index the box is attached to.
    pub bone_index: usize,
    /// Bone name (`X Head`, `X Spine1`, ...), the value `GetLastTraceBone` returns.
    pub bone: String,
    /// Box centre in the bone's local frame.
    pub center: [f32; 3],
    /// Box half-extents in the bone's local frame.
    pub half: [f32; 3],
}

/// A decoded mesh's skeleton, animation set and hit boxes, shared by every placed instance.
#[derive(Debug, Clone)]
pub struct HitBoxMesh {
    /// `Package.Mesh` label for diagnostics.
    pub path: String,
    /// Reference skeleton (bind transforms, in the convention `normalize` applies).
    pub skeleton: Skeleton,
    /// `MeshAnimation` clips, when the mesh has one.
    pub anims: Option<AnimSet>,
    /// Skeleton bone -> animation track index.
    pub bone_map: Vec<Option<usize>>,
    /// Hit boxes with valid bone indices.
    pub boxes: Vec<BoneBoxDef>,
    /// Boxes dropped because their bone index is outside the skeleton (reported, never silent).
    pub dropped_boxes: usize,
    /// Mesh-space placement constants.
    pub placement: MeshPlacement,
}

/// Finds an export of `class` whose object name is `name` (case-insensitive).
fn find_export(p: &xiii_package::Package, name: &str, class: &str) -> Option<usize> {
    (0..p.exports().len()).find(|&i| {
        p.export_class_path(i) == Some(class)
            && p.object_name(xiii_package::ObjectRef::Export(i as u32))
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// Loads `Package.Mesh` (a `SkeletalMesh`), its `MeshAnimation` and its decoded hit boxes.
pub fn load_mesh(cache: &mut PackageCache, spec: &str) -> Result<HitBoxMesh, String> {
    let (pkg_name, mesh_name) = spec
        .split_once('.')
        .ok_or_else(|| format!("hit-box mesh {spec:?} must be Package.Mesh"))?;
    let loaded: std::sync::Arc<Loaded> = cache.get(pkg_name)?;
    let mi = find_export(&loaded.package, mesh_name, SKELETAL_MESH_CLASS)
        .ok_or_else(|| format!("no SkeletalMesh {mesh_name} in {}", loaded.name))?;
    let raw = decode_skeletal_mesh(&loaded.package, &loaded.data, mi)
        .map_err(|e| format!("{mesh_name}: {e}"))?;
    let skeleton = Skeleton::from_mesh(&raw).map_err(|e| format!("{mesh_name}: skeleton: {e}"))?;
    let anims = match raw.animation {
        xiii_package::ObjectRef::Null => None,
        r => {
            let (apkg, ai) = cache.resolve(&loaded, r)?;
            if apkg.package.export_class_path(ai) != Some(MESH_ANIMATION_CLASS) {
                return Err(format!(
                    "{spec}: {} is not a MeshAnimation",
                    apkg.package
                        .object_path(xiii_package::ObjectRef::Export(ai as u32))
                        .unwrap_or("?")
                ));
            }
            let raw_anim =
                xiii_decode::skeletal::decode_mesh_animation(&apkg.package, &apkg.data, ai)
                    .map_err(|e| format!("{}: {e}", apkg.name))?;
            Some(AnimSet::from_raw(&raw_anim).map_err(|e| format!("{}: {e}", apkg.name))?)
        }
    };
    let bone_map = anims
        .as_ref()
        .map(|a| a.bone_map(&skeleton))
        .unwrap_or_default();
    let mut boxes = Vec::new();
    let mut dropped_boxes = 0usize;
    for bb in &raw.bone_boxes {
        let bi = usize::from(bb.bone);
        let Some(bone) = skeleton.bones.get(bi) else {
            dropped_boxes += 1;
            continue;
        };
        let center = [
            (bb.bbox.min[0] + bb.bbox.max[0]) * 0.5,
            (bb.bbox.min[1] + bb.bbox.max[1]) * 0.5,
            (bb.bbox.min[2] + bb.bbox.max[2]) * 0.5,
        ];
        let half = [
            (bb.bbox.max[0] - bb.bbox.min[0]).abs() * 0.5,
            (bb.bbox.max[1] - bb.bbox.min[1]).abs() * 0.5,
            (bb.bbox.max[2] - bb.bbox.min[2]).abs() * 0.5,
        ];
        boxes.push(BoneBoxDef {
            bone_index: bi,
            bone: bone.name.clone(),
            center,
            half,
        });
    }
    Ok(HitBoxMesh {
        path: spec.to_owned(),
        skeleton,
        anims,
        bone_map,
        boxes,
        dropped_boxes,
        placement: MeshPlacement {
            mesh_origin: raw.mesh_origin,
            mesh_scale: raw.mesh_scale,
            rot_origin: raw.rot_origin,
        },
    })
}

/// One world-space oriented box (Unreal space).
#[derive(Debug, Clone, PartialEq)]
pub struct WorldBoneBox {
    /// Bone name the box belongs to.
    pub bone: String,
    /// Box centre (Unreal units).
    pub center: [f32; 3],
    /// Row-major rotation matrix `M` with `world = center + M * local`.
    pub axes: Mat3,
    /// Half-extents along the box's local axes.
    pub half: [f32; 3],
}

/// Standard row-major rotation matrix of a unit quaternion (Hamilton, `v' = q v q*`).
fn quat_mat3(q: Quat) -> Mat3 {
    let (x, y, z, w) = (q.x, q.y, q.z, q.w);
    [
        [
            1.0 - 2.0 * (y * y + z * z),
            2.0 * (x * y - w * z),
            2.0 * (x * z + w * y),
        ],
        [
            2.0 * (x * y + w * z),
            1.0 - 2.0 * (x * x + z * z),
            2.0 * (y * z - w * x),
        ],
        [
            2.0 * (x * z - w * y),
            2.0 * (y * z + w * x),
            1.0 - 2.0 * (x * x + y * y),
        ],
    ]
}

/// Applies a row-major matrix to a vector.
fn apply(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    [
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    ]
}

impl WorldBoneBox {
    /// Ray parameter `t` in `0..=1` where `start + t*(end-start)` first enters the box, or
    /// `None`. Slab test in the box's local frame.
    pub fn ray_hit(&self, start: [f32; 3], end: [f32; 3]) -> Option<f32> {
        let inv = mat3_transpose(&self.axes);
        let rel = [
            start[0] - self.center[0],
            start[1] - self.center[1],
            start[2] - self.center[2],
        ];
        let o = apply(&inv, rel);
        let d = apply(
            &inv,
            [end[0] - start[0], end[1] - start[1], end[2] - start[2]],
        );
        let mut tmin = 0.0f32;
        let mut tmax = 1.0f32;
        for k in 0..3 {
            let (o_k, d_k, h) = (o[k], d[k], self.half[k]);
            if d_k.abs() < 1e-9 {
                if o_k < -h || o_k > h {
                    return None;
                }
                continue;
            }
            let t1 = (-h - o_k) / d_k;
            let t2 = (h - o_k) / d_k;
            let (lo, hi) = if t1 < t2 { (t1, t2) } else { (t2, t1) };
            tmin = tmin.max(lo);
            tmax = tmax.min(hi);
            if tmin > tmax {
                return None;
            }
        }
        Some(tmin)
    }
}

/// World-space boxes of `mesh` for `location`/`rotation` (Unreal) and an optional animated pose
/// `(clip, frame, looping)`. `None` uses the bind pose. Boxes whose bone index has no pose entry
/// are skipped (the pose always has one entry per skeleton bone).
pub fn world_boxes(
    mesh: &HitBoxMesh,
    location: [f32; 3],
    rotation: [i32; 3],
    clip: Option<(&Clip, f32, bool)>,
) -> Vec<WorldBoneBox> {
    let pose: Vec<Transform> = match clip {
        Some((c, frame, looping)) => {
            evaluate_pose(&mesh.skeleton, c, &mesh.bone_map, frame, looping)
        }
        None => mesh.skeleton.bind_globals(),
    };
    let actor_rot = unreal_rotator_matrix(rotation);
    let origin_rot = unreal_rotator_matrix(mesh.placement.rot_origin);
    let rot = mat3_mul(&actor_rot, &origin_rot);
    let offset = [
        mesh.placement.mesh_origin[0] * mesh.placement.mesh_scale[0],
        mesh.placement.mesh_origin[1] * mesh.placement.mesh_scale[1],
        mesh.placement.mesh_origin[2] * mesh.placement.mesh_scale[2],
    ];
    let mut out = Vec::with_capacity(mesh.boxes.len());
    for b in &mesh.boxes {
        let Some(p) = pose.get(b.bone_index) else {
            continue;
        };
        let mesh_center = p.transform_point(Vec3::from_array(b.center));
        let rel = [
            mesh_center.x - offset[0],
            mesh_center.y - offset[1],
            mesh_center.z - offset[2],
        ];
        let world_center = apply(&rot, rel);
        out.push(WorldBoneBox {
            bone: b.bone.clone(),
            center: [
                location[0] + world_center[0],
                location[1] + world_center[1],
                location[2] + world_center[2],
            ],
            axes: mat3_mul(&rot, &quat_mat3(p.rotation)),
            half: b.half,
        });
    }
    out
}

/// Shared posed hit-box table keyed by VM `ObjectId`, installed as a `HitZones` provider.
///
/// The owner (`--play` `Session`) clones [`PosedHitZones::handle`] and updates it each fixed
/// step; the VM holds the provider and calls [`HitZones::ray_bone`] during a trace.
#[derive(Clone, Default)]
pub struct PosedHitZones {
    table: Rc<RefCell<HashMap<ObjectId, Vec<WorldBoneBox>>>>,
}

impl PosedHitZones {
    /// Empty provider.
    pub fn new() -> Self {
        Self::default()
    }

    /// The shared table handle the owner updates each step.
    pub fn handle(&self) -> Rc<RefCell<HashMap<ObjectId, Vec<WorldBoneBox>>>> {
        self.table.clone()
    }

    /// Boxes currently posed for `actor`.
    pub fn boxes_for(&self, actor: ObjectId) -> Option<Vec<WorldBoneBox>> {
        self.table.borrow().get(&actor).cloned()
    }
}

impl HitZones for PosedHitZones {
    fn bone_at(&self, center: [f32; 3], radius: f32, half_height: f32, hit: [f32; 3]) -> String {
        // Called only when `ray_bone` had no box hit (no skeleton, or the ray missed every box):
        // fall back to the collision-cylinder classification.
        CylinderZones.bone_at(center, radius, half_height, hit)
    }

    fn ray_bone(&self, actor: ObjectId, start: [f32; 3], end: [f32; 3]) -> Option<String> {
        let table = self.table.borrow();
        let boxes = table.get(&actor)?;
        let mut best: Option<(f32, &str)> = None;
        for b in boxes {
            if let Some(t) = b.ray_hit(start, end)
                && best.is_none_or(|(bt, _)| t < bt)
            {
                best = Some((t, b.bone.as_str()));
            }
        }
        best.map(|(_, name)| name.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_decode::skeletal::math::{Quat as Q, Transform as T, Vec3 as V};
    use xiii_decode::skeletal::normalize::Bone;

    fn bone(name: &str, parent: Option<usize>, pos: V) -> Bone {
        let local = T::new(Q::IDENTITY, pos);
        Bone {
            name: name.to_owned(),
            parent,
            flags: 0,
            bind_local: local,
            bind_global: local,
        }
    }

    /// Two-bone skeleton: root at the origin, a child 90 UU up. One box around the child tip and
    /// one at the root, both axis-aligned in bone space.
    fn synthetic_mesh() -> HitBoxMesh {
        let skeleton = Skeleton {
            bones: vec![
                bone("X Spine", None, V::ZERO),
                bone("X Head", Some(0), V::new(0.0, 0.0, 90.0)),
            ],
        };
        HitBoxMesh {
            path: "synthetic.Mesh".to_owned(),
            bone_map: Vec::new(),
            skeleton,
            anims: None,
            boxes: vec![
                BoneBoxDef {
                    bone_index: 0,
                    bone: "X Spine".to_owned(),
                    center: [0.0, 0.0, 20.0],
                    half: [20.0, 20.0, 20.0],
                },
                BoneBoxDef {
                    bone_index: 1,
                    bone: "X Head".to_owned(),
                    center: [0.0, 0.0, 0.0],
                    half: [11.0, 11.0, 11.0],
                },
            ],
            dropped_boxes: 0,
            placement: MeshPlacement {
                mesh_origin: [0.0; 3],
                mesh_scale: [1.0; 3],
                rot_origin: [0; 3],
            },
        }
    }

    #[test]
    fn world_boxes_follow_the_bone_pose_and_placement() {
        let mesh = synthetic_mesh();
        // Bind pose at the origin: the head box sits 90 UU up, the spine box 20 UU up.
        let boxes = world_boxes(&mesh, [0.0; 3], [0; 3], None);
        let head = boxes.iter().find(|b| b.bone == "X Head").unwrap();
        let spine = boxes.iter().find(|b| b.bone == "X Spine").unwrap();
        assert!((head.center[2] - 90.0).abs() < 1e-3, "{:?}", head.center);
        assert!((spine.center[2] - 20.0).abs() < 1e-3, "{:?}", spine.center);
        // Actor translation moves both; a +90 UU X translation is one box width to the right.
        let moved = world_boxes(&mesh, [90.0, 0.0, 0.0], [0; 3], None);
        assert!((moved[0].center[0] - 90.0).abs() < 1e-3);
        // Yaw 90 degrees (16384) turns the +X offset onto +Y.
        let yawed = world_boxes(&mesh, [0.0, 90.0, 0.0], [0, 16384, 0], None);
        assert!(
            (yawed[0].center[1] - 90.0).abs() < 1e-3,
            "{:?}",
            yawed[0].center
        );
    }

    #[test]
    fn ray_classifies_head_vs_body_and_misses_outside() {
        let mesh = synthetic_mesh();
        let boxes = world_boxes(&mesh, [0.0; 3], [0; 3], None);
        let head = boxes.iter().find(|b| b.bone == "X Head").unwrap();
        let spine = boxes.iter().find(|b| b.bone == "X Spine").unwrap();
        // Horizontal ray at head height from -X to +X hits the head box.
        assert!(
            head.ray_hit([-200.0, 0.0, 90.0], [200.0, 0.0, 90.0])
                .is_some(),
            "head miss"
        );
        // Same ray at spine height hits only the spine box.
        assert!(
            head.ray_hit([-200.0, 0.0, 20.0], [200.0, 0.0, 20.0])
                .is_none()
        );
        assert!(
            spine
                .ray_hit([-200.0, 0.0, 20.0], [200.0, 0.0, 20.0])
                .is_some()
        );
        // A ray that stops short does not reach the box.
        assert!(
            head.ray_hit([-200.0, 0.0, 90.0], [-150.0, 0.0, 90.0])
                .is_none()
        );
        // Parallel ray outside the slab misses.
        assert!(
            head.ray_hit([-200.0, 50.0, 90.0], [200.0, 50.0, 90.0])
                .is_none()
        );
    }

    #[test]
    fn provider_returns_none_for_unknown_actor_and_nearest_box_for_known() {
        let provider = PosedHitZones::new();
        let table = provider.handle();
        let mesh = synthetic_mesh();
        table
            .borrow_mut()
            .insert(7, world_boxes(&mesh, [0.0; 3], [0; 3], None));
        assert_eq!(
            provider.ray_bone(7, [-200.0, 0.0, 90.0], [200.0, 0.0, 90.0]),
            Some("X Head".to_owned())
        );
        // Unknown actor: no per-bone data, caller falls back to the cylinder.
        assert_eq!(
            provider.ray_bone(8, [-200.0, 0.0, 90.0], [200.0, 0.0, 90.0]),
            None
        );
        // Known actor, ray below every box: `None` (cylinder fallback).
        assert_eq!(
            provider.ray_bone(7, [-200.0, 0.0, -5.0], [200.0, 0.0, -5.0]),
            None
        );
    }

    fn opt_in_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in: the real BaseSoldier6 mesh (`XIIIPersos.MiocheM`) decodes hit boxes whose posed
    /// world positions are consistent with a humanoid: a head box near the top and lower boxes
    /// near the feet, with distinct bones. Prints the measured box table.
    #[test]
    fn opt_in_miochem_hit_boxes_are_humanoid() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut cache = PackageCache::open(&game_dir).expect("open install");
        let mesh = load_mesh(&mut cache, "XIIIPersos.MiocheM").expect("decode MiocheM");
        println!(
            "[hitbox] {}: bones {} boxes {} dropped {} mesh_scale {:?} mesh_origin {:?} rot_origin {:?}",
            mesh.path,
            mesh.skeleton.bones.len(),
            mesh.boxes.len(),
            mesh.dropped_boxes,
            mesh.placement.mesh_scale,
            mesh.placement.mesh_origin,
            mesh.placement.rot_origin,
        );
        assert!(!mesh.boxes.is_empty(), "no hit boxes decoded");
        let boxes = world_boxes(&mesh, [0.0; 3], [0; 3], None);
        for b in &boxes {
            println!(
                "[hitbox]   {:>14} center ({:8.1},{:8.1},{:8.1}) half ({:5.1},{:5.1},{:5.1})",
                b.bone, b.center[0], b.center[1], b.center[2], b.half[0], b.half[1], b.half[2]
            );
        }
        // Recover the mesh-space height of each box (the VM actor origin is the collision
        // centre; the mesh origin offset shifts the box down). With an identity actor rotation
        // and a yaw-only `RotOrigin`, the Z axis is unchanged, so adding the scaled `MeshOrigin`
        // Z recovers the mesh-space position the skeleton pose reports.
        let head = boxes
            .iter()
            .find(|b| b.bone.eq_ignore_ascii_case("X Head"))
            .expect("X Head box");
        let spine = boxes
            .iter()
            .find(|b| b.bone.eq_ignore_ascii_case("X Spine1"))
            .expect("X Spine1 box");
        let off_z = mesh.placement.mesh_origin[2] * mesh.placement.mesh_scale[2];
        let head_mesh_z = head.center[2] + off_z;
        let spine_mesh_z = spine.center[2] + off_z;
        println!(
            "[hitbox] mesh-space: head {head_mesh_z:.1} UU, spine1 {spine_mesh_z:.1} UU (offset {off_z:.1})"
        );
        // A humanoid: the head box is above the spine box and near the top of a ~150 UU mesh.
        assert!(
            head_mesh_z > spine_mesh_z + 20.0,
            "head {head_mesh_z} not above spine1 {spine_mesh_z}"
        );
        assert!(
            (120.0..180.0).contains(&head_mesh_z),
            "head mesh height {head_mesh_z} not near the top of a humanoid"
        );
        // The skeleton's own named bones must agree with the box placement (same pose chain).
        let pose = mesh.skeleton.bind_globals();
        let head_bone = mesh.skeleton.find("X Head").expect("X Head bone");
        let neck_bone = mesh.skeleton.find("X Spine1").expect("X Spine1 bone");
        assert!(
            pose[head_bone].translation.z > pose[neck_bone].translation.z,
            "skeleton bone order disagrees with the box order"
        );
    }
}
