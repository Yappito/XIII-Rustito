//! World-space BSP primitives of the placed **volume brushes** (`Volume`, `PhysicsVolume`,
//! `WaterVolume`, `LadderVolume`, `DetectionVolume`, `Trigger`, ...).
//!
//! Retail `AActor::IsOverlapping` (Engine.dll @0x10352bd0) tests a volume brush with its `Brush`
//! `UModel` primitive, not the actor's collision cylinder (see `xiii_script::brush`). This module
//! decodes each placed volume's `Brush` model read-only and transforms its BSP nodes into world
//! (Unreal) space with the same effective placement the world importer applies, so the VM's
//! overlap test can walk the real geometry. A brush whose model cannot be decoded is reported in
//! [`VolumeBrushes::diagnostics`] and omitted (the VM then keeps the cylinder fallback).

use std::path::Path;
use std::sync::Arc;

use xiii_decode::common::{BevyTransform, Mat3, to_bevy_position};
use xiii_decode::model::{decode_model, level};
use xiii_package::{Limits, ObjectRef};
use xiii_script::brush::{BrushNode, BrushPrimitive};

use crate::physics::bevy_to_unreal_position;
use crate::{
    ClassDefaults, Loaded, PackageCache, ResolvedPlacement, apply_transform_pub,
    placement_transform, zones,
};

/// One placed volume brush with its world-space BSP primitive.
pub struct VolumeBrush {
    /// Map export name (e.g. `DetectionVolume4`).
    pub name: String,
    /// Export object path (e.g. `XIDPawn.DetectionVolume'DetectionVolume4'`).
    pub path: String,
    /// World-space BSP primitive.
    pub primitive: BrushPrimitive,
}

/// Every decoded volume brush of one map plus decode diagnostics.
#[derive(Default)]
pub struct VolumeBrushes {
    /// Decoded brushes.
    pub brushes: Vec<VolumeBrush>,
    /// Non-fatal decode problems (one line each); never silently dropped.
    pub diagnostics: Vec<String>,
    /// Map name.
    pub map: String,
}

/// `(x,y,z) -> (y,z,-x)/90` linear part (the axis permutation of [`to_bevy_position`]).
const SOURCE_TO_BEVY: Mat3 = [
    [0.0, 1.0 / 90.0, 0.0],
    [0.0, 0.0, 1.0 / 90.0],
    [-1.0 / 90.0, 0.0, 0.0],
];

/// `(x,y,z) -> (-z,x,y)*90` linear part (the inverse of [`SOURCE_TO_BEVY`]).
const BEVY_TO_SOURCE: Mat3 = [[0.0, 0.0, -90.0], [90.0, 0.0, 0.0], [0.0, 90.0, 0.0]];

fn mul(a: &Mat3, b: &Mat3) -> Mat3 {
    std::array::from_fn(|i| std::array::from_fn(|j| (0..3).map(|k| a[i][k] * b[k][j]).sum()))
}

fn apply(m: &Mat3, v: [f32; 3]) -> [f32; 3] {
    std::array::from_fn(|i| m[i][0] * v[0] + m[i][1] * v[1] + m[i][2] * v[2])
}

fn transpose(m: &Mat3) -> Mat3 {
    std::array::from_fn(|i| std::array::from_fn(|j| m[j][i]))
}

fn inverse(m: &Mat3) -> Option<Mat3> {
    let det = m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0]);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let cof = [
        [
            m[1][1] * m[2][2] - m[1][2] * m[2][1],
            m[0][2] * m[2][1] - m[0][1] * m[2][2],
            m[0][1] * m[1][2] - m[0][2] * m[1][1],
        ],
        [
            m[1][2] * m[2][0] - m[1][0] * m[2][2],
            m[0][0] * m[2][2] - m[0][2] * m[2][0],
            m[0][2] * m[1][0] - m[0][0] * m[1][2],
        ],
        [
            m[1][0] * m[2][1] - m[1][1] * m[2][0],
            m[0][1] * m[2][0] - m[0][0] * m[2][1],
            m[0][0] * m[1][1] - m[0][1] * m[1][0],
        ],
    ];
    Some(std::array::from_fn(|i| {
        std::array::from_fn(|j| cof[i][j] * inv)
    }))
}

/// The affine `local (Unreal) -> world (Unreal)` linear part of a placement.
fn placement_linear(t: &BevyTransform) -> Mat3 {
    let s: Mat3 = [
        [t.scale[0], 0.0, 0.0],
        [0.0, t.scale[1], 0.0],
        [0.0, 0.0, t.scale[2]],
    ];
    mul(
        &BEVY_TO_SOURCE,
        &mul(&t.rotation, &mul(&s, &SOURCE_TO_BEVY)),
    )
}

/// Decodes every placed volume brush's `Brush` model into a world-space BSP primitive.
pub fn import(game_dir: &Path, map: &str) -> Result<VolumeBrushes, String> {
    let mut cache = PackageCache::open(game_dir)?;
    let map_pkg = cache.map(map)?;
    let actors = level::scan_level(&map_pkg.package, &map_pkg.data);
    let mut defaults = ClassDefaults::open(game_dir)?;
    let mut out = VolumeBrushes {
        map: map.to_owned(),
        ..Default::default()
    };
    for a in &actors.all_located {
        // Only `Volume`-derived actors are volume brushes (`AActor::IsVolumeBrush` requires
        // `Brush != null` and `IsA(AVolume)`).
        let chain = match defaults.class_chain(&a.class) {
            Ok(c) => c,
            Err(e) => {
                out.diagnostics
                    .push(format!("{}: class chain unavailable: {e}", a.path));
                continue;
            }
        };
        if !chain.iter().any(|c| c.eq_ignore_ascii_case("volume")) {
            continue;
        }
        let props = map_pkg
            .package
            .read_object_properties(&map_pkg.data, a.export, &Limits::default())
            .ok();
        let brush = props
            .as_ref()
            .and_then(|pr| xiii_decode::common::Props::new(&map_pkg.package, pr).object("Brush"))
            .filter(|r| !r.is_null());
        let Some(brush) = brush else {
            continue;
        };
        let (eff, _) = match defaults.resolve(&a.class, a) {
            Ok(v) => v,
            Err(e) => {
                out.diagnostics
                    .push(format!("{}: placement resolve failed: {e}", a.path));
                continue;
            }
        };
        match world_primitive(&mut cache, &map_pkg, &eff, brush) {
            Ok(primitive) => {
                let name = a.path.rsplit('.').next().unwrap_or(&a.path).to_owned();
                out.brushes.push(VolumeBrush {
                    name,
                    path: a.path.clone(),
                    primitive,
                });
            }
            Err(e) => out
                .diagnostics
                .push(format!("{}: brush primitive decode failed: {e}", a.path)),
        }
    }
    Ok(out)
}

/// Builds one brush's world-space BSP from its `Brush` -> `Engine.Model`.
pub(crate) fn world_primitive(
    cache: &mut PackageCache,
    map_pkg: &Arc<Loaded>,
    eff: &ResolvedPlacement,
    brush: ObjectRef,
) -> Result<BrushPrimitive, String> {
    let (bp, bx) = cache.resolve(map_pkg, brush)?;
    let model = decode_model(&bp.package, &bp.data, bx).map_err(|e| e.to_string())?;
    let t = placement_transform(eff);
    let linear = placement_linear(&t);
    let normal_matrix = transpose(&inverse(&linear).ok_or("placement matrix is singular")?);
    let root = zones::ZoneMap::new(&model).root();
    let mut nodes = Vec::with_capacity(model.nodes.len());
    for n in &model.nodes {
        let n_local = [n.plane[0], n.plane[1], n.plane[2]];
        let mut n_world = apply(&normal_matrix, n_local);
        let len =
            (n_world[0] * n_world[0] + n_world[1] * n_world[1] + n_world[2] * n_world[2]).sqrt();
        if len > 1e-12 {
            n_world = [n_world[0] / len, n_world[1] / len, n_world[2] / len];
        }
        // A point on the local plane (the closest point to the origin, exact for a non-unit
        // normal); its world image fixes the world plane's distance.
        let nn = n_local[0] * n_local[0] + n_local[1] * n_local[1] + n_local[2] * n_local[2];
        let s = if nn > 1e-12 { n.plane[3] / nn } else { 0.0 };
        let p_local = [n_local[0] * s, n_local[1] * s, n_local[2] * s];
        let p_world = bevy_to_unreal_position(apply_transform_pub(&t, to_bevy_position(p_local)));
        let d = n_world[0] * p_world[0] + n_world[1] * p_world[1] + n_world[2] * p_world[2];
        nodes.push(BrushNode {
            plane: [n_world[0], n_world[1], n_world[2], d],
            csg: n.num_vertices > 0 && n.flags & (0x01 | 0x20) == 0,
            front: n.front,
            back: n.back,
        });
    }
    Ok(BrushPrimitive {
        root,
        root_outside: model.root_outside != 0,
        nodes,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity_transform() -> BevyTransform {
        BevyTransform {
            translation: [0.0; 3],
            rotation: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
            scale: [1.0; 3],
        }
    }

    /// With no rotation and unit scale, the local→world linear part must be the identity, so a
    /// stored plane is unchanged except for the translation the caller adds via the point.
    #[test]
    fn identity_placement_maps_a_normal_to_itself() {
        let l = placement_linear(&identity_transform());
        for n in [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]] {
            let r = apply(&l, n);
            assert!((r[0] - n[0]).abs() < 1e-6, "{r:?}");
            assert!((r[1] - n[1]).abs() < 1e-6, "{r:?}");
            assert!((r[2] - n[2]).abs() < 1e-6, "{r:?}");
        }
    }

    /// Under an arbitrary rotation the transformed normal must stay perpendicular to the world
    /// images of two in-plane directions: the inverse-transpose rule, not a naive rotation.
    #[test]
    fn rotated_plane_normal_stays_perpendicular_to_its_plane() {
        let mut t = identity_transform();
        t.rotation = [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]];
        t.translation = [3.0, -2.0, 7.0];
        let linear = placement_linear(&t);
        let normal_matrix = transpose(&inverse(&linear).unwrap());
        let n_local = [0.5, -0.5, std::f32::consts::FRAC_1_SQRT_2];
        let n_world = apply(&normal_matrix, n_local);
        let world =
            |p: [f32; 3]| bevy_to_unreal_position(apply_transform_pub(&t, to_bevy_position(p)));
        // A point on the plane and two independent in-plane directions.
        let d = 5.0;
        let p0 = [n_local[0] * d, n_local[1] * d, n_local[2] * d];
        let u = [1.0, 1.0, 0.0];
        // An in-plane direction: the cross product `n_local x u` is perpendicular to `n_local`.
        let v = [
            n_local[1] * u[2] - n_local[2] * u[1],
            n_local[2] * u[0] - n_local[0] * u[2],
            n_local[0] * u[1] - n_local[1] * u[0],
        ];
        let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
        let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
        let wp = world(p0);
        for dir in [u, v] {
            let edge = sub(world([p0[0] + dir[0], p0[1] + dir[1], p0[2] + dir[2]]), wp);
            assert!(
                dot(n_world, edge).abs() < 1e-3,
                "normal {n_world:?} not perpendicular to {edge:?}"
            );
        }
    }

    /// The inverse helper rejects a singular (zero-scale) placement instead of dividing by zero.
    #[test]
    fn singular_placement_is_rejected() {
        assert!(inverse(&[[0.0; 3]; 3]).is_none());
        assert!(inverse(&[[1.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 1.0]]).is_none());
        assert!(inverse(&[[1.0, 0.0, 0.0], [0.0, 2.0, 0.0], [0.0, 0.0, 4.0]]).is_some());
    }

    fn opt_in_game_dir() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in: Hual04a's `DetectionVolume4`/`5` are boxes rotated in the XY plane; the old
    /// world-AABB substitute over-fired, so this pins the exact classification. The route walks
    /// from the spawn (outside DV4) into the canyon (inside), and the canyon floor sits below
    /// DV5's class-cylinder top but inside its box.
    #[test]
    fn opt_in_hual04a_volume_brushes_classify_the_route() {
        let Some(game_dir) = opt_in_game_dir() else {
            println!("SKIPPED: set XIII_GOG_DIR to the installation root to run this test");
            return;
        };
        let vb = import(&game_dir, "Hual04a").expect("import Hual04a volume brushes");
        println!(
            "[volume brushes] {} decoded, {} diagnostics",
            vb.brushes.len(),
            vb.diagnostics.len()
        );
        for d in &vb.diagnostics {
            println!("[volume brushes] diagnostic: {d}");
        }
        let find = |name: &str| {
            vb.brushes
                .iter()
                .find(|b| b.name == name)
                .map(|b| &b.primitive)
                .unwrap_or_else(|| panic!("{name} must decode"))
        };
        let dv4 = find("DetectionVolume4");
        let dv5 = find("DetectionVolume5");
        // The spawn box centre (place_spawn) is outside DV4; the first route point enters it.
        assert!(!dv4.contains_point([-10483.919, -1445.0024, 137.67448]));
        assert!(dv4.contains_point([-10200.0, -1500.0, 137.67448]));
        // DV5's world centre (model-local (-246,163,248)) is well inside its brush; the class
        // cylinder top z=158 is below the canyon floor, so the brush is what fires `dernierpoint`.
        assert!(dv5.contains_point([-6387.6, 413.1, 408.0]));
        // A point above the brush top (world z 784) is outside.
        assert!(!dv5.contains_point([-6387.6, 413.1, 900.0]));
    }
}
