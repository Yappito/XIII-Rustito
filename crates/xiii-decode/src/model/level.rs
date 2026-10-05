//! Actor placements of a level package: everything the world viewer needs to place static
//! meshes and the player start, plus the collision-relevant actor flags.
//!
//! Values come from the tagged properties of each actor export. Missing properties fall back
//! to the engine defaults for `Actor` (Location 0, Rotation 0, DrawScale 1, DrawScale3D 1);
//! class-specific default overrides are **not** applied because class defaults inside
//! UClass payloads are not located yet (see xiii-package README). That is a known source of
//! error for subclasses that change DrawScale/DrawType defaults.

use xiii_package::{Limits, ObjectRef, Package};

use crate::common::{DecodeError, Props};

/// One placed actor that references a static mesh or is otherwise relevant to the world.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorPlacement {
    /// Export index in the map.
    pub export: usize,
    /// Class path.
    pub class: String,
    /// Object path.
    pub path: String,
    /// `Location`.
    pub location: [f32; 3],
    /// `Rotation` (pitch, yaw, roll).
    pub rotation: [i32; 3],
    /// `DrawScale`.
    pub draw_scale: f32,
    /// `DrawScale3D`.
    pub draw_scale_3d: [f32; 3],
    /// `PrePivot` (not applied by the viewer).
    pub pre_pivot: Option<[f32; 3]>,
    /// `StaticMesh` reference.
    pub static_mesh: Option<ObjectRef>,
    /// `DrawType` byte if present.
    pub draw_type: Option<u8>,
    /// `bHidden`.
    pub hidden: bool,
    /// `bCollideActors`, `bBlockActors`, `bBlockPlayers` when present.
    pub collision_flags: [Option<bool>; 3],
}

impl ActorPlacement {
    /// Combined scale `DrawScale * DrawScale3D` (source axes).
    pub fn scale(&self) -> [f32; 3] {
        self.draw_scale_3d.map(|s| s * self.draw_scale)
    }
}

/// Result of scanning a level.
#[derive(Debug, Clone, Default)]
pub struct LevelActors {
    /// Actors with a `StaticMesh` property.
    pub static_mesh_actors: Vec<ActorPlacement>,
    /// `PlayerStart` actors.
    pub player_starts: Vec<ActorPlacement>,
    /// Exports whose property block failed to decode.
    pub failures: Vec<(usize, DecodeError)>,
}

fn placement(package: &Package, export: usize, p: &Props<'_>) -> ActorPlacement {
    ActorPlacement {
        export,
        class: package.export_class_path(export).unwrap_or("?").to_owned(),
        path: package
            .object_path(ObjectRef::Export(export as u32))
            .unwrap_or("?")
            .to_owned(),
        location: p.vector("Location").unwrap_or([0.0; 3]),
        rotation: p.rotator("Rotation").unwrap_or([0; 3]),
        draw_scale: p.float("DrawScale").unwrap_or(1.0),
        draw_scale_3d: match p.get("DrawScale3D").map(|x| &x.value) {
            Some(xiii_package::PropertyValue::Struct(xiii_package::StructValue::Vector(v))) => *v,
            _ => [1.0; 3],
        },
        pre_pivot: p.vector("PrePivot"),
        static_mesh: p.object("StaticMesh").filter(|r| !r.is_null()),
        draw_type: p.byte("DrawType"),
        hidden: p.bool("bHidden").unwrap_or(false),
        collision_flags: [
            p.bool("bCollideActors"),
            p.bool("bBlockActors"),
            p.bool("bBlockPlayers"),
        ],
    }
}

/// Scans all exports of a level package for static-mesh actors and player starts.
pub fn scan_level(package: &Package, data: &[u8]) -> LevelActors {
    let mut out = LevelActors::default();
    for i in 0..package.exports().len() {
        if package.exports()[i].serial_size == 0 {
            continue;
        }
        let class = package.export_class_path(i).unwrap_or("");
        // Skip classes whose payload is not an actor property block.
        if matches!(
            class,
            "Core.Class"
                | "Engine.Model"
                | "Engine.Polys"
                | "Engine.Texture"
                | "Engine.TerrainSector"
        ) {
            continue;
        }
        let props = match package.read_object_properties(data, i, &Limits::default()) {
            Ok(p) => p,
            Err(e) => {
                out.failures
                    .push((i, DecodeError::from(e).in_export(package, i)));
                continue;
            }
        };
        let p = Props::new(package, &props);
        if class.eq_ignore_ascii_case("Engine.PlayerStart") {
            out.player_starts.push(placement(package, i, &p));
        } else if p.object("StaticMesh").is_some_and(|r| !r.is_null()) {
            out.static_mesh_actors.push(placement(package, i, &p));
        }
    }
    out
}
