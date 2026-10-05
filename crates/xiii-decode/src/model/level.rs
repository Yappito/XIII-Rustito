//! Actor placements of a level package: everything the world viewer needs to place static
//! meshes and the player start, plus the collision-relevant actor flags.
//!
//! Values are the **map's own tagged properties only**; absent properties are `None` so the
//! importer can fall back to the resolved (inherited) class defaults via `xiii_script`
//! (`Vm::class_layout`) and, only if the class default is absent too, to the engine
//! defaults documented on `Engine.Actor` (Location 0, Rotation 0, DrawScale 1,
//! DrawScale3D 1). Every fallback use is counted by the importer (see
//! `xiii-app/src/viewer/load.rs`), never applied silently.

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
    /// `Location` from the map, if present.
    pub location: Option<[f32; 3]>,
    /// `Rotation` from the map (pitch, yaw, roll), if present.
    pub rotation: Option<[i32; 3]>,
    /// `DrawScale` from the map, if present.
    pub draw_scale: Option<f32>,
    /// `DrawScale3D` from the map, if present.
    pub draw_scale_3d: Option<[f32; 3]>,
    /// `PrePivot` from the map, if present.
    pub pre_pivot: Option<[f32; 3]>,
    /// `StaticMesh` reference.
    pub static_mesh: Option<ObjectRef>,
    /// `DrawType` byte if present.
    pub draw_type: Option<u8>,
    /// `bHidden`.
    pub hidden: bool,
    /// `bCollideActors`, `bBlockActors`, `bBlockPlayers` when present.
    pub collision_flags: [Option<bool>; 3],
    /// `CollisionHeight` from the map, if present (a HALF height for Pawn-like actors).
    pub collision_height: Option<f32>,
    /// `CollisionRadius` from the map, if present.
    pub collision_radius: Option<f32>,
}

/// Effective placement values: map property, else class default, else engine default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EffectivePlacement {
    /// `Location` (never null in the corpus class defaults; zero when absent from both).
    pub location: [f32; 3],
    /// `Rotation` (pitch, yaw, roll).
    pub rotation: [i32; 3],
    /// `DrawScale`.
    pub draw_scale: f32,
    /// `DrawScale3D`.
    pub draw_scale_3d: [f32; 3],
    /// `PrePivot`.
    pub pre_pivot: [f32; 3],
}

impl ActorPlacement {
    /// Combined scale `DrawScale * DrawScale3D` (source axes) of the effective values.
    pub fn scale(&self, e: &EffectivePlacement) -> [f32; 3] {
        e.draw_scale_3d.map(|s| s * e.draw_scale)
    }
}

/// What a value was taken from, for the fallback counters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlacementSource {
    /// The map's tagged property block.
    MapProperty,
    /// The class's inherited default (`Vm::class_layout`).
    ClassDefault,
    /// Neither map nor class default has the property; the documented `Engine.Actor`
    /// default (Location 0, Rotation 0, DrawScale 1, DrawScale3D 1, PrePivot 0).
    EngineDefault,
}

/// Result of scanning a level.
#[derive(Debug, Clone, Default)]
pub struct LevelActors {
    /// Actors with a `StaticMesh` property.
    pub static_mesh_actors: Vec<ActorPlacement>,
    /// `PlayerStart` actors.
    pub player_starts: Vec<ActorPlacement>,
    /// Every placed actor with a decoded `Location` (any class), for alignment studies.
    pub all_located: Vec<ActorPlacement>,
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
        location: p.vector("Location"),
        rotation: p.rotator("Rotation"),
        draw_scale: p.float("DrawScale"),
        draw_scale_3d: match p.get("DrawScale3D").map(|x| &x.value) {
            Some(xiii_package::PropertyValue::Struct(xiii_package::StructValue::Vector(v))) => {
                Some(*v)
            }
            _ => None,
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
        collision_height: p.float("CollisionHeight"),
        collision_radius: p.float("CollisionRadius"),
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
            let placed = placement(package, i, &p);
            out.all_located.push(placed.clone());
            out.player_starts.push(placed);
        } else if p.object("StaticMesh").is_some_and(|r| !r.is_null()) {
            let placed = placement(package, i, &p);
            out.all_located.push(placed.clone());
            out.static_mesh_actors.push(placed);
        } else if p.vector("Location").is_some() {
            out.all_located.push(placement(package, i, &p));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_package::{Builder, parse};

    /// Tagged-property block: entries `(name, kind-code, size-code, payload)`; the payload
    /// is written verbatim, so `value.len()` must match what size code `size_code` says
    /// (0 = 1 B, 1 = 2 B, 2 = 4 B, 3 = 12 B). Vector = kind 11; Rotator = kind 12;
    /// Object = kind 5. Terminated by the `None` name.
    fn block_bytes(b: &mut Builder, entries: &[(&str, u8, u8, Vec<u8>)]) -> Vec<u8> {
        let mut block = Vec::new();
        for (name, kind, size_code, value) in entries {
            let ni = b.name(name);
            block.extend(crate::common::test_package::compact(ni));
            block.push(kind | size_code << 4);
            block.extend_from_slice(value);
        }
        block.extend(crate::common::test_package::compact(0));
        block
    }

    /// Property block only: the synthetic builder's exports have no RF_HAS_STACK flag, so
    /// there is no state frame before the tags.
    fn actor_payload(b: &mut Builder, entries: &[(&str, u8, u8, Vec<u8>)]) -> Vec<u8> {
        block_bytes(b, entries)
    }

    /// (size code 3, 12-byte vector payload).
    fn vec12(x: f32, y: f32, z: f32) -> (u8, Vec<u8>) {
        let mut v = Vec::with_capacity(12);
        v.extend_from_slice(&x.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v.extend_from_slice(&z.to_le_bytes());
        (3, v)
    }

    fn rot12(p: i32, y: i32, r: i32) -> (u8, Vec<u8>) {
        let mut v = Vec::with_capacity(12);
        v.extend_from_slice(&p.to_le_bytes());
        v.extend_from_slice(&y.to_le_bytes());
        v.extend_from_slice(&r.to_le_bytes());
        (3, v)
    }

    /// (size code 0 = 1 byte, compact index payload) for a small object reference.
    fn obj_ref(raw: i32) -> (u8, Vec<u8>) {
        let bytes = crate::common::test_package::compact(raw);
        assert_eq!(
            bytes.len(),
            1,
            "compact index must be one byte for this helper"
        );
        (0, bytes)
    }

    #[test]
    fn absent_properties_are_none_and_present_ones_are_kept() {
        let mut b = Builder::new();
        let (_, v) = vec12(100.0, 200.0, 300.0);
        // A null StaticMesh object property plus a real one below: the actor only counts as a
        // static-mesh actor with a non-null mesh; also add a plain StaticMesh export to
        // reference (ObjectProperty with an Export reference, compact 1 for the first export).
        let payload = actor_payload(&mut b, &[("Location", 11, 3, v)]);
        let mesh_export = b.export("StaticMesh", "M", vec![]);
        let _ = mesh_export;
        b.export("StaticMeshActor", "SMA1", payload);
        let bytes = b.build();
        let p = parse(&bytes);
        let actors = scan_level(&p, &bytes);
        assert_eq!(actors.failures.len(), 0, "{:?}", actors.failures);
        assert_eq!(
            actors.static_mesh_actors.len(),
            0,
            "no StaticMesh property on the actor"
        );
        assert_eq!(actors.all_located.len(), 1);
        let a = &actors.all_located[0];
        assert_eq!(a.location, Some([100.0, 200.0, 300.0]));
        // No Rotation/DrawScale/DrawScale3D/PrePivot properties: all None (class defaults
        // are filled by the caller).
        assert_eq!(a.rotation, None);
        assert_eq!(a.draw_scale, None);
        assert_eq!(a.draw_scale_3d, None);
        assert_eq!(a.pre_pivot, None);
        assert_eq!(a.collision_height, None);
        // The same actor appears in all_located.
        assert_eq!(actors.all_located.len(), 1);
    }

    #[test]
    fn player_start_static_mesh_and_located_non_mesh() {
        let mut b = Builder::new();
        let (ls, lv) = vec12(1.0, 2.0, 3.0);
        let ps_payload = actor_payload(&mut b, &[("Location", 11, ls, lv)]);
        b.export("PlayerStart", "PS1", ps_payload);
        let (_, lv2) = vec12(4.0, 5.0, 6.0);
        let (_, pv) = vec12(0.0, 0.0, -8.0);
        let sma_payload = actor_payload(
            &mut b,
            &[("Location", 11, ls, lv2), ("PrePivot", 11, ls, pv)],
        );
        b.export("StaticMeshActor", "SMA2", sma_payload);
        let (ts, tv) = rot12(16384, 32768, 0);
        let (_, tloc) = vec12(7.0, 8.0, 9.0);
        let t_payload = actor_payload(
            &mut b,
            &[("Location", 11, ls, tloc), ("Rotation", 12, ts, tv)],
        );
        b.export("Trigger", "T1", t_payload);
        let bytes = b.build();
        let p = parse(&bytes);
        let actors = scan_level(&p, &bytes);
        assert_eq!(actors.player_starts.len(), 1);
        // SMA2 has no StaticMesh property: it is only located, so the static-mesh list is 0
        // here and the null-reference test below covers the mesh case.
        assert_eq!(actors.static_mesh_actors.len(), 0);
        // All three exports have a Location: PS1, SMA2, T1.
        assert_eq!(actors.all_located.len(), 3);
        assert_eq!(actors.all_located[1].pre_pivot, Some([0.0, 0.0, -8.0]));
        assert_eq!(actors.player_starts[0].location, Some([1.0, 2.0, 3.0]));
        let t = &actors.all_located[2];
        assert_eq!(t.rotation, Some([16384, 32768, 0]));
    }

    #[test]
    fn null_static_mesh_reference_is_ignored_but_located() {
        let mut b = Builder::new();
        let (ls, lv) = vec12(1.0, 2.0, 3.0);
        let (os, null) = obj_ref(0);
        let payload = actor_payload(
            &mut b,
            &[("Location", 11, ls, lv), ("StaticMesh", 5, os, null)],
        );
        b.export("StaticMeshActor", "SMA3", payload);
        let bytes = b.build();
        let p = parse(&bytes);
        let actors = scan_level(&p, &bytes);
        assert_eq!(actors.static_mesh_actors.len(), 0, "{:?}", actors);
        // Still collected in all_located (it has a Location).
        assert_eq!(actors.all_located.len(), 1);
    }

    #[test]
    fn non_null_static_mesh_reference_sorts_into_static_mesh_actors() {
        let mut b = Builder::new();
        let (ls, lv) = vec12(1.0, 2.0, 3.0);
        let (_, pv) = vec12(0.0, 0.0, -12.0);
        // A 4-byte positive reference: compact 1 = the first export (the mesh below).
        let (os, mesh) = obj_ref(1);
        let mesh_export = b.export("StaticMesh", "M", vec![]);
        let payload = actor_payload(
            &mut b,
            &[
                ("Location", 11, ls, lv),
                ("PrePivot", 11, ls, pv),
                ("StaticMesh", 5, os, mesh),
            ],
        );
        let actor = b.export("StaticMeshActor", "SMA4", payload);
        let bytes = b.build();
        let p = parse(&bytes);
        let actors = scan_level(&p, &bytes);
        assert_eq!(mesh_export, 0);
        assert_eq!(actor, 1);
        assert_eq!(actors.failures.len(), 0, "{:?}", actors.failures);
        assert_eq!(actors.static_mesh_actors.len(), 1);
        assert_eq!(actors.all_located.len(), 1);
        let a = &actors.static_mesh_actors[0];
        assert_eq!(a.location, Some([1.0, 2.0, 3.0]));
        assert_eq!(a.pre_pivot, Some([0.0, 0.0, -12.0]));
        assert!(a.static_mesh.is_some_and(|r| !r.is_null()));
    }
}
