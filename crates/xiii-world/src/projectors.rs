//! `Projector` / `ShadowProjector` decoding: the tagged properties of a projective-texture
//! actor (a projected light cone or a blob shadow), from the map's own instances and from a
//! running `xiii-script` VM.
//!
//! ## What a UE2 projector is
//!
//! `Engine.Projector` is an `Actor` that projects a texture into the world with a cone/frustum
//! (`FOV`, `MaxTraceDistance`, `DrawScale`). Its `ProjTexture` is the image, and the
//! `bProject*`/`bClip*` flags bound the trace. `Engine.ShadowProjector` is the blob-shadow
//! subclass used by XIII: a pawn with `bActorShadows` (default true on `XIIIPawn`) spawns one in
//! `SetInitialState` at its own `Location` (`xiii.XIIIPawn.SetInitialState`, disassembled:
//! `self.Shadow = Spawn(Engine.ShadowProjector, self, 'None', self.Location)`), tuning it from
//! the map's `MapInfo` (`MaxTraceDistance`, `ShadowIntensity`, `ShadowMaxDist`,
//! `ShadowTransDist`). `ShadowProjector.PostBeginPlay` builds its `ProjTexture` at runtime from a
//! `FinalBlend(FrameBufferBlending=FB_Darken)` wrapping an `engine.ShadowBitmapMaterial`.
//!
//! ## Evidence
//!
//! * Class defaults (measured, `xiii-tool script defaults`):
//!   `Engine.Projector`: `FOV 0`, `MaxTraceDistance 1000`, `bProjectBSP/Terrain/StaticMesh/Actor
//!   true`, `bClipBSP false`, `bFade false`, `bProjectOnAlpha/Unlit/ParallelBSP false`,
//!   `DrawScale 1`, `Texture Proj_Icon`.
//!   `Engine.ShadowProjector`: `FOV 0`, `MaxTraceDistance 250`, `bProjectActor false`,
//!   `bClipBSP true`, `bFade true`, `bProjectOnAlpha true`, `bProjectOnParallelBSP true`,
//!   `AttachPriority 20`, `ShadowIntensity 196`, `ShadowScale 1`, `ShadowMaxDist 1500`,
//!   `ShadowTransDist 1000`.
//! * Map instance (measured, the only tagged map projector in the 35-map campaign):
//!   `SPADS01 Projector0 Engine.Projector ProjTexture=XIIIspads.spaproj_alpha FOV=20
//!   MaxTraceDistance=2500 bProjectBSP=false bProjectTerrain=false bClipBSP=true
//!   bProjectOnUnlit=true DrawScale=0.5 @(-912.4,178.2,-1792.0) rot(-16372,0,0)`.
//! * Runtime instance (measured, `--play` Plage01): the soldier spawns `ShadowProjector` actors;
//!   `PresentationEvent::ProjectorDetach` fires for them. Their runtime property block is read
//!   through the VM (effective map+class+script values).
//!
//! ## Ownership
//!
//! This module is Bevy-free and owns the decode. Placement/orientation into Bevy space and the
//! presentation (a decal or a projected quad) live in `xiii-app`; the app asks this module for a
//! [`ProjectorDef`] and a [`ProjectorPose`].

use xiii_decode::common::{Props, to_bevy_direction, to_bevy_position};
use xiii_decode::model::level::LevelActors;
use xiii_package::{Limits, ObjectRef, Package, PropertyValue};
use xiii_script::{ObjRef, ObjectId, Value, Vm};

/// How the projected texture blends with the receiver, derived from the material chain's
/// `FrameBufferBlending`/`OutputBlending` byte. Only the values XIII uses are named; any other
/// byte is [`ProjectorBlend::Other`] (reported, not silently treated as opaque).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectorBlend {
    /// `FB_Overwrite`/`OB_Normal` (0/1): replace.
    Overwrite,
    /// `FB_Darken` (5): multiply — the shadow case.
    Darken,
    /// `FB_AlphaBlend`/`FB_Translucent`/`OB_AlphaBlend` (2/4/5): alpha blend.
    Alpha,
    /// `FB_Modulate`/`OB_Modulate` (1/2): multiply.
    Modulate,
    /// A byte this table does not name.
    Other(u8),
}

impl ProjectorBlend {
    /// Maps a raw `FrameBufferBlending` byte (the `FinalBlend` enum order is measured in
    /// `xiii-world::materials` and pinned by `opt_in_material_enum_names_match_blend_mapping`).
    pub fn from_frame_buffer_byte(b: u8) -> Self {
        match b {
            0 => Self::Overwrite,
            1 => Self::Modulate,
            2 | 4 => Self::Alpha,
            5 => Self::Darken,
            other => Self::Other(other),
        }
    }

    /// Maps a raw `OutputBlending` byte from a `Shader`.
    pub fn from_output_blending_byte(b: u8) -> Self {
        match b {
            0 => Self::Overwrite,
            2 => Self::Modulate,
            5 => Self::Alpha,
            other => Self::Other(other),
        }
    }
}

/// Decoded projector parameters. Every field is the *effective* value: map property, else the
/// class default (from the running VM or the documented engine default), else absent.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectorDef {
    /// Class path (`Engine.Projector`, `Engine.ShadowProjector`, ...).
    pub class_path: String,
    /// `ProjTexture` object path (`Package.Object`), `None` when null. A `ShadowProjector`
    /// builds this at runtime, so it is often `None` on the class default.
    pub texture_path: Option<String>,
    /// Raw `ProjTexture` map reference (for the importer to resolve into a material).
    pub texture_object: Option<ObjectRef>,
    /// Index into [`crate::WorldScene::materials`] for the resolved `ProjTexture` material,
    /// filled by the importer. `None` when the projector has no texture (the runtime
    /// `ShadowProjector` case, whose texture is built at `PostBeginPlay`).
    pub material_index: Option<usize>,
    /// `FOV` in degrees; UE2 treats 0 as "use the default 45-degree cone" (hypothesis; the
    /// class default is 0 and the only map instance tags 20).
    pub fov: i32,
    /// `MaxTraceDistance` in Unreal units.
    pub max_trace_distance: i32,
    /// `DrawScale`.
    pub draw_scale: f32,
    /// `bProjectBSP`.
    pub b_project_bsp: bool,
    /// `bProjectStaticMesh`.
    pub b_project_static_mesh: bool,
    /// `bProjectActor`.
    pub b_project_actor: bool,
    /// `bProjectTerrain`.
    pub b_project_terrain: bool,
    /// `bClipBSP`.
    pub b_clip_bsp: bool,
    /// `bFade`.
    pub b_fade: bool,
    /// `bProjectOnAlpha`.
    pub b_project_on_alpha: bool,
    /// `bProjectOnUnlit`.
    pub b_project_on_unlit: bool,
    /// `bProjectOnParallelBSP`.
    pub b_project_on_parallel_bsp: bool,
    /// Blend mode of the projected material.
    pub blend: ProjectorBlend,
    /// `ShadowProjector.ShadowIntensity` (byte, 0..255), when the class has it.
    pub shadow_intensity: Option<i32>,
    /// `ShadowProjector.ShadowScale`.
    pub shadow_scale: Option<f32>,
    /// `ShadowProjector.ShadowMaxDist` (UU).
    pub shadow_max_dist: Option<f32>,
    /// `ShadowProjector.ShadowTransDist` (UU).
    pub shadow_trans_dist: Option<f32>,
}

impl ProjectorDef {
    /// The documented `Engine.Projector` class default (used when no map/VM value is available;
    /// every use is a fallback, not a silent zero). Sources: the class defaults printed by
    /// `xiii-tool script defaults engine.u Engine.Projector`.
    pub fn engine_projector_default() -> Self {
        Self {
            class_path: "Engine.Projector".to_owned(),
            texture_path: None,
            texture_object: None,
            material_index: None,
            fov: 0,
            max_trace_distance: 1000,
            draw_scale: 1.0,
            b_project_bsp: true,
            b_project_static_mesh: true,
            b_project_actor: true,
            b_project_terrain: true,
            b_clip_bsp: false,
            b_fade: false,
            b_project_on_alpha: false,
            b_project_on_unlit: false,
            b_project_on_parallel_bsp: false,
            blend: ProjectorBlend::Alpha,
            shadow_intensity: None,
            shadow_scale: None,
            shadow_max_dist: None,
            shadow_trans_dist: None,
        }
    }

    /// The documented `Engine.ShadowProjector` class default.
    pub fn engine_shadow_default() -> Self {
        Self {
            class_path: "Engine.ShadowProjector".to_owned(),
            texture_path: None,
            texture_object: None,
            material_index: None,
            fov: 0,
            max_trace_distance: 250,
            draw_scale: 1.0,
            b_project_bsp: true,
            b_project_static_mesh: true,
            b_project_actor: false,
            b_project_terrain: true,
            b_clip_bsp: true,
            b_fade: true,
            b_project_on_alpha: true,
            b_project_on_unlit: false,
            b_project_on_parallel_bsp: true,
            blend: ProjectorBlend::Darken,
            shadow_intensity: Some(196),
            shadow_scale: Some(1.0),
            shadow_max_dist: Some(1500.0),
            shadow_trans_dist: Some(1000.0),
        }
    }

    /// True when `class_path` is (a subclass of) `ShadowProjector`.
    pub fn is_shadow_class(class_path: &str) -> bool {
        class_path
            .rsplit('.')
            .next()
            .is_some_and(|s| s.eq_ignore_ascii_case("ShadowProjector"))
    }

    /// The class default for a projector class path, based on the class name.
    pub fn class_default(class_path: &str) -> Self {
        if Self::is_shadow_class(class_path) {
            let mut d = Self::engine_shadow_default();
            d.class_path = class_path.to_owned();
            d
        } else {
            let mut d = Self::engine_projector_default();
            d.class_path = class_path.to_owned();
            d
        }
    }

    /// The FOV actually used for a cone: `FOV` when non-zero, else the UE2 default of 45
    /// degrees. **Hypothesis** (the class default is 0 and the engine's 45 is from UT2003/UE2
    /// documentation, not a decoded XIII value).
    pub fn effective_fov_degrees(&self) -> f32 {
        if self.fov != 0 { self.fov as f32 } else { 45.0 }
    }

    /// Frustum depth along the projector axis in metres: `DrawScale` scales the UE2 default
    /// projector extent. **Hypothesis**: the engine draws a fixed-size box scaled by
    /// `DrawScale`; the exact authoring size is not decoded, so this uses the actor's
    /// `MaxTraceDistance` as the hard cap.
    pub fn max_distance_m(&self) -> f32 {
        self.max_trace_distance as f32 / xiii_decode::common::UNREAL_UNITS_PER_METER
    }
}

/// One placed projector in Bevy space, ready for a renderer.
#[derive(Debug, Clone, PartialEq)]
pub struct ProjectorPose {
    /// Decoded parameters.
    pub def: ProjectorDef,
    /// Source object path (map actor or VM actor display name).
    pub name: String,
    /// Bevy-space position (metres).
    pub position: [f32; 3],
    /// Bevy-space forward direction (the projector axis; from the actor rotation, source +X).
    pub forward: [f32; 3],
    /// Bevy-space up (source +Z).
    pub up: [f32; 3],
}

/// Display path of a script object reference (mirrors `Vm::ref_path`, which is private): a
/// static global resolves to `Package.Object`, an instance to its display name, and an external
/// reference to its interned path. Empty when it cannot be named.
fn obj_path(vm: &Vm<'_>, r: &ObjRef) -> String {
    match r {
        ObjRef::Static(g) => vm.set().path(*g),
        ObjRef::Instance(i) => vm
            .objects
            .get(*i as usize)
            .map(|o| o.name.clone())
            .unwrap_or_default(),
        ObjRef::External(id) => vm.external_path(&ObjRef::External(*id)).unwrap_or_default(),
    }
}

/// Reads a `Color`/texture object reference from a map property block.
fn object_ref(package: &Package, p: &Props, name: &str) -> Option<(ObjectRef, String)> {
    match p.get(name)?.value {
        PropertyValue::Object(r) if !r.is_null() => {
            let path = package.object_path(r).map(str::to_owned)?;
            Some((r, path))
        }
        _ => None,
    }
}

/// `byte`/`int` property coerced for the int fields the projector declares as `Int`.
fn as_int(p: &Props, name: &str) -> Option<i32> {
    match p.get(name)?.value {
        PropertyValue::Int(v) => Some(v),
        PropertyValue::Byte(v) => Some(i32::from(v)),
        _ => None,
    }
}

/// Reads a projector's tagged map properties on top of its engine/class defaults.
pub fn decode_map_projector(
    package: &Package,
    data: &[u8],
    class_path: &str,
    actor: ObjectRef,
) -> ProjectorDef {
    let mut def = ProjectorDef::class_default(class_path);
    let ObjectRef::Export(e) = actor else {
        return def;
    };
    let Ok(props) = package.read_object_properties(data, e as usize, &Limits::default()) else {
        return def;
    };
    let p = Props::new(package, &props);
    if let Some((r, t)) = object_ref(package, &p, "ProjTexture") {
        def.texture_path = Some(t);
        def.texture_object = Some(r);
    }
    if let Some(v) = as_int(&p, "FOV") {
        def.fov = v;
    }
    if let Some(v) = as_int(&p, "MaxTraceDistance") {
        def.max_trace_distance = v;
    }
    if let Some(v) = p.float("DrawScale") {
        def.draw_scale = v;
    }
    for (name, slot) in [
        ("bProjectBSP", &mut def.b_project_bsp),
        ("bProjectStaticMesh", &mut def.b_project_static_mesh),
        ("bProjectActor", &mut def.b_project_actor),
        ("bProjectTerrain", &mut def.b_project_terrain),
        ("bClipBSP", &mut def.b_clip_bsp),
        ("bFade", &mut def.b_fade),
        ("bProjectOnAlpha", &mut def.b_project_on_alpha),
        ("bProjectOnUnlit", &mut def.b_project_on_unlit),
        ("bProjectOnParallelBSP", &mut def.b_project_on_parallel_bsp),
    ] {
        if let Some(v) = p.bool(name) {
            *slot = v;
        }
    }
    if let Some(v) = p.byte("ShadowIntensity") {
        def.shadow_intensity = Some(i32::from(v));
    }
    if let Some(v) = p.float("ShadowScale") {
        def.shadow_scale = Some(v);
    }
    if let Some(v) = p.float("ShadowMaxDist") {
        def.shadow_max_dist = Some(v);
    }
    if let Some(v) = p.float("ShadowTransDist") {
        def.shadow_trans_dist = Some(v);
    }
    def
}

/// Every projector actor placed in a map (class name ends in `Projector`), with its map
/// properties applied. `actors` is the importer's own level scan so the two cannot drift.
pub fn map_projectors(package: &Package, data: &[u8], actors: &LevelActors) -> Vec<ProjectorPose> {
    let mut out = Vec::new();
    for a in &actors.all_located {
        let short = a.class.rsplit('.').next().unwrap_or("");
        if !short.eq_ignore_ascii_case("Projector")
            && !short.eq_ignore_ascii_case("ShadowProjector")
            && !short.ends_with("Projector")
        {
            continue;
        }
        let Some(loc) = a.location else { continue };
        let rot = a.rotation.unwrap_or([0; 3]);
        let def = decode_map_projector(package, data, &a.class, ObjectRef::Export(a.export as u32));
        out.push(ProjectorPose {
            def,
            name: a.path.clone(),
            position: to_bevy_position(loc),
            forward: to_bevy_direction(source_axis(rot, 0)),
            up: to_bevy_direction(source_axis(rot, 2)),
        });
    }
    out
}

/// The source-space axis of a rotator: 0 = forward (+X), 1 = right (+Y), 2 = up (+Z). Uses
/// UE2's rotator application (pitch about +Y, yaw about +Z, roll about +X); the forward axis is
/// enough for the projector cone, and `up` is the second basis vector.
fn source_axis(rot: [i32; 3], axis: usize) -> [f32; 3] {
    let tau = std::f32::consts::TAU / 65536.0;
    let (p, y, r) = (
        rot[0] as f32 * tau,
        rot[1] as f32 * tau,
        rot[2] as f32 * tau,
    );
    // UE2 rotation matrix rows (X, Y, Z) as used by `actor_to_bevy_pre_pivot`.
    let (sp, cp) = p.sin_cos();
    let (sy, cy) = y.sin_cos();
    let (sr, cr) = r.sin_cos();
    let rows = [
        [cp * cy, cp * sy, -sp],
        [sr * sp * cy - cr * sy, sr * sp * sy + cr * cy, sr * cp],
        [cr * sp * cy + sr * sy, cr * sp * sy - sr * cy, cr * cp],
    ];
    // Axis 0 = forward = row 0; axis 2 = up = row 2.
    rows[axis]
}

/// Reads a VM actor's effective projector properties into a [`ProjectorDef`]. `None` when `id`
/// is not a live projector actor (class name does not end in `Projector`).
///
/// The VM stores the effective values (map property applied over the inherited class default by
/// the loader), so this is the authoritative runtime decode for `--play`. A struct field the VM
/// did not decode is `Value::Unsupported`; those are reported through `None`/default rather than
/// guessed.
pub fn projector_def_from_vm(vm: &Vm<'_>, id: ObjectId) -> Option<ProjectorDef> {
    let o = vm.objects.get(id as usize)?;
    if o.deleted {
        return None;
    }
    let class_path = vm.set().path(o.class);
    let short = class_path.rsplit('.').next().unwrap_or("");
    if !short.ends_with("Projector") {
        return None;
    }
    let mut def = ProjectorDef::class_default(&class_path);
    let int_prop = |name: &str| match vm.get_property(id, name) {
        Some(Value::Int(v)) => Some(*v),
        Some(Value::Byte(v)) => Some(i32::from(*v)),
        _ => None,
    };
    let float_prop = |name: &str| match vm.get_property(id, name) {
        Some(Value::Float(v)) => Some(*v),
        _ => None,
    };
    let bool_prop = |name: &str| match vm.get_property(id, name) {
        Some(Value::Bool(v)) => Some(*v),
        _ => None,
    };
    if let Some(Value::Object(Some(r))) = vm.get_property(id, "ProjTexture") {
        let path = obj_path(vm, r);
        if !path.is_empty() {
            def.texture_path = Some(path);
        }
    }
    if let Some(v) = int_prop("FOV") {
        def.fov = v;
    }
    if let Some(v) = int_prop("MaxTraceDistance") {
        def.max_trace_distance = v;
    }
    if let Some(v) = float_prop("DrawScale") {
        def.draw_scale = v;
    }
    for (name, slot) in [
        ("bProjectBSP", &mut def.b_project_bsp),
        ("bProjectStaticMesh", &mut def.b_project_static_mesh),
        ("bProjectActor", &mut def.b_project_actor),
        ("bProjectTerrain", &mut def.b_project_terrain),
        ("bClipBSP", &mut def.b_clip_bsp),
        ("bFade", &mut def.b_fade),
        ("bProjectOnAlpha", &mut def.b_project_on_alpha),
        ("bProjectOnUnlit", &mut def.b_project_on_unlit),
        ("bProjectOnParallelBSP", &mut def.b_project_on_parallel_bsp),
    ] {
        if let Some(v) = bool_prop(name) {
            *slot = v;
        }
    }
    if let Some(v) = int_prop("ShadowIntensity") {
        def.shadow_intensity = Some(v);
    }
    if let Some(v) = float_prop("ShadowScale") {
        def.shadow_scale = Some(v);
    }
    if let Some(v) = float_prop("ShadowMaxDist") {
        def.shadow_max_dist = Some(v);
    }
    if let Some(v) = float_prop("ShadowTransDist") {
        def.shadow_trans_dist = Some(v);
    }
    def.class_path = class_path;
    Some(def)
}

/// Bevy-space pose of a VM projector actor. `None` when it has no `Location`.
pub fn projector_pose_from_vm(vm: &Vm<'_>, id: ObjectId) -> Option<ProjectorPose> {
    let def = projector_def_from_vm(vm, id)?;
    let loc = vm.location_prop(id)?;
    let rot = vm.rotation_prop(id).unwrap_or([0; 3]);
    Some(ProjectorPose {
        def,
        name: vm.objects[id as usize].name.clone(),
        position: to_bevy_position(loc),
        forward: to_bevy_direction(source_axis(rot, 0)),
        up: to_bevy_direction(source_axis(rot, 2)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn class_defaults_match_measured_corpus() {
        let p = ProjectorDef::engine_projector_default();
        assert_eq!(p.max_trace_distance, 1000);
        assert!(p.b_project_actor && p.b_project_bsp && p.b_project_static_mesh);
        assert!(!p.b_clip_bsp && !p.b_fade);
        let s = ProjectorDef::engine_shadow_default();
        assert_eq!(s.max_trace_distance, 250);
        assert!(!s.b_project_actor && s.b_clip_bsp && s.b_fade);
        assert_eq!(s.blend, ProjectorBlend::Darken);
    }

    #[test]
    fn blend_byte_mapping_covers_the_corpus_values() {
        assert_eq!(
            ProjectorBlend::from_frame_buffer_byte(5),
            ProjectorBlend::Darken
        );
        assert_eq!(
            ProjectorBlend::from_frame_buffer_byte(0),
            ProjectorBlend::Overwrite
        );
        assert_eq!(
            ProjectorBlend::from_frame_buffer_byte(3),
            ProjectorBlend::Other(3)
        );
    }

    #[test]
    fn fov_zero_falls_back_to_45() {
        let mut d = ProjectorDef::engine_projector_default();
        assert_eq!(d.effective_fov_degrees(), 45.0);
        d.fov = 20;
        assert_eq!(d.effective_fov_degrees(), 20.0);
    }

    #[test]
    fn source_axis_identity_is_forward_x_up_z() {
        assert_eq!(source_axis([0; 3], 0), [1.0, 0.0, 0.0]);
        assert_eq!(source_axis([0; 3], 2), [0.0, 0.0, 1.0]);
        // Yaw 16384 = quarter turn: forward becomes +Y.
        let f = source_axis([0, 16384, 0], 0);
        assert!((f[0]).abs() < 1e-4 && (f[1] - 1.0).abs() < 1e-4, "{f:?}");
    }

    #[test]
    fn shadow_class_detection_is_case_insensitive() {
        assert!(ProjectorDef::is_shadow_class("Engine.ShadowProjector"));
        assert!(ProjectorDef::is_shadow_class("engine.shadowprojector"));
        assert!(!ProjectorDef::is_shadow_class("Engine.Projector"));
    }
}
