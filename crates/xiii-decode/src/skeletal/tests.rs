//! Synthetic-fixture tests (no proprietary data).

use super::error::SkelErrorKind;
use super::math::{Quat, Transform, Vec3};
use super::normalize::{AnimSet, Skeleton, SkinnedMesh, evaluate_pose, local_rotation};
use super::validate;
use super::{decode_mesh_animation_bytes, decode_skeletal_mesh_bytes};
use xiii_package::{Limits, Package};

// ---------------------------------------------------------------- byte builders

fn compact(v: i32) -> Vec<u8> {
    let neg = v < 0;
    let mut m = v.unsigned_abs();
    let mut out = Vec::new();
    let mut b = (m & 0x3f) as u8;
    if neg {
        b |= 0x80;
    }
    m >>= 6;
    if m != 0 {
        b |= 0x40;
    }
    out.push(b);
    while m != 0 {
        let mut b = (m & 0x7f) as u8;
        m >>= 7;
        if m != 0 {
            b |= 0x80;
        }
        out.push(b);
    }
    out
}

#[derive(Default)]
struct W(Vec<u8>);

impl W {
    fn ci(&mut self, v: i32) -> &mut Self {
        self.0.extend(compact(v));
        self
    }
    fn u8(&mut self, v: u8) -> &mut Self {
        self.0.push(v);
        self
    }
    fn u16(&mut self, v: u16) -> &mut Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    fn i16(&mut self, v: i16) -> &mut Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    fn i32(&mut self, v: i32) -> &mut Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    fn u32(&mut self, v: u32) -> &mut Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    fn f32(&mut self, v: f32) -> &mut Self {
        self.0.extend(v.to_le_bytes());
        self
    }
    fn v3(&mut self, v: [f32; 3]) -> &mut Self {
        v.iter().for_each(|&x| {
            self.f32(x);
        });
        self
    }
    fn q(&mut self, v: [f32; 4]) -> &mut Self {
        v.iter().for_each(|&x| {
            self.f32(x);
        });
        self
    }
}

/// Minimal valid version-100 package with the given names and no imports/exports.
fn package(names: &[&str]) -> (Vec<u8>, Package) {
    let mut name_bytes = Vec::new();
    for n in names {
        name_bytes.extend(compact(n.len() as i32 + 1));
        name_bytes.extend(n.as_bytes());
        name_bytes.push(0);
        name_bytes.extend(0u32.to_le_bytes());
    }
    let names_off = 64u32;
    let end = names_off + name_bytes.len() as u32;
    let mut w = W::default();
    w.u32(0x9e2a_83c1).u16(100).u16(58).u32(0);
    w.i32(names.len() as i32).u32(names_off); // names
    w.i32(0).u32(end); // exports
    w.i32(0).u32(end); // imports
    w.0.extend([0u8; 16]);
    w.i32(1).i32(0).i32(names.len() as i32);
    assert_eq!(w.0.len(), 64);
    w.0.extend(name_bytes);
    let p = Package::parse(&w.0, &Limits::default()).expect("synthetic package parses");
    (w.0, p)
}

const NAMES: &[&str] = &["None", "Root", "Child", "Seq", "PlayFootStep", "Grp"];

fn bbox(w: &mut W, min: f32, max: f32) {
    w.v3([min; 3]).v3([max; 3]).u8(1);
}

/// Two-bone skeleton (root at origin, child 10 units up +Z, child stored with a conjugated
/// 90-degree rotation about Z), a triangle with one point per bone mix, one hit box.
fn mesh_payload() -> Vec<u8> {
    let mut w = W::default();
    bbox(&mut w, -1.0, 1.0); // primitive box
    w.q([0.0, 0.0, 0.0, 1.0]); // sphere
    w.u8(1).u8(1).u8(0).u8(0).f32(1.0); // XIII primitive extension
    w.i32(1).i32(3); // version, vertex count
    w.ci(0); // packed verts
    w.ci(0); // legacy tris
    w.ci(1).ci(0); // textures: [null]
    w.v3([1.0; 3]).v3([0.0; 3]).i32(0).i32(16384).i32(0);
    w.ci(3).u16(0).u16(1).u16(2); // legacy u16
    w.ci(1).u16(0); // face level
    w.ci(1).u16(0).u16(1).u16(2).u16(0); // faces
    w.ci(3).u16(0).u16(1).u16(2); // collapse
    w.ci(3); // wedges
    for i in 0..3u16 {
        w.u16(i).f32(f32::from(i)).f32(0.5);
    }
    w.ci(1).u32(0).i32(0); // materials
    w.f32(1.0).f32(0.0).f32(1.0).i32(10).f32(0.3).f32(0.0);
    // points
    w.ci(3)
        .v3([0.0, 0.0, 0.0])
        .v3([1.0, 0.0, 10.0])
        .v3([0.0, 1.0, 10.0]);
    // bones
    let s = std::f32::consts::FRAC_1_SQRT_2;
    w.ci(2);
    w.ci(1)
        .u32(0)
        .q([0.0, 0.0, 0.0, 1.0])
        .v3([0.0; 3])
        .f32(0.0)
        .v3([0.0; 3])
        .i32(0);
    w.ci(2)
        .u32(0)
        .q([0.0, 0.0, -s, s])
        .v3([0.0, 0.0, 10.0])
        .f32(0.0)
        .v3([0.0; 3])
        .i32(0);
    w.ci(0); // animation: null
    w.i32(2); // depth
    // weight groups: point 0 one influence; points 1, 2 two influences
    w.ci(2);
    w.ci(1).u16(0).i32(0);
    w.ci(2).u16(1).u16(2).i32(1);
    w.ci(5);
    w.u16(65535).u16(0);
    w.u16(16384).u16(0).u16(49151).u16(1);
    w.u16(0).u16(0).u16(65535).u16(1);
    w.ci(0).ci(0).ci(0); // attach
    w.ci(1);
    for v in [0u16, 0, 0, 2, 3, 0, 0, 0, 1] {
        w.u16(v);
    }
    w.ci(0); // second legacy sections
    w.ci(1);
    bbox(&mut w, -11.0, 11.0);
    w.u16(1);
    w.0
}

/// Animation with the same two bones and one 3-frame sequence.
fn anim_payload() -> Vec<u8> {
    let mut w = W::default();
    w.i32(0);
    w.ci(2).ci(1).u32(0).i32(0).ci(2).u32(0).i32(0);
    w.ci(1); // moves
    w.v3([0.0; 3]).f32(3.0).i32(0).u32(0);
    w.ci(2).i32(0).i32(1);
    // tracks: root 1 quat 1 pos; child 2 quats 2 pos
    w.ci(2);
    w.u16(0).u16(0).u16(1).u16(1);
    w.u16(1).u16(1).u16(2).u16(2);
    w.ci(3);
    w.i16(0).i16(0).i16(0).i16(32767);
    w.i16(0).i16(0).i16(0).i16(32767);
    w.i16(0).i16(0).i16(-23170).i16(23170); // stored -90 about Z (child: conjugated)
    w.ci(3)
        .v3([0.0; 3])
        .v3([0.0, 0.0, 10.0])
        .v3([0.0, 0.0, 12.0]);
    w.ci(3).f32(0.0).f32(0.0).f32(2.0);
    w.ci(1); // sequences
    w.ci(3).ci(1).ci(5).i32(0).i32(3);
    w.ci(1).f32(0.5).ci(4);
    w.f32(30.0);
    w.0
}

// ---------------------------------------------------------------- math

fn close(a: Vec3, b: Vec3) -> bool {
    (a - b).length() < 1e-4
}

#[test]
fn quaternion_rotation_and_composition() {
    let z90 = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), std::f32::consts::FRAC_PI_2);
    assert!(close(
        z90.rotate(Vec3::new(1.0, 0.0, 0.0)),
        Vec3::new(0.0, 1.0, 0.0)
    ));
    let x90 = Quat::from_axis_angle(Vec3::new(1.0, 0.0, 0.0), std::f32::consts::FRAC_PI_2);
    let v = Vec3::new(0.3, -2.0, 5.0);
    assert!(close((z90 * x90).rotate(v), z90.rotate(x90.rotate(v))));
    assert!(close(z90.conjugate().rotate(z90.rotate(v)), v));
    // slerp endpoints, midpoint and shortest arc
    assert!(close(z90.slerp(x90, 0.0).rotate(v), z90.rotate(v)));
    assert!(close(z90.slerp(x90, 1.0).rotate(v), x90.rotate(v)));
    let neg = Quat::new(-z90.x, -z90.y, -z90.z, -z90.w);
    let mid = Quat::IDENTITY.slerp(neg, 0.5);
    let z45 = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), std::f32::consts::FRAC_PI_4);
    assert!(close(mid.rotate(v), z45.rotate(v)));
}

#[test]
fn transform_compose_and_inverse() {
    let a = Transform::new(
        Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), 0.7),
        Vec3::new(1.0, 2.0, 3.0),
    );
    let b = Transform::new(
        Quat::from_axis_angle(Vec3::new(1.0, 1.0, 0.0), -1.1),
        Vec3::new(-4.0, 0.5, 9.0),
    );
    let p = Vec3::new(0.25, 7.0, -3.0);
    assert!(close(
        a.mul(&b).transform_point(p),
        a.transform_point(b.transform_point(p))
    ));
    assert!(close(a.inverse().transform_point(a.transform_point(p)), p));
    assert!(close(a.mul(&a.inverse()).transform_point(p), p));
}

#[test]
fn storage_convention_conjugates_non_root() {
    let s = std::f32::consts::FRAC_1_SQRT_2;
    let stored = [0.0, 0.0, -s, s];
    assert_eq!(
        local_rotation(stored, true),
        Quat::from_array(stored).normalized()
    );
    let child = local_rotation(stored, false);
    assert!(close(
        child.rotate(Vec3::new(1.0, 0.0, 0.0)),
        Vec3::new(0.0, 1.0, 0.0)
    ));
}

// ---------------------------------------------------------------- mesh serializer

#[test]
fn mesh_fixture_decodes_and_normalizes() {
    let (_, pkg) = package(NAMES);
    let raw = decode_skeletal_mesh_bytes(&pkg, &mesh_payload(), 0).expect("fixture decodes");
    assert_eq!(raw.lod_version, 1);
    assert_eq!(raw.points.len(), 3);
    assert_eq!(raw.ref_skeleton[1].name, "Child");
    assert_eq!(raw.ref_skeleton[1].parent, 0);
    assert_eq!(raw.bone_boxes.len(), 1);
    assert_eq!(raw.bone_boxes[0].bone, 1);
    assert!(raw.textures[0].is_null());

    let skel = Skeleton::from_mesh(&raw).unwrap();
    assert_eq!(skel.root_count(), 1);
    assert_eq!(skel.max_depth(), 2);
    let child = skel.bones[1].bind_global;
    assert!(close(child.translation, Vec3::new(0.0, 0.0, 10.0)));
    assert!(close(
        child.transform_vector(Vec3::new(1.0, 0.0, 0.0)),
        Vec3::new(0.0, 1.0, 0.0)
    ));

    let mesh = SkinnedMesh::from_raw(&raw, &skel, Some(&pkg)).unwrap();
    assert_eq!(mesh.positions.len(), 3);
    assert_eq!(mesh.indices, vec![0, 1, 2]);
    assert_eq!(mesh.sections.len(), 1);
    assert_eq!(mesh.influence_stats.max_influences, 2);
    assert_eq!(mesh.influence_stats.points_without_influences, 0);
    for w in &mesh.weights {
        assert!((w.iter().sum::<f32>() - 1.0).abs() < 1e-5);
    }
    let rep = validate::mesh_report(&mesh, &skel);
    assert!(rep.bind_skin_max_error < 1e-4);
    assert_eq!(rep.degenerate_triangles, 0);
}

#[test]
fn mesh_truncation_fails_cleanly_at_every_length() {
    let (_, pkg) = package(NAMES);
    let full = mesh_payload();
    for len in 0..full.len() {
        let e = decode_skeletal_mesh_bytes(&pkg, &full[..len], 0).expect_err("truncated");
        assert!(e.offset.is_some_and(|o| o <= len as u64), "{e}");
    }
}

#[test]
fn mesh_trailing_bytes_are_reported() {
    let (_, pkg) = package(NAMES);
    let mut p = mesh_payload();
    let end = p.len();
    p.extend([0xAA, 0xBB]);
    let e = decode_skeletal_mesh_bytes(&pkg, &p, 0).unwrap_err();
    assert_eq!(e.kind, SkelErrorKind::TrailingBytes { remaining: 2 });
    assert_eq!(e.offset, Some(end as u64));
}

#[test]
fn mesh_unsupported_variants_and_bad_counts() {
    let (_, pkg) = package(NAMES);
    // version 4 (UT2004-style) is rejected at the version field (offset 25 + 16 + 8)
    let mut p = mesh_payload();
    p[49..53].copy_from_slice(&4i32.to_le_bytes());
    let e = decode_skeletal_mesh_bytes(&pkg, &p, 0).unwrap_err();
    assert!(matches!(e.kind, SkelErrorKind::Unsupported(_)), "{e}");
    assert_eq!(e.offset, Some(49));
    // packed-verts count far beyond the data
    let mut p = mesh_payload();
    let mut huge = p[..57].to_vec();
    huge.extend(compact(1_000_000));
    huge.extend(&p[58..]);
    let e = decode_skeletal_mesh_bytes(&pkg, &huge, 0).unwrap_err();
    assert!(
        matches!(e.kind, SkelErrorKind::ArrayExceedsData { .. }),
        "{e}"
    );
    // negative count
    p[57] = 0x81;
    let e = decode_skeletal_mesh_bytes(&pkg, &p, 0).unwrap_err();
    assert_eq!(e.kind, SkelErrorKind::NegativeCount(-1));
}

#[test]
fn mesh_rejects_second_legacy_sections_and_bad_parents() {
    let (_, pkg) = package(NAMES);
    let p = mesh_payload();
    // The second legacy array count sits right before the final box array (1 + 25 + 2 bytes).
    let at = p.len() - 29;
    assert_eq!(p[at], 0);
    let mut q = p.clone();
    q[at] = 1;
    let e = decode_skeletal_mesh_bytes(&pkg, &q, 0).unwrap_err();
    assert!(matches!(e.kind, SkelErrorKind::Unsupported(_)), "{e}");

    let mut raw = decode_skeletal_mesh_bytes(&pkg, &p, 0).unwrap();
    raw.ref_skeleton[0].parent = 1;
    assert!(Skeleton::from_mesh(&raw).is_err());
    let mut raw = decode_skeletal_mesh_bytes(&pkg, &p, 0).unwrap();
    raw.weight_indices[1].start = 2;
    let skel = Skeleton::from_mesh(&raw).unwrap();
    assert!(SkinnedMesh::from_raw(&raw, &skel, None).is_err());
}

#[test]
fn name_index_out_of_range_is_an_error() {
    let (_, pkg) = package(&NAMES[..2]); // "Child" (index 2) missing
    let e = decode_skeletal_mesh_bytes(&pkg, &mesh_payload(), 0).unwrap_err();
    assert_eq!(e.kind, SkelErrorKind::NameOutOfRange(2));
}

// ---------------------------------------------------------------- animation serializer

#[test]
fn anim_fixture_decodes_and_samples() {
    let (_, pkg) = package(NAMES);
    let raw = decode_mesh_animation_bytes(&pkg, &anim_payload(), 0).expect("decodes");
    assert_eq!(raw.ref_bones.len(), 2);
    assert_eq!(raw.moves[0].quats.len(), 3);
    assert_eq!(raw.sequences[0].name, "Seq");
    assert_eq!(raw.sequences[0].groups, vec!["Grp".to_owned()]);
    assert_eq!(raw.sequences[0].notifies[0].function, "PlayFootStep");

    let set = AnimSet::from_raw(&raw).unwrap();
    let clip = set.clip("seq").unwrap();
    assert_eq!(clip.num_frames, 3);
    assert!((clip.duration - 0.1).abs() < 1e-6);
    let rep = validate::clip_report(&set, clip);
    assert!(rep.track_count_ok && rep.times_monotone && rep.times_in_range);
    assert!(rep.starts_at_zero && rep.position_counts_ok);
    assert_eq!(rep.notifies_out_of_range, 0);

    let child = &clip.tracks[1];
    // frame 1: halfway between identity and +90 about Z, position 11
    let t = child.sample(1.0, clip.track_time, false);
    assert!(close(t.translation, Vec3::new(0.0, 0.0, 11.0)));
    let z45 = Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), std::f32::consts::FRAC_PI_4);
    let v = Vec3::new(1.0, 0.0, 0.0);
    assert!(close(t.rotation.rotate(v), z45.rotate(v)));
    // after the last key: hold, or wrap toward key 0 when looping
    assert!(close(
        child.sample(2.5, 3.0, false).translation,
        Vec3::new(0.0, 0.0, 12.0)
    ));
    assert!(close(
        child.sample(2.5, 3.0, true).translation,
        Vec3::new(0.0, 0.0, 11.0)
    ));

    // pose a mesh with the clip
    let mraw = decode_skeletal_mesh_bytes(&pkg, &mesh_payload(), 0).unwrap();
    let skel = Skeleton::from_mesh(&mraw).unwrap();
    let mesh = SkinnedMesh::from_raw(&mraw, &skel, None).unwrap();
    let map = set.bone_map(&skel);
    assert_eq!(map, vec![Some(0), Some(1)]);
    // Point 2 (0, 1, 10) is fully on the child, whose bind rotation is +90 about Z.
    // Frame 0: child unrotated at z = 10, so the point turns -90 about the joint.
    let pose = evaluate_pose(&skel, clip, &map, 0.0, false);
    let skinned = mesh.skin(&skel, &pose);
    assert!(
        close(skinned[2], Vec3::new(1.0, 0.0, 10.0)),
        "{:?}",
        skinned[2]
    );
    assert!(close(skinned[0], Vec3::ZERO));
    // Frame 2: bind rotation again, joint lifted to z = 12.
    let pose = evaluate_pose(&skel, clip, &map, 2.0, false);
    assert!(close(pose[1].translation, Vec3::new(0.0, 0.0, 12.0)));
    let skinned = mesh.skin(&skel, &pose);
    assert!(
        close(skinned[2], Vec3::new(0.0, 1.0, 12.0)),
        "{:?}",
        skinned[2]
    );
}

#[test]
fn anim_truncation_and_trailing() {
    let (_, pkg) = package(NAMES);
    let full = anim_payload();
    for len in 0..full.len() {
        assert!(decode_mesh_animation_bytes(&pkg, &full[..len], 0).is_err());
    }
    let mut p = full.clone();
    p.push(0);
    let e = decode_mesh_animation_bytes(&pkg, &p, 0).unwrap_err();
    assert_eq!(e.kind, SkelErrorKind::TrailingBytes { remaining: 1 });
}

#[test]
fn anim_track_ranges_and_time_counts_are_checked() {
    let (_, pkg) = package(NAMES);
    let base = anim_payload();
    // Tracks start after: version 4, bones 1 + 2 * 9, moves 1, chunk fixed 24, bone indices 9.
    let tracks = 4 + 19 + 1 + 24 + 9 + 1;
    // child track: quat_count 2 -> 3 (runs past the 3 quats)
    let mut p = base.clone();
    p[tracks + 8 + 4..tracks + 8 + 6].copy_from_slice(&3u16.to_le_bytes());
    let e = decode_mesh_animation_bytes(&pkg, &p, 0).unwrap_err();
    assert!(matches!(e.kind, SkelErrorKind::Invalid(_)), "{e}");
    // three quats but only two key times
    let times_at = tracks + 16 + 1 + 24 + 1 + 36;
    assert_eq!(base[times_at], 3);
    let mut p = base[..times_at].to_vec();
    p.extend(compact(2));
    p.extend(&base[times_at + 1 + 4..]);
    let e = decode_mesh_animation_bytes(&pkg, &p, 0).unwrap_err();
    assert!(matches!(e.kind, SkelErrorKind::Invalid(_)), "{e}");
}

// ---------------------------------------------------------------- opt-in corpus test

#[test]
fn local_corpus_skeletal() {
    let Some(dir) = std::env::var_os("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR to run the skeletal corpus test");
        return;
    };
    let pc = std::path::Path::new(&dir).join("system").join("PC");
    let mut meshes = 0;
    let mut anims = 0;
    for file in [
        "xiiipersos.u",
        "xiiipersosG.u",
        "xiiiarmes.u",
        "xiiideco.u",
        "xiiivehicule.u",
    ] {
        let data = std::fs::read(pc.join(file)).expect("read package");
        let pkg = Package::parse(&data, &Limits::default()).expect("parse package");
        for i in 0..pkg.exports().len() {
            match pkg.export_class_path(i) {
                Some(super::SKELETAL_MESH_CLASS) => {
                    let raw = super::decode_skeletal_mesh(&pkg, &data, i)
                        .unwrap_or_else(|e| panic!("{file}: {e}"));
                    let skel = Skeleton::from_mesh(&raw).unwrap();
                    let mesh = SkinnedMesh::from_raw(&raw, &skel, Some(&pkg)).unwrap();
                    let rep = validate::mesh_report(&mesh, &skel);
                    assert!(rep.weight_sum_min > 0.999 && rep.weight_sum_max < 1.001);
                    assert!(rep.bind_skin_max_error < 1e-2, "{file} export {i}");
                    assert_eq!(skel.root_count(), 1);
                    meshes += 1;
                }
                Some(super::MESH_ANIMATION_CLASS) => {
                    let raw = super::decode_mesh_animation(&pkg, &data, i)
                        .unwrap_or_else(|e| panic!("{file}: {e}"));
                    let set = AnimSet::from_raw(&raw).unwrap();
                    for c in &set.clips {
                        let r = validate::clip_report(&set, c);
                        assert!(r.track_count_ok && r.times_monotone && r.times_in_range);
                        assert!(r.position_counts_ok && r.quats_finite);
                    }
                    anims += 1;
                }
                _ => {}
            }
        }
    }
    assert_eq!((meshes, anims), (129, 96));
}
