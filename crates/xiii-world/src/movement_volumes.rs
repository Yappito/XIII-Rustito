//! Movement-relevant `PhysicsVolume` brushes: water, ladders and generic physics volumes.
//!
//! The host movement simulation (`xiii-app --play`) needs to know, for the player box centre,
//! whether it is inside a water volume, a ladder volume or a plain physics volume and what that
//! volume's movement properties are. UE2 keeps those on the placed `PhysicsVolume`/`WaterVolume`/
//! `LadderVolume` brush actors; this module decodes the placed actors of a map read-only, resolves
//! each brush's world-space bounds and exposes a point query.
//!
//! ## Evidence (measured, GOG corpus)
//!
//! * `Gameplay.WaterVolume` chain `watervolume <- physicsvolume <- volume <- brush <- actor`;
//!   class defaults `bWaterVolume=true`, `FluidFriction=2.4`, `Gravity=(0,0,-950)`,
//!   `Buoyancy=0`, `TerminalVelocity=2500`, `GroundFriction=8`.
//! * `Engine.PhysicsVolume` chain `physicsvolume <- volume <- brush <- actor`; defaults
//!   `bWaterVolume=false`, `FluidFriction=0.3`, `Gravity=(0,0,-950)`.
//! * `Engine.LadderVolume` chain `laddervolume <- physicsvolume <- volume <- brush <- actor`;
//!   defaults `ClimbDir=(0,0,1)`, `bDirectional=false`, `bNoPhysicalLadder=false`.
//! * Banque01 places `WaterVolume0`, `LadderVolume5`, `LadderVolume1`, `PhysicsVolume2`.
//!
//! The brush bounds come from the actor's `Brush` -> `Engine.Model` primitive bounding box,
//! transformed with the same effective placement (Location/Rotation/DrawScale/PrePivot) the world
//! importer applies. A volume whose brush cannot be decoded is reported in
//! [`MovementVolumes::diagnostics`] and stored as a degenerate point volume
//! (`bounds_are_point`), which contains nothing: a decode failure can never silently claim
//! coverage.

use std::path::Path;

use xiii_decode::common::{Props, to_bevy_position};
use xiii_decode::model::{decode_model, level};
use xiii_package::Limits;

use crate::physics::bevy_to_unreal_position;
use crate::{ClassDefaults, PackageCache, apply_transform_pub, placement_transform};

/// Kind of a movement volume.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeKind {
    /// A plain `PhysicsVolume` (no water, no ladder).
    Physics,
    /// A `WaterVolume` (`bWaterVolume`).
    Water,
    /// A `LadderVolume`.
    Ladder,
}

/// One placed movement volume with its world-space bounds and movement properties.
#[derive(Debug, Clone, PartialEq)]
pub struct MovementVolume {
    /// Actor name (map export name, e.g. `WaterVolume0`).
    pub name: String,
    /// Map class path (e.g. `Gameplay.WaterVolume`).
    pub class: String,
    /// Classified kind.
    pub kind: VolumeKind,
    /// `Gravity` (Unreal units/s^2).
    pub gravity: [f32; 3],
    /// `ZoneVelocity` (Unreal units/s).
    pub zone_velocity: [f32; 3],
    /// `FluidFriction`.
    pub fluid_friction: f32,
    /// `Buoyancy`.
    pub buoyancy: f32,
    /// `TerminalVelocity`.
    pub terminal_velocity: f32,
    /// `GroundFriction`.
    pub ground_friction: f32,
    /// `LadderVolume.ClimbDir` (Unreal axes).
    pub climb_dir: [f32; 3],
    /// `bNoPhysicalLadder`.
    pub no_physical_ladder: bool,
    /// Minimum corner of the world-space AABB (Unreal units).
    pub min_uu: [f32; 3],
    /// Maximum corner of the world-space AABB (Unreal units).
    pub max_uu: [f32; 3],
    /// True when the bounds are a degenerate point at the actor origin (brush not decoded).
    pub bounds_are_point: bool,
}

impl MovementVolume {
    /// Size of the AABB (Unreal units).
    pub fn size_uu(&self) -> [f32; 3] {
        [
            self.max_uu[0] - self.min_uu[0],
            self.max_uu[1] - self.min_uu[1],
            self.max_uu[2] - self.min_uu[2],
        ]
    }

    /// True when the Unreal-space point lies inside the AABB. A degenerate point volume
    /// contains nothing (so a brush decode failure never silently claims coverage).
    pub fn contains(&self, point: [f32; 3]) -> bool {
        if self.bounds_are_point {
            return false;
        }
        (0..3).all(|k| point[k] >= self.min_uu[k] && point[k] <= self.max_uu[k])
    }

    /// True when an AABB (`center_uu` +/- `half_uu`, Unreal axes) overlaps this volume. This is
    /// the rule the pawn uses (a thin water slab is touched by the pawn's box, not its centre).
    pub fn overlaps_box(&self, center_uu: [f32; 3], half_uu: [f32; 3]) -> bool {
        if self.bounds_are_point {
            return false;
        }
        (0..3).all(|k| {
            center_uu[k] + half_uu[k] >= self.min_uu[k]
                && center_uu[k] - half_uu[k] <= self.max_uu[k]
        })
    }

    /// Volume of the AABB (used to pick the innermost volume deterministically).
    pub fn volume_uu3(&self) -> f32 {
        let s = self.size_uu();
        (s[0].max(0.0)) * (s[1].max(0.0)) * (s[2].max(0.0))
    }
}

/// Movement volumes of one map plus decode diagnostics.
#[derive(Debug, Clone, Default)]
pub struct MovementVolumes {
    /// Every decoded volume.
    pub volumes: Vec<MovementVolume>,
    /// Non-fatal decode problems (one line each); never silently dropped.
    pub diagnostics: Vec<String>,
    /// Map name the volumes were imported from.
    pub map: String,
}

impl MovementVolumes {
    /// Decodes every placed `PhysicsVolume`/`WaterVolume`/`LadderVolume` of `map`.
    pub fn import(game_dir: &Path, map: &str) -> Result<Self, String> {
        let mut cache = PackageCache::open(game_dir)?;
        let map_pkg = cache.map(map)?;
        let actors = level::scan_level(&map_pkg.package, &map_pkg.data);
        let mut defaults = ClassDefaults::open(game_dir)?;
        let mut out = MovementVolumes {
            map: map.to_owned(),
            ..Default::default()
        };
        for a in &actors.all_located {
            // Only brush volumes that participate in pawn physics. `DetectionVolume` and
            // `BlockingVolume` derive from `Volume` but not `PhysicsVolume`, so they are skipped.
            let chain = match defaults.class_chain(&a.class) {
                Ok(c) => c,
                Err(e) => {
                    out.diagnostics
                        .push(format!("{}: class chain unavailable: {e}", a.path));
                    continue;
                }
            };
            let has = |n: &str| chain.iter().any(|c| c.eq_ignore_ascii_case(n));
            let kind = if has("laddervolume") {
                VolumeKind::Ladder
            } else if has("watervolume") {
                VolumeKind::Water
            } else if has("physicsvolume") {
                VolumeKind::Physics
            } else {
                continue;
            };
            let props = map_pkg
                .package
                .read_object_properties(&map_pkg.data, a.export, &Limits::default())
                .ok();
            let p = props.as_ref().map(|pr| Props::new(&map_pkg.package, pr));
            let get_bool = |name: &str| p.as_ref().and_then(|p| p.bool(name));
            let get_float = |name: &str| p.as_ref().and_then(|p| p.float(name));
            let get_vec = |name: &str| p.as_ref().and_then(|p| p.vector(name));
            let is_water = matches!(kind, VolumeKind::Water)
                || get_bool("bWaterVolume").unwrap_or(false)
                || defaults
                    .bool_default(&a.class, "bWaterVolume")
                    .ok()
                    .flatten()
                    .unwrap_or(false);
            let kind = if is_water { VolumeKind::Water } else { kind };
            let (eff, _) = defaults.resolve(&a.class, a)?;
            let gravity = get_vec("Gravity")
                .or_else(|| defaults.vector_default(&a.class, "Gravity").ok().flatten())
                .unwrap_or([0.0, 0.0, -950.0]);
            let zone_velocity = get_vec("ZoneVelocity")
                .or_else(|| {
                    defaults
                        .vector_default(&a.class, "ZoneVelocity")
                        .ok()
                        .flatten()
                })
                .unwrap_or([0.0; 3]);
            let fluid_friction = get_float("FluidFriction")
                .or_else(|| {
                    defaults
                        .float_default(&a.class, "FluidFriction")
                        .ok()
                        .flatten()
                })
                .unwrap_or(0.0);
            let buoyancy = get_float("Buoyancy")
                .or_else(|| defaults.float_default(&a.class, "Buoyancy").ok().flatten())
                .unwrap_or(0.0);
            let terminal_velocity = get_float("TerminalVelocity")
                .or_else(|| {
                    defaults
                        .float_default(&a.class, "TerminalVelocity")
                        .ok()
                        .flatten()
                })
                .unwrap_or(0.0);
            let ground_friction = get_float("GroundFriction")
                .or_else(|| {
                    defaults
                        .float_default(&a.class, "GroundFriction")
                        .ok()
                        .flatten()
                })
                .unwrap_or(0.0);
            let climb_dir = get_vec("ClimbDir")
                .or_else(|| defaults.vector_default(&a.class, "ClimbDir").ok().flatten())
                .unwrap_or([0.0, 0.0, 1.0]);
            let no_physical_ladder = get_bool("bNoPhysicalLadder")
                .or_else(|| {
                    defaults
                        .bool_default(&a.class, "bNoPhysicalLadder")
                        .ok()
                        .flatten()
                })
                .unwrap_or(false);

            // Brush geometry: the actor's `Brush` -> `Engine.Model` primitive bounding box,
            // transformed with the same effective placement the world importer uses.
            let (min_uu, max_uu, point_fallback) = match p
                .as_ref()
                .and_then(|p| p.object("Brush"))
                .filter(|r| !r.is_null())
            {
                Some(brush) => match cache.resolve(&map_pkg, brush) {
                    Ok((bp, bx)) => match decode_model(&bp.package, &bp.data, bx) {
                        Ok(model) => {
                            let b = model.primitive.bounding_box;
                            let t = placement_transform(&eff);
                            let mut mn = [f32::INFINITY; 3];
                            let mut mx = [f32::NEG_INFINITY; 3];
                            for i in 0..8 {
                                // Brush model bounds are in source (Unreal) local axes; the
                                // transform operates in Bevy space, so convert the corner first
                                // (the same order the static-mesh importer uses).
                                let corner = to_bevy_position([
                                    if i & 1 == 0 { b.min[0] } else { b.max[0] },
                                    if i & 2 == 0 { b.min[1] } else { b.max[1] },
                                    if i & 4 == 0 { b.min[2] } else { b.max[2] },
                                ]);
                                let bevy = apply_transform_pub(&t, corner);
                                let uu = bevy_to_unreal_position(bevy);
                                for k in 0..3 {
                                    mn[k] = mn[k].min(uu[k]);
                                    mx[k] = mx[k].max(uu[k]);
                                }
                            }
                            (mn, mx, false)
                        }
                        Err(e) => {
                            out.diagnostics
                                .push(format!("{}: brush model decode failed: {e}", a.path));
                            (eff.location, eff.location, true)
                        }
                    },
                    Err(e) => {
                        out.diagnostics
                            .push(format!("{}: brush model unresolved: {e}", a.path));
                        (eff.location, eff.location, true)
                    }
                },
                None => {
                    out.diagnostics
                        .push(format!("{}: no Brush property", a.path));
                    (eff.location, eff.location, true)
                }
            };
            let name = a.path.rsplit('.').next().unwrap_or(&a.path).to_owned();
            out.volumes.push(MovementVolume {
                name,
                class: a.class.clone(),
                kind,
                gravity,
                zone_velocity,
                fluid_friction,
                buoyancy,
                terminal_velocity,
                ground_friction,
                climb_dir,
                no_physical_ladder,
                min_uu,
                max_uu,
                bounds_are_point: point_fallback,
            });
        }
        Ok(out)
    }

    /// Every volume containing `point`, innermost (smallest) first.
    pub fn volumes_at(&self, point: [f32; 3]) -> Vec<&MovementVolume> {
        let mut v: Vec<&MovementVolume> = self
            .volumes
            .iter()
            .filter(|vol| vol.contains(point))
            .collect();
        v.sort_by(|a, b| a.volume_uu3().total_cmp(&b.volume_uu3()));
        v
    }

    /// The innermost water volume containing `point`, if any.
    pub fn water_at(&self, point: [f32; 3]) -> Option<&MovementVolume> {
        self.volumes_at(point)
            .into_iter()
            .find(|v| v.kind == VolumeKind::Water)
    }

    /// The innermost ladder volume containing `point`, if any.
    pub fn ladder_at(&self, point: [f32; 3]) -> Option<&MovementVolume> {
        self.volumes_at(point)
            .into_iter()
            .find(|v| v.kind == VolumeKind::Ladder)
    }

    /// The innermost physics volume containing `point`, if any.
    pub fn physics_at(&self, point: [f32; 3]) -> Option<&MovementVolume> {
        self.volumes_at(point).into_iter().next()
    }

    /// Every volume overlapping a box, innermost (smallest) first.
    pub fn volumes_at_box(&self, center_uu: [f32; 3], half_uu: [f32; 3]) -> Vec<&MovementVolume> {
        let mut v: Vec<&MovementVolume> = self
            .volumes
            .iter()
            .filter(|vol| vol.overlaps_box(center_uu, half_uu))
            .collect();
        v.sort_by(|a, b| a.volume_uu3().total_cmp(&b.volume_uu3()));
        v
    }

    /// The innermost water volume overlapping a box, if any.
    pub fn water_box_at(&self, center_uu: [f32; 3], half_uu: [f32; 3]) -> Option<&MovementVolume> {
        self.volumes_at_box(center_uu, half_uu)
            .into_iter()
            .find(|v| v.kind == VolumeKind::Water)
    }

    /// The innermost ladder volume overlapping a box, if any.
    pub fn ladder_box_at(&self, center_uu: [f32; 3], half_uu: [f32; 3]) -> Option<&MovementVolume> {
        self.volumes_at_box(center_uu, half_uu)
            .into_iter()
            .find(|v| v.kind == VolumeKind::Ladder)
    }

    /// One diagnostic line per kind/name for the startup report.
    pub fn summary(&self) -> String {
        let count = |k: VolumeKind| self.volumes.iter().filter(|v| v.kind == k).count();
        let mut names: Vec<String> = self
            .volumes
            .iter()
            .map(|v| {
                format!(
                    "{} [{} min=({:.0},{:.0},{:.0}) size=({:.0},{:.0},{:.0}){}{}]",
                    v.name,
                    match v.kind {
                        VolumeKind::Water => "water",
                        VolumeKind::Ladder => "ladder",
                        VolumeKind::Physics => "physics",
                    },
                    v.min_uu[0],
                    v.min_uu[1],
                    v.min_uu[2],
                    v.size_uu()[0],
                    v.size_uu()[1],
                    v.size_uu()[2],
                    if v.bounds_are_point {
                        " POINT-FALLBACK"
                    } else {
                        ""
                    },
                    if v.kind == VolumeKind::Ladder && v.no_physical_ladder {
                        " no-physical"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        names.sort();
        format!(
            "{} volume(s): {} water, {} ladder, {} physics{}{}",
            self.volumes.len(),
            count(VolumeKind::Water),
            count(VolumeKind::Ladder),
            count(VolumeKind::Physics),
            if names.is_empty() {
                String::new()
            } else {
                format!(": {}", names.join(", "))
            },
            if self.diagnostics.is_empty() {
                String::new()
            } else {
                format!(" ({} diagnostic(s))", self.diagnostics.len())
            }
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn point_outside_a_degenerate_volume_is_not_contained() {
        let mut v = MovementVolume {
            name: "P".into(),
            class: "Test.Volume".into(),
            kind: VolumeKind::Physics,
            gravity: [0.0, 0.0, -950.0],
            zone_velocity: [0.0; 3],
            fluid_friction: 0.0,
            buoyancy: 0.0,
            terminal_velocity: 0.0,
            ground_friction: 0.0,
            climb_dir: [0.0, 0.0, 1.0],
            no_physical_ladder: false,
            min_uu: [1.0, 2.0, 3.0],
            max_uu: [1.0, 2.0, 3.0],
            bounds_are_point: true,
        };
        assert!(!v.contains([1.0, 2.0, 3.0]));
        v.bounds_are_point = false;
        assert!(!v.contains([2.0, 2.0, 3.0]));
        // Inclusive on both faces.
        v.min_uu = [0.0; 3];
        v.max_uu = [1.0; 3];
        assert!(v.contains([0.0, 0.0, 0.0]));
        assert!(v.contains([1.0, 1.0, 1.0]));
        assert!((v.volume_uu3() - 1.0).abs() < 1e-6);
    }

    /// Opt-in: Banque01 places one `WaterVolume`, two `LadderVolume`s and one `PhysicsVolume`.
    /// The brush AABB must be non-degenerate and contain the actor origin (a brush is built around
    /// its Location), and the water/ladder point query must find the volume at its own centre.
    #[test]
    fn opt_in_banque01_volumes_decode_and_query() {
        let Some(game_dir) = opt_in_game_dir() else {
            println!("SKIPPED: set XIII_GOG_DIR to the installation root to run this test");
            return;
        };
        let v = MovementVolumes::import(&game_dir, "Banque01").expect("import Banque01 volumes");
        println!("[volumes test] {}", v.summary());
        for l in &v.diagnostics {
            println!("[volumes test] diagnostic: {l}");
        }
        let water: Vec<_> = v
            .volumes
            .iter()
            .filter(|x| x.kind == VolumeKind::Water)
            .collect();
        let ladder: Vec<_> = v
            .volumes
            .iter()
            .filter(|x| x.kind == VolumeKind::Ladder)
            .collect();
        assert!(!water.is_empty(), "Banque01 must have a water volume");
        assert!(!ladder.is_empty(), "Banque01 must have a ladder volume");
        for vol in &v.volumes {
            let s = vol.size_uu();
            println!(
                "[volumes test] {} {:?} min=({:.1},{:.1},{:.1}) size=({:.1},{:.1},{:.1}) friction={} gravity={:?} point={}",
                vol.name,
                vol.kind,
                vol.min_uu[0],
                vol.min_uu[1],
                vol.min_uu[2],
                s[0],
                s[1],
                s[2],
                vol.fluid_friction,
                vol.gravity,
                vol.bounds_are_point
            );
            // Water and ladder brushes must decode to real geometry; a plain `PhysicsVolume`
            // may legitimately be a `DefaultPhysicsVolume` with no `Brush` (point fallback).
            if vol.kind != VolumeKind::Physics {
                assert!(
                    !vol.bounds_are_point,
                    "{}: brush bounds did not decode: {:?}",
                    vol.name, v.diagnostics
                );
                assert!(
                    s[0] > 1.0 && s[1] > 1.0 && s[2] > 1.0,
                    "{}: degenerate AABB {s:?}",
                    vol.name
                );
            }
        }
        // The centre of each volume is inside it and found by the kind query.
        for vol in &water {
            let c = [
                (vol.min_uu[0] + vol.max_uu[0]) * 0.5,
                (vol.min_uu[1] + vol.max_uu[1]) * 0.5,
                (vol.min_uu[2] + vol.max_uu[2]) * 0.5,
            ];
            assert_eq!(v.water_at(c).map(|x| &x.name), Some(&vol.name));
        }
        for vol in &ladder {
            let c = [
                (vol.min_uu[0] + vol.max_uu[0]) * 0.5,
                (vol.min_uu[1] + vol.max_uu[1]) * 0.5,
                (vol.min_uu[2] + vol.max_uu[2]) * 0.5,
            ];
            assert_eq!(v.ladder_at(c).map(|x| &x.name), Some(&vol.name));
        }
    }
}
