//! UE2 particle emitters (`Engine.Emitter` actors and their `Engine.ParticleEmitter`
//! subobjects) placed in maps, decoded from the tagged-property reader and simulated on the CPU.
//!
//! XIII's emitter system is the **Unreal 2 / pre-UT2004** one: the [`Engine.Emitter`] actor owns
//! an `Emitters` dynamic array of `ParticleEmitter` objects (`SpriteEmitter`, `MeshEmitter`,
//! `BeamEmitter`, `SparkEmitter`, ...), each serialized as its own map export. The class and
//! property layout was verified against the installed `system/engine.u` with
//! `xiii-tool script classes/defaults`: `ParticleEmitter` extends `Object`, `SpriteEmitter` /
//! `MeshEmitter` / `BeamEmitter` / `SparkEmitter` extend `ParticleEmitter`, and `Emitter`
//! extends `Actor` (measured, GOG corpus).
//!
//! Semantics of the properties and enums follow the public UT2003 emitter reference
//! (`beyondunrealwiki.github.io` pages `particleemitter`, `particleemitter/structs`,
//! `particleemitter/enums`, and `spriteemitter`), which documents the same class/property names
//! as the installed `engine.u`. Where the retail XIII renderer cannot be observed, a rule is
//! labelled a **hypothesis** in the code and in `local/reports/item5f-particles.md`.
//!
//! This module is Bevy-free: it produces [`ParticleSystem`] descriptions (with resolved texture
//! indices into [`crate::WorldScene::textures`] and optional mesh geometry) and a deterministic
//! [`ParticleSystemSim`] the renderer steps.
//!
//! Nothing is dropped silently: every emitter actor/subobject class and every property in use is
//! counted (`particle.*`), and properties the simulator does not model are listed as
//! `particle.unsupported_property (<name>)`.

use std::collections::BTreeMap;
use std::sync::Arc;

use xiii_decode::common::{BevyTransform, Props, to_bevy_direction, to_bevy_position};
use xiii_decode::model::level::ActorPlacement;
use xiii_decode::static_mesh::decode_static_mesh;
use xiii_package::{Cursor, Limits, ObjectRef, Package, PropertyValue, Span, StructValue};
use xiii_script::Value;
use xiii_script::vm::ClassLayout;

use crate::{ClassDefaults, Importer, Loaded, placement_transform};

/// A decoded tagged-property value, decoupled from the package so the decoder can be unit-tested
/// with synthetic property blocks (no proprietary data).
#[derive(Debug, Clone, PartialEq)]
pub enum ParticleValue {
    /// `float`.
    Float(f32),
    /// `int`.
    Int(i32),
    /// `byte` / enum.
    Byte(u8),
    /// `bool`.
    Bool(bool),
    /// `vector`.
    Vector([f32; 3]),
    /// `range` (min, max).
    Range([f32; 2]),
    /// `rangevector` (X, Y, Z ranges).
    RangeVector([[f32; 2]; 3]),
    /// `plane` (X, Y, Z, W).
    Plane([f32; 4]),
    /// `color` (stored order; see [`EmitterDesc::color_scale`]).
    Color([u8; 4]),
    /// `array<ParticleColorScale>`: `(relative time, color)`.
    ColorScale(Vec<(f32, [u8; 4])>),
    /// `array<ParticleTimeScale>`: `(relative time, relative size)`.
    SizeScale(Vec<(f32, f32)>),
    /// Object reference.
    Object(ObjectRef),
    /// `name`.
    Name(String),
    /// `string`.
    Str(String),
}

/// Sprite reference geometry of a `MeshEmitter` (Bevy local space: metres, right-handed).
#[derive(Debug, Clone, PartialEq)]
pub struct ParticleMesh {
    /// Positions.
    pub positions: Vec<[f32; 3]>,
    /// Normals.
    pub normals: Vec<[f32; 3]>,
    /// UVs.
    pub uvs: Vec<[f32; 2]>,
    /// Triangle list.
    pub indices: Vec<u32>,
    /// Resolved base texture index into [`crate::WorldScene::textures`], when available.
    pub texture: Option<usize>,
    /// Draw both sides.
    pub two_sided: bool,
}

/// What a sub-emitter draws.
#[derive(Debug, Clone, PartialEq)]
pub enum EmitterShape {
    /// Camera-facing sprite (the default for `SpriteEmitter`).
    Sprite,
    /// Static-mesh particles (`MeshEmitter`).
    Mesh(Box<ParticleMesh>),
    /// A class whose drawing is recognized but not modelled (Beam/Spark/...).
    Unsupported(String),
}

/// One decoded `ParticleEmitter` subobject.
#[derive(Debug, Clone)]
pub struct EmitterDesc {
    /// Short class name (`SpriteEmitter`, `MeshEmitter`, ...).
    pub class: String,
    /// Object path.
    pub name: String,
    /// `MaxParticles`.
    pub max_particles: usize,
    /// `LifetimeRange`.
    pub lifetime: [f32; 2],
    /// `InitialDelayRange`.
    pub initial_delay: [f32; 2],
    /// `InitialTimeRange` (initial particle age).
    pub initial_time: [f32; 2],
    /// `StartLocationOffset`.
    pub start_location_offset: [f32; 3],
    /// `StartLocationRange`.
    pub start_location_range: [[f32; 2]; 3],
    /// `StartLocationShape` (`0` Box, `1` Sphere, ...).
    pub start_location_shape: u8,
    /// `SphereRadiusRange`.
    pub sphere_radius: [f32; 2],
    /// `StartVelocityRange`.
    pub start_velocity: [[f32; 2]; 3],
    /// `Acceleration`.
    pub acceleration: [f32; 3],
    /// `VelocityLossRange`.
    pub velocity_loss: [[f32; 2]; 3],
    /// `MaxAbsVelocity`.
    pub max_abs_velocity: [f32; 3],
    /// `UseColorScale`.
    pub use_color_scale: bool,
    /// `ColorScale`: `(relative time in 0..=1, RGB(A))`. The alpha byte is stored but the wiki
    /// states it is unused by most draw styles, so the renderer uses the fade factors for alpha.
    pub color_scale: Vec<(f32, [u8; 4])>,
    /// `ColorScaleRepeats`.
    pub color_scale_repeats: f32,
    /// `FadeIn`.
    pub fade_in: bool,
    /// `FadeInEndTime` (absolute seconds).
    pub fade_in_end_time: f32,
    /// `FadeInFactor` (X,Y,Z = RGB, W = alpha).
    pub fade_in_factor: [f32; 4],
    /// `FadeOut`.
    pub fade_out: bool,
    /// `FadeOutStartTime` (absolute seconds).
    pub fade_out_start_time: f32,
    /// `FadeOutFactor` (X,Y,Z = RGB, W = alpha).
    pub fade_out_factor: [f32; 4],
    /// `StartSizeRange`.
    pub start_size: [[f32; 2]; 3],
    /// `UseSizeScale`.
    pub use_size_scale: bool,
    /// `UseRegularSizeScale`.
    pub use_regular_size_scale: bool,
    /// `UniformSize`.
    pub uniform_size: bool,
    /// `SizeScale`: `(relative time in 0..=1, relative size)`.
    pub size_scale: Vec<(f32, f32)>,
    /// `SizeScaleRepeats`.
    pub size_scale_repeats: f32,
    /// `SpinParticles`.
    pub spin_particles: bool,
    /// `StartSpinRange` (pitch, yaw, roll).
    pub start_spin: [[f32; 2]; 3],
    /// `SpinsPerSecondRange` (pitch, yaw, roll).
    pub spins_per_second: [[f32; 2]; 3],
    /// `DrawStyle` (`EParticleDrawStyle`).
    pub draw_style: u8,
    /// Resolved sprite texture index, after [`import_particles`].
    pub texture: Option<usize>,
    /// Raw `Texture` reference (resolved by the importer).
    pub texture_ref: Option<ObjectRef>,
    /// Raw `StaticMesh` reference of a `MeshEmitter` (resolved by the importer).
    pub mesh_ref: Option<ObjectRef>,
    /// `TextureUSubdivisions`.
    pub subdivisions_u: u32,
    /// `TextureVSubdivisions`.
    pub subdivisions_v: u32,
    /// `SubdivisionStart`.
    pub subdivision_start: i32,
    /// `SubdivisionEnd`.
    pub subdivision_end: i32,
    /// `BlendBetweenSubdivisions`.
    pub blend_between_subdivisions: bool,
    /// `UseRandomSubdivision`.
    pub use_random_subdivision: bool,
    /// `RespawnDeadParticles`.
    pub respawn_dead_particles: bool,
    /// `AutomaticInitialSpawning`.
    pub automatic_initial_spawning: bool,
    /// `ParticlesPerSecond`.
    pub particles_per_second: f32,
    /// `InitialParticlesPerSecond`.
    pub initial_particles_per_second: f32,
    /// `Disabled`.
    pub disabled: bool,
    /// `EParticleCoordinateSystem` (`0` Independent, `1` Relative, `2` Absolute).
    pub coordinate_system: u8,
    /// `SecondsBeforeInactive`.
    pub seconds_before_inactive: f32,
    /// `AutoDestroy` (sub-emitter local).
    pub auto_destroy: bool,
    /// `AutoReset`.
    pub auto_reset: bool,
    /// Sprite/mesh/unsupported drawing.
    pub shape: EmitterShape,
    /// Properties present on this subobject that the simulator does not model.
    pub unsupported: Vec<String>,
}

impl EmitterDesc {
    /// Average lifetime, used to derive the automatic spawn rate.
    pub fn average_lifetime(&self) -> f32 {
        ((self.lifetime[0] + self.lifetime[1]) * 0.5).max(1.0 / 512.0)
    }
}

/// One map-placed `Emitter` actor and its sub-emitters.
#[derive(Debug, Clone)]
pub struct ParticleSystem {
    /// Actor object path.
    pub path: String,
    /// Effective placement (Location/Rotation/DrawScale/PrePivot), Bevy space.
    pub transform: BevyTransform,
    /// `AutoDestroy`.
    pub auto_destroy: bool,
    /// `AutoReset`.
    pub auto_reset: bool,
    /// `TimeTillResetRange`.
    pub time_till_reset: [f32; 2],
    /// The actor is a `TrigerredEmitter`/`TrigerredExplosionEmitter`/`PloufEmitter` (its
    /// `PostBeginPlay` assigns `Disabled = bInitiallyOn` to every sub-emitter).
    pub triggered: bool,
    /// Effective state at level start: `false` for a triggered actor with the default
    /// `bInitiallyOn=false`; for other actors the sub-emitter's own `Disabled` decides (this field
    /// is then informational).
    pub initially_active: bool,
    /// Sub-emitters in `Emitters` order.
    pub emitters: Vec<EmitterDesc>,
}

/// Names the simulator reads; every other property present on a subobject is listed as
/// unsupported (counted by [`import_particles`]).
const MODELED_PROPS: &[&str] = &[
    "maxparticles",
    "lifetimerange",
    "initialdelayrange",
    "initialtimerange",
    "startlocationoffset",
    "startlocationrange",
    "startlocationshape",
    "sphereradiusrange",
    "startvelocityrange",
    "acceleration",
    "velocitylossrange",
    "maxabsvelocity",
    "usecolorscale",
    "colorscale",
    "colorscalerepeats",
    "fadein",
    "fadeinendtime",
    "fadeinfactor",
    "fadeout",
    "fadeoutstarttime",
    "fadeoutfactor",
    "startsizerange",
    "usesizescale",
    "useregularsizescale",
    "uniformsize",
    "sizescale",
    "sizescalerepeats",
    "spinparticles",
    "startspinrange",
    "spinspersecondrange",
    "drawstyle",
    "texture",
    "textureusubdivisions",
    "texturevsubdivisions",
    "subdivisionstart",
    "subdivisionend",
    "blendbetweensubdivisions",
    "userandomsubdivision",
    "respawndeadparticles",
    "automaticinitialspawning",
    "particlespersecond",
    "initialparticlespersecond",
    "disabled",
    "coordinatesystem",
    "secondsbeforeinactive",
    "autodestroy",
    "autoreset",
    "name",
    "staticmesh",
    "usemeshblendmode",
    "rendertwosided",
    "scaleaxis",
];

/// Property names whose inherited class default is consulted when the map block omits them.
const INHERITED_PROPS: &[&str] = &[
    "maxparticles",
    "lifetimerange",
    "initialdelayrange",
    "initialtimerange",
    "startlocationoffset",
    "startlocationrange",
    "startlocationshape",
    "sphereradiusrange",
    "startvelocityrange",
    "acceleration",
    "velocitylossrange",
    "maxabsvelocity",
    "usecolorscale",
    "colorscale",
    "colorscalerepeats",
    "fadein",
    "fadeinendtime",
    "fadeinfactor",
    "fadeout",
    "fadeoutstarttime",
    "fadeoutfactor",
    "startsizerange",
    "usesizescale",
    "useregularsizescale",
    "uniformsize",
    "sizescale",
    "sizescalerepeats",
    "spinparticles",
    "startspinrange",
    "spinspersecondrange",
    "drawstyle",
    "texture",
    "textureusubdivisions",
    "texturevsubdivisions",
    "subdivisionstart",
    "subdivisionend",
    "blendbetweensubdivisions",
    "userandomsubdivision",
    "respawndeadparticles",
    "automaticinitialspawning",
    "particlespersecond",
    "initialparticlespersecond",
    "disabled",
    "coordinatesystem",
    "secondsbeforeinactive",
    "autodestroy",
    "autoreset",
];

/// `ParticleEmitter` (and subclass) class default for a property the map does not serialize.
/// Values are the measured `system/engine.u` class defaults (`xiii-tool script defaults`) and the
/// UT2003 emitter reference; struct-typed defaults that `xiii-script` reports as `Unsupported`
/// fall back to these documented values.
fn documented_default(name: &str) -> Option<ParticleValue> {
    Some(match name {
        "maxparticles" => ParticleValue::Int(0),
        "lifetimerange" => ParticleValue::Range([4.0, 4.0]),
        "initialdelayrange" | "initialtimerange" => ParticleValue::Range([0.0, 0.0]),
        "startsizerange" => ParticleValue::RangeVector([[100.0, 100.0]; 3]),
        "drawstyle" => ParticleValue::Byte(3),
        "respawndeadparticles"
        | "automaticinitialspawning"
        | "useregularsizescale"
        | "uniformsize" => ParticleValue::Bool(true),
        "fadeinfactor" | "fadeoutfactor" => ParticleValue::Plane([1.0, 1.0, 1.0, 1.0]),
        "particlespersecond" | "initialparticlespersecond" => ParticleValue::Float(0.0),
        "secondsbeforeinactive" => ParticleValue::Float(1.0),
        _ => return None,
    })
}

// ---------------------------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------------------------

fn f32_of(v: &BTreeMap<String, ParticleValue>, name: &str, default: f32) -> f32 {
    match v.get(name) {
        Some(ParticleValue::Float(x)) => *x,
        Some(ParticleValue::Int(i)) => *i as f32,
        Some(ParticleValue::Byte(b)) => f32::from(*b),
        _ => default,
    }
}

fn i32_of(v: &BTreeMap<String, ParticleValue>, name: &str, default: i32) -> i32 {
    match v.get(name) {
        Some(ParticleValue::Int(i)) => *i,
        Some(ParticleValue::Float(x)) => *x as i32,
        Some(ParticleValue::Byte(b)) => i32::from(*b),
        _ => default,
    }
}

fn u8_of(v: &BTreeMap<String, ParticleValue>, name: &str, default: u8) -> u8 {
    match v.get(name) {
        Some(ParticleValue::Byte(b)) => *b,
        Some(ParticleValue::Int(i)) => u8::try_from(*i).unwrap_or(default),
        _ => default,
    }
}

fn bool_of(v: &BTreeMap<String, ParticleValue>, name: &str, default: bool) -> bool {
    match v.get(name) {
        Some(ParticleValue::Bool(b)) => *b,
        _ => default,
    }
}

fn vec3_of(v: &BTreeMap<String, ParticleValue>, name: &str, default: [f32; 3]) -> [f32; 3] {
    match v.get(name) {
        Some(ParticleValue::Vector(x)) => *x,
        _ => default,
    }
}

fn range_of(v: &BTreeMap<String, ParticleValue>, name: &str, default: [f32; 2]) -> [f32; 2] {
    match v.get(name) {
        Some(ParticleValue::Range(x)) => *x,
        _ => default,
    }
}

fn range_vec_of(
    v: &BTreeMap<String, ParticleValue>,
    name: &str,
    default: [[f32; 2]; 3],
) -> [[f32; 2]; 3] {
    match v.get(name) {
        Some(ParticleValue::RangeVector(x)) => *x,
        _ => default,
    }
}

fn plane_of(v: &BTreeMap<String, ParticleValue>, name: &str, default: [f32; 4]) -> [f32; 4] {
    match v.get(name) {
        Some(ParticleValue::Plane(x)) => *x,
        _ => default,
    }
}

fn object_of(v: &BTreeMap<String, ParticleValue>, name: &str) -> Option<ObjectRef> {
    match v.get(name) {
        Some(ParticleValue::Object(r)) if !r.is_null() => Some(*r),
        _ => None,
    }
}

/// Decodes one `ParticleEmitter` subobject from its effective property values. `class` is the
/// short class name; `values` already merges the map block over the inherited class defaults.
pub fn decode_emitter(
    class: &str,
    name: &str,
    values: &BTreeMap<String, ParticleValue>,
) -> EmitterDesc {
    let color_scale = match values.get("colorscale") {
        Some(ParticleValue::ColorScale(v)) => {
            let mut v = v.clone();
            v.sort_by(|a, b| a.0.total_cmp(&b.0));
            v
        }
        _ => Vec::new(),
    };
    let mut size_scale = match values.get("sizescale") {
        Some(ParticleValue::SizeScale(v)) => v.clone(),
        _ => Vec::new(),
    };
    size_scale.sort_by(|a, b| a.0.total_cmp(&b.0));

    let unsupported: Vec<String> = values
        .keys()
        .filter(|k| !MODELED_PROPS.contains(&k.as_str()))
        .cloned()
        .collect();

    let shape = match class.to_ascii_lowercase().as_str() {
        "beamemitter" | "sparkemitter" | "trailemitter" => {
            EmitterShape::Unsupported(class.to_owned())
        }
        "meshemitter" => EmitterShape::Sprite, // replaced with Mesh once the mesh resolves
        _ => EmitterShape::Sprite,
    };

    let max_particles = i32_of(values, "maxparticles", 0).max(0) as usize;
    EmitterDesc {
        class: class.to_owned(),
        name: name.to_owned(),
        max_particles,
        lifetime: range_of(values, "lifetimerange", [4.0, 4.0]),
        initial_delay: range_of(values, "initialdelayrange", [0.0, 0.0]),
        initial_time: range_of(values, "initialtimerange", [0.0, 0.0]),
        start_location_offset: vec3_of(values, "startlocationoffset", [0.0; 3]),
        start_location_range: range_vec_of(values, "startlocationrange", [[0.0; 2]; 3]),
        start_location_shape: u8_of(values, "startlocationshape", 0),
        sphere_radius: range_of(values, "sphereradiusrange", [0.0, 0.0]),
        start_velocity: range_vec_of(values, "startvelocityrange", [[0.0; 2]; 3]),
        acceleration: vec3_of(values, "acceleration", [0.0; 3]),
        velocity_loss: range_vec_of(values, "velocitylossrange", [[0.0; 2]; 3]),
        max_abs_velocity: vec3_of(values, "maxabsvelocity", [0.0; 3]),
        use_color_scale: bool_of(values, "usecolorscale", false),
        color_scale,
        color_scale_repeats: f32_of(values, "colorscalerepeats", 0.0),
        fade_in: bool_of(values, "fadein", false),
        fade_in_end_time: f32_of(values, "fadeinendtime", 0.0),
        fade_in_factor: plane_of(values, "fadeinfactor", [1.0; 4]),
        fade_out: bool_of(values, "fadeout", false),
        fade_out_start_time: f32_of(values, "fadeoutstarttime", 0.0),
        fade_out_factor: plane_of(values, "fadeoutfactor", [1.0; 4]),
        start_size: range_vec_of(values, "startsizerange", [[100.0, 100.0]; 3]),
        use_size_scale: bool_of(values, "usesizescale", false),
        use_regular_size_scale: bool_of(values, "useregularsizescale", true),
        uniform_size: bool_of(values, "uniformsize", true),
        size_scale,
        size_scale_repeats: f32_of(values, "sizescalerepeats", 0.0),
        spin_particles: bool_of(values, "spinparticles", false),
        start_spin: range_vec_of(values, "startspinrange", [[0.0; 2]; 3]),
        spins_per_second: range_vec_of(values, "spinspersecondrange", [[0.0; 2]; 3]),
        draw_style: u8_of(values, "drawstyle", 3),
        texture: None,
        texture_ref: object_of(values, "texture"),
        mesh_ref: object_of(values, "staticmesh"),
        subdivisions_u: i32_of(values, "textureusubdivisions", 0).max(0) as u32,
        subdivisions_v: i32_of(values, "texturevsubdivisions", 0).max(0) as u32,
        subdivision_start: i32_of(values, "subdivisionstart", 0),
        subdivision_end: i32_of(values, "subdivisionend", 0),
        blend_between_subdivisions: bool_of(values, "blendbetweensubdivisions", false),
        use_random_subdivision: bool_of(values, "userandomsubdivision", false),
        respawn_dead_particles: bool_of(values, "respawndeadparticles", true),
        automatic_initial_spawning: bool_of(values, "automaticinitialspawning", true),
        particles_per_second: f32_of(values, "particlespersecond", 0.0),
        initial_particles_per_second: f32_of(values, "initialparticlespersecond", 0.0),
        disabled: bool_of(values, "disabled", false),
        coordinate_system: u8_of(values, "coordinatesystem", 0),
        seconds_before_inactive: f32_of(values, "secondsbeforeinactive", 1.0),
        auto_destroy: bool_of(values, "autodestroy", false),
        auto_reset: bool_of(values, "autoreset", false),
        shape,
        unsupported,
    }
}

/// Converts a decoded tagged value to a [`ParticleValue`]; structs whose name is unknown to
/// `xiii-script` and arrays other than the two scale arrays are not modelled (`None`).
fn value_to_particle(v: &PropertyValue) -> Option<ParticleValue> {
    Some(match v {
        PropertyValue::Float(x) => ParticleValue::Float(*x),
        PropertyValue::Int(i) => ParticleValue::Int(*i),
        PropertyValue::Byte(b) => ParticleValue::Byte(*b),
        PropertyValue::Bool(b) => ParticleValue::Bool(*b),
        PropertyValue::Object(r) | PropertyValue::Class(r) => ParticleValue::Object(*r),
        PropertyValue::Str(s) => ParticleValue::Str(s.clone()),
        PropertyValue::Struct(s) => match s {
            StructValue::Vector(v) => ParticleValue::Vector(*v),
            StructValue::Range(r) => ParticleValue::Range(*r),
            StructValue::RangeVector(r) => ParticleValue::RangeVector(*r),
            StructValue::Plane(p) => ParticleValue::Plane(*p),
            StructValue::Color(c) => ParticleValue::Color(*c),
            _ => return None,
        },
        _ => return None,
    })
}

fn value_of_script(v: &Value) -> Option<ParticleValue> {
    Some(match v {
        Value::Float(x) => ParticleValue::Float(*x),
        Value::Int(i) => ParticleValue::Int(*i),
        Value::Byte(b) => ParticleValue::Byte(*b),
        Value::Bool(b) => ParticleValue::Bool(*b),
        Value::Vector(v) => ParticleValue::Vector(*v),
        _ => return None,
    })
}

/// Reads `array<ParticleColorScale>` (`{ f32 RelativeTime; u8 R,G,B,A }`, 8 bytes each; the
/// struct order was measured on Plage01: the first float takes 0/0.5/1 and the trailing bytes are
/// the color). Alpha is stored but unused by most draw styles.
fn decode_color_scale(data: &[u8], span: Span, count: u32) -> Vec<(f32, [u8; 4])> {
    let mut out = Vec::new();
    let mut off = span.start;
    for _ in 0..count {
        if off + 8 > span.end || off + 8 > data.len() {
            break;
        }
        let t = f32::from_le_bytes(data[off..off + 4].try_into().unwrap_or([0; 4]));
        out.push((
            t,
            [data[off + 4], data[off + 5], data[off + 6], data[off + 7]],
        ));
        off += 8;
    }
    out
}

/// Reads `array<ParticleTimeScale>` (`{ f32 RelativeTime; f32 RelativeSize }`).
fn decode_size_scale(data: &[u8], span: Span, count: u32) -> Vec<(f32, f32)> {
    let mut out = Vec::new();
    let mut off = span.start;
    for _ in 0..count {
        if off + 8 > span.end || off + 8 > data.len() {
            break;
        }
        let t = f32::from_le_bytes(data[off..off + 4].try_into().unwrap_or([0; 4]));
        let s = f32::from_le_bytes(data[off + 4..off + 8].try_into().unwrap_or([0; 4]));
        out.push((t, s));
        off += 8;
    }
    out
}

/// Builds the effective value map of one subobject: the map's tagged block first, then inherited
/// class defaults for the properties the block omits, then documented UE2 defaults for absent
/// struct-typed values.
fn values_from_block(
    package: &Package,
    data: &[u8],
    props: &xiii_package::ObjectProperties,
    layout: &ClassLayout,
) -> BTreeMap<String, ParticleValue> {
    let mut values = BTreeMap::new();
    for p in &props.block.properties {
        let name = package.property_name(p).to_ascii_lowercase();
        if name == "colorscale" {
            if let PropertyValue::Array { count, elements } = &p.value {
                values.insert(
                    name,
                    ParticleValue::ColorScale(decode_color_scale(data, *elements, *count)),
                );
            }
            continue;
        }
        if name == "sizescale" {
            if let PropertyValue::Array { count, elements } = &p.value {
                values.insert(
                    name,
                    ParticleValue::SizeScale(decode_size_scale(data, *elements, *count)),
                );
            }
            continue;
        }
        if let Some(v) = value_to_particle(&p.value) {
            values.entry(name).or_insert(v);
        }
    }
    for &name in INHERITED_PROPS {
        if values.contains_key(name) {
            continue;
        }
        if let Some(default) = layout
            .slot_by_name(name)
            .and_then(|s| layout.defaults.get(s.base))
            .and_then(value_of_script)
        {
            values.insert(name.to_owned(), default);
        }
    }
    for &name in INHERITED_PROPS {
        if !values.contains_key(name)
            && let Some(default) = documented_default(name)
        {
            values.insert(name.to_owned(), default);
        }
    }
    values
}

/// Reads the object references of an `Emitters` dynamic array.
fn emitter_refs(
    package: &Package,
    data: &[u8],
    props: &xiii_package::ObjectProperties,
) -> Vec<ObjectRef> {
    let p = Props::new(package, props);
    let Some(PropertyValue::Array { elements, .. }) = p.get("Emitters").map(|x| &x.value) else {
        return Vec::new();
    };
    if elements.end > data.len() {
        return Vec::new();
    }
    let mut cursor = Cursor::new(&data[..elements.end]);
    if cursor.seek(elements.start).is_err() {
        return Vec::new();
    }
    let mut out = Vec::new();
    while cursor.pos() < elements.end {
        match cursor.compact_index() {
            Ok(raw) => {
                if let Some(r) = package.resolve(raw)
                    && !r.is_null()
                {
                    out.push(r);
                }
            }
            Err(_) => break,
        }
    }
    out
}

/// Reads the placement fields of an export into an [`ActorPlacement`] so the shared
/// [`ClassDefaults::resolve`] can apply the map/class/engine fallback rule.
fn actor_placement(package: &Package, export: usize, p: &Props<'_>) -> ActorPlacement {
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
            Some(PropertyValue::Struct(StructValue::Vector(v))) => Some(*v),
            _ => None,
        },
        pre_pivot: p.vector("PrePivot"),
        static_mesh: None,
        static_mesh_instance: None,
        draw_type: None,
        hidden: p.bool("bHidden").unwrap_or(false),
        collision_flags: [None; 3],
        collision_height: None,
        collision_radius: None,
    }
}

// ---------------------------------------------------------------------------------------------
// Import
// ---------------------------------------------------------------------------------------------

/// Decodes every map emitter, resolves its textures/meshes and appends the systems to
/// `im.scene`. Called by [`crate::import_map`] after the static geometry. Counts every emitter
/// class, sub-emitter class and property in use, plus unsupported properties/classes.
pub(crate) fn import_particles(
    im: &mut Importer<'_>,
    map_pkg: &Arc<Loaded>,
    defaults: &mut ClassDefaults,
) {
    let package = &map_pkg.package;
    let mut sub_emitters: BTreeMap<usize, EmitterDesc> = BTreeMap::new();
    let mut actors: Vec<(usize, Vec<ObjectRef>, bool, bool)> = Vec::new();
    for i in 0..package.exports().len() {
        if package.exports()[i].serial_size == 0 {
            continue;
        }
        let Some(class) = package.export_class_path(i) else {
            continue;
        };
        let short = class.rsplit('.').next().unwrap_or("").to_owned();
        if !short.to_ascii_lowercase().contains("emit") {
            continue;
        }
        let Ok(layout) = defaults.layout(class) else {
            continue;
        };
        let has = |name: &str| layout.chain_names.iter().any(|n| n == name);
        if !has("particleemitter") && !has("emitter") {
            continue;
        }
        let props = match package.read_object_properties(&map_pkg.data, i, &Limits::default()) {
            Ok(p) => p,
            Err(e) => {
                im.scene.fail(
                    "fail.particle.properties",
                    format!(
                        "{}: {e}",
                        package
                            .object_path(ObjectRef::Export(i as u32))
                            .unwrap_or("?")
                    ),
                );
                continue;
            }
        };
        if has("particleemitter") {
            for prop in &props.block.properties {
                let name = package.property_name(prop).to_ascii_lowercase();
                im.scene.count(&format!("particle.property.{name}"), 1);
            }
            let values = values_from_block(package, &map_pkg.data, &props, &layout);
            let name = package
                .object_path(ObjectRef::Export(i as u32))
                .unwrap_or("?")
                .to_owned();
            let mut desc = decode_emitter(&short, &name, &values);
            im.scene.count(&format!("particle.subemitter.{short}"), 1);
            for k in values.keys() {
                if !MODELED_PROPS.contains(&k.as_str()) {
                    im.scene
                        .count(&format!("particle.unsupported_property ({k})"), 1);
                }
            }
            if let EmitterShape::Unsupported(c) = &desc.shape {
                im.scene
                    .count(&format!("particle.unsupported_class.{c}"), 1);
            }
            if desc.max_particles == 0 {
                im.scene.count("particle.note.max_particles_zero", 1);
            }
            if desc.texture_ref.is_none() && desc.mesh_ref.is_none() {
                im.scene.count("particle.note.no_texture", 1);
            }
            resolve_assets(im, map_pkg, &mut desc);
            sub_emitters.insert(i, desc);
        } else {
            let refs = emitter_refs(package, &map_pkg.data, &props);
            // `TrigerredEmitter.PostBeginPlay` (measured disassembly, `xidcine.u`) does
            // `Emitters[i].Disabled = bInitiallyOn`; the default `bInitiallyOn` is false, so the
            // whole actor starts inactive. `bInitiallyOn` is honoured per map instance.
            let triggered = has("trigerredemitter");
            let b_initially_on = effective_bool(package, &props, &layout, "bInitiallyOn", false);
            im.scene.count(&format!("particle.actor.{short}"), 1);
            im.scene.count("particle.emitters_referenced", refs.len());
            actors.push((i, refs, triggered, b_initially_on));
        }
    }

    for (actor_i, refs, triggered, b_initially_on) in actors {
        let Some(props) = package
            .read_object_properties(&map_pkg.data, actor_i, &Limits::default())
            .ok()
        else {
            continue;
        };
        let p = Props::new(package, &props);
        let placement = actor_placement(package, actor_i, &p);
        let (effective, _) = match defaults.resolve(&placement.class, &placement) {
            Ok(v) => v,
            Err(e) => {
                im.scene.fail(
                    "fail.particle.placement",
                    format!("{}: {e}", placement.path),
                );
                continue;
            }
        };
        let mut emitters = Vec::new();
        for r in refs {
            match im.cache.resolve(map_pkg, r) {
                Ok((target, idx)) if Arc::ptr_eq(&target, map_pkg) => {
                    if let Some(desc) = sub_emitters.get(&idx) {
                        emitters.push(desc.clone());
                    } else {
                        im.scene.fail(
                            "fail.particle.subobject_missing",
                            format!("{} -> export {idx}", placement.path),
                        );
                    }
                }
                Ok((target, idx)) => {
                    im.scene.fail(
                        "fail.particle.subobject_external",
                        format!("{} -> {}.{idx}", placement.path, target.name),
                    );
                }
                Err(e) => {
                    im.scene.fail(
                        "fail.particle.subobject_ref",
                        format!("{}: {e}", placement.path),
                    );
                }
            }
        }
        // `TrigerredEmitter.PostBeginPlay` assigns `Disabled = bInitiallyOn` to every sub-emitter,
        // so a triggered actor with the default `bInitiallyOn=false` starts fully inactive.
        let initially_active = !triggered || b_initially_on;
        let short = placement.class.rsplit('.').next().unwrap_or("").to_owned();
        let state_key = if initially_active {
            format!("particle.initially_active.{short}")
        } else {
            format!("particle.initially_inactive.{short}")
        };
        im.scene.count(&state_key, 1);
        im.scene.count("particle.systems", 1);
        im.scene.count("particle.system_emitters", emitters.len());
        if emitters.is_empty() {
            im.scene.count("particle.system_empty", 1);
        }
        im.scene.particle_systems.push(ParticleSystem {
            path: placement.path.clone(),
            transform: placement_transform(&effective),
            auto_destroy: p.bool("AutoDestroy").unwrap_or(true),
            auto_reset: p.bool("AutoReset").unwrap_or(false),
            time_till_reset: match p.get("TimeTillResetRange").map(|x| &x.value) {
                Some(PropertyValue::Struct(StructValue::Range(r))) => *r,
                _ => [0.0, 0.0],
            },
            triggered,
            initially_active,
            emitters,
        });
    }
}

/// Effective boolean of an actor property: the map's tagged value, else the inherited class
/// default, else `default`.
fn effective_bool(
    package: &Package,
    props: &xiii_package::ObjectProperties,
    layout: &ClassLayout,
    name: &str,
    default: bool,
) -> bool {
    let p = Props::new(package, props);
    if let Some(v) = p.bool(name) {
        return v;
    }
    if let Some(slot) = layout.slot_by_name(name)
        && let Some(Value::Bool(b)) = layout.defaults.get(slot.base)
    {
        return *b;
    }
    default
}

/// Resolves a sub-emitter's sprite texture or static-mesh geometry through the shared importer
/// (the same texture path the rest of the scene uses). Failures are counted, never dropped.
fn resolve_assets(im: &mut Importer<'_>, from: &Arc<Loaded>, desc: &mut EmitterDesc) {
    if let Some(r) = desc.texture_ref {
        match im.texture_from(from, r, 0) {
            Ok(t) => {
                desc.texture = Some(t);
                im.scene.count("particle.texture.resolved", 1);
            }
            Err(e) => {
                im.scene
                    .fail("fail.particle.texture", format!("{}: {e}", desc.name));
            }
        }
    }
    let Some(r) = desc.mesh_ref else { return };
    let (pkg, idx) = match im.cache.resolve(from, r) {
        Ok(v) => v,
        Err(e) => {
            im.scene
                .fail("fail.particle.mesh_ref", format!("{}: {e}", desc.name));
            return;
        }
    };
    let mesh = match decode_static_mesh(&pkg.package, &pkg.data, idx) {
        Ok(m) => m,
        Err(e) => {
            im.scene
                .fail("fail.particle.mesh_decode", format!("{}: {e}", desc.name));
            return;
        }
    };
    let positions: Vec<[f32; 3]> = mesh
        .vertices
        .iter()
        .map(|v| to_bevy_position(v.position))
        .collect();
    let normals: Vec<[f32; 3]> = mesh
        .vertices
        .iter()
        .map(|v| to_bevy_direction(v.normal))
        .collect();
    let uvs: Vec<[f32; 2]> = mesh
        .uv_streams
        .first()
        .map(|s| s.uvs.clone())
        .unwrap_or_else(|| vec![[0.0; 2]; mesh.vertices.len()]);
    let mut indices = Vec::new();
    for si in 0..mesh.sections.len() {
        indices.extend(mesh.section_indices(si).iter().map(|&i| u32::from(i)));
    }
    let (texture, two_sided) = match mesh.materials.first() {
        Some(m) => {
            let (slot, mat_index) = im.material(&pkg, m.material, "particle");
            let resolved = &im.scene.materials[mat_index];
            let tex = match slot {
                crate::MaterialSlot::Texture(t) => Some(t),
                crate::MaterialSlot::Missing(_) => resolved.base,
            };
            (tex, resolved.two_sided)
        }
        None => (None, false),
    };
    im.scene.count("particle.mesh.resolved", 1);
    im.scene.count("particle.mesh.vertices", positions.len());
    desc.shape = EmitterShape::Mesh(Box::new(ParticleMesh {
        positions,
        normals,
        uvs,
        indices,
        texture,
        two_sided,
    }));
}

// ---------------------------------------------------------------------------------------------
// Simulation
// ---------------------------------------------------------------------------------------------

/// One live particle. Positions/velocities are **Unreal units relative to the emitter actor**
/// (`PTCS_Independent` semantics); the renderer applies the actor placement and the axis change.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Particle {
    /// Position relative to the emitter.
    pub position: [f32; 3],
    /// Velocity.
    pub velocity: [f32; 3],
    /// Age in seconds.
    pub age: f32,
    /// Lifetime in seconds.
    pub lifetime: f32,
    /// Base size per axis (UU), before the size scale.
    pub size: [f32; 3],
    /// Base color (RGB from `ColorScale`; alpha is filled from the fade factors at draw time).
    pub color: [u8; 4],
    /// Spin `(pitch, yaw, roll)` in Unreal rotator units.
    pub spin: [f32; 3],
    /// Spin rate per second.
    pub spin_rate: [f32; 3],
    /// Subdivision frame index.
    pub subdivision: u32,
    /// Slot is alive.
    pub active: bool,
}

/// Deterministic xorshift64* generator. The seed is fixed per sub-emitter index, so two runs of
/// the same map produce identical particles.
#[derive(Debug, Clone)]
struct Rng {
    state: u64,
}

impl Rng {
    fn new(seed: u64) -> Self {
        Self { state: seed | 1 }
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.state = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform in `0..1`.
    fn unit(&mut self) -> f32 {
        (self.next_u64() >> 40) as f32 / (1u64 << 24) as f32
    }

    fn range(&mut self, r: [f32; 2]) -> f32 {
        let (lo, hi) = if r[0] <= r[1] {
            (r[0], r[1])
        } else {
            (r[1], r[0])
        };
        lo + (hi - lo) * self.unit()
    }

    fn range_vec(&mut self, r: [[f32; 2]; 3]) -> [f32; 3] {
        [self.range(r[0]), self.range(r[1]), self.range(r[2])]
    }
}

/// Simulator of one sub-emitter.
#[derive(Debug, Clone)]
pub struct EmitterSim {
    /// Particle pool (active and dead slots).
    pub particles: Vec<Particle>,
    /// Whether the emitter currently spawns (mirrors `ParticleEmitter.Disabled`).
    pub enabled: bool,
    rng: Rng,
    spawn_fraction: f32,
    elapsed: f32,
    /// Total particles spawned; with `RespawnDeadParticles=false` the emitter stops after
    /// `MaxParticles` (the slot index does not wrap, upstream UT2003 `SpawnParticle`).
    spawned_total: usize,
}

impl EmitterSim {
    /// Builds an empty simulator with a deterministic seed derived from `index`.
    pub fn new(index: usize) -> Self {
        Self {
            particles: Vec::new(),
            enabled: true,
            rng: Rng::new(0x9E37_79B9_7F4A_7C15u64.wrapping_mul(index as u64 + 1)),
            spawn_fraction: 0.0,
            elapsed: 0.0,
            spawned_total: 0,
        }
    }

    /// Enables or disables spawning (existing particles keep living out their lifetime).
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Flips spawning on/off (a `TriggerEmit`/`TriggerToggle` host bridge).
    pub fn toggle(&mut self) {
        self.enabled = !self.enabled;
    }

    /// Clears the pool and spawn accumulator (a reset/`TriggerControl` host bridge).
    pub fn reset(&mut self) {
        self.particles.clear();
        self.spawn_fraction = 0.0;
        self.spawned_total = 0;
    }

    /// Number of active particles.
    pub fn active(&self) -> usize {
        self.particles.iter().filter(|p| p.active).count()
    }

    /// Spawns one particle into a slot with fresh randomized values.
    fn spawn_one(&mut self, desc: &EmitterDesc) {
        let position = match desc.start_location_shape {
            // PTLS_Box
            0 => {
                let r = self.rng.range_vec(desc.start_location_range);
                [
                    desc.start_location_offset[0] + r[0],
                    desc.start_location_offset[1] + r[1],
                    desc.start_location_offset[2] + r[2],
                ]
            }
            // PTLS_Sphere: uniform direction, radius from `SphereRadiusRange`, plus the offset.
            1 => {
                let radius = self.rng.range(desc.sphere_radius);
                let (x, y, z) = loop {
                    let a = self.rng.unit() * 2.0 - 1.0;
                    let b = self.rng.unit() * 2.0 - 1.0;
                    let c = self.rng.unit() * 2.0 - 1.0;
                    let len2 = a * a + b * b + c * c;
                    if len2 > 1e-6 && len2 <= 1.0 {
                        break (a, b, c);
                    }
                };
                [
                    desc.start_location_offset[0] + x * radius,
                    desc.start_location_offset[1] + y * radius,
                    desc.start_location_offset[2] + z * radius,
                ]
            }
            // PTLS_Polar and PTLS_All need polar ranges that are not modelled.
            _ => desc.start_location_offset,
        };
        let velocity = self.rng.range_vec(desc.start_velocity);
        let size = self.rng.range_vec(desc.start_size);
        let spin = if desc.spin_particles {
            self.rng.range_vec(desc.start_spin)
        } else {
            [0.0; 3]
        };
        let spin_rate = if desc.spin_particles {
            self.rng.range_vec(desc.spins_per_second)
        } else {
            [0.0; 3]
        };
        let lifetime = self.rng.range(desc.lifetime).max(1.0 / 512.0);
        // `InitialTimeRange` starts a particle already aged (UT2003 reference).
        let age = self
            .rng
            .range(desc.initial_time)
            .clamp(0.0, lifetime * 0.999);
        let rel = age / lifetime;
        let color = if desc.use_color_scale && !desc.color_scale.is_empty() {
            sample_color(&desc.color_scale, rel, desc.color_scale_repeats)
        } else {
            [255, 255, 255, 255]
        };
        let subdivision = if desc.use_random_subdivision {
            (self.rng.next_u64()
                % u64::from(desc.subdivisions_u.max(1) * desc.subdivisions_v.max(1)))
                as u32
        } else {
            subdivision_frame(desc, rel)
        };
        let particle = Particle {
            position,
            velocity,
            age,
            lifetime,
            size,
            color,
            spin,
            spin_rate,
            subdivision,
            active: true,
        };
        if let Some(slot) = self.particles.iter_mut().find(|p| !p.active) {
            *slot = particle;
        } else {
            self.particles.push(particle);
        }
    }

    /// Advances one sub-emitter by `dt` seconds.
    pub fn step(&mut self, desc: &EmitterDesc, dt: f32) {
        if !(dt.is_finite() && dt > 0.0) {
            return;
        }
        self.elapsed += dt;
        // Update existing particles.
        let avg_life = desc.average_lifetime();
        for p in &mut self.particles {
            if !p.active {
                continue;
            }
            p.age += dt;
            if p.age >= p.lifetime {
                if desc.respawn_dead_particles {
                    p.active = false;
                } else {
                    p.active = false;
                    continue;
                }
            }
            p.velocity[0] += desc.acceleration[0] * dt;
            p.velocity[1] += desc.acceleration[1] * dt;
            p.velocity[2] += desc.acceleration[2] * dt;
            for axis in 0..3 {
                let loss = desc.velocity_loss[axis];
                let factor = (1.0 - loss[1] * dt).max(0.0);
                p.velocity[axis] *= factor;
                if desc.max_abs_velocity[axis] > 0.0 {
                    p.velocity[axis] = p.velocity[axis]
                        .clamp(-desc.max_abs_velocity[axis], desc.max_abs_velocity[axis]);
                }
            }
            p.position[0] += p.velocity[0] * dt;
            p.position[1] += p.velocity[1] * dt;
            p.position[2] += p.velocity[2] * dt;
            p.spin[0] += p.spin_rate[0] * dt * 65536.0;
            p.spin[1] += p.spin_rate[1] * dt * 65536.0;
            p.spin[2] += p.spin_rate[2] * dt * 65536.0;
            if desc.use_color_scale && !desc.color_scale.is_empty() {
                p.color = sample_color(
                    &desc.color_scale,
                    (p.age / p.lifetime).clamp(0.0, 1.0),
                    desc.color_scale_repeats,
                );
            }
            if !desc.use_random_subdivision {
                p.subdivision = subdivision_frame(desc, (p.age / p.lifetime).clamp(0.0, 1.0));
            }
        }
        // Spawn.
        // Spawning is gated by the effective enabled state (the importer folds the sub-emitter's
        // own `Disabled` and the triggered actor's `bInitiallyOn` into `EmitterSim::enabled`), so
        // `desc.disabled` is deliberately not re-checked here.
        if !self.enabled || desc.max_particles == 0 {
            return;
        }
        let alive = self.active();
        let rate = if desc.automatic_initial_spawning {
            if desc.particles_per_second > 0.0 {
                desc.particles_per_second
            } else {
                desc.max_particles as f32 / avg_life
            }
        } else if alive < desc.max_particles {
            desc.initial_particles_per_second
        } else {
            desc.particles_per_second
        };
        // `RespawnDeadParticles=false` stops once `MaxParticles` particles have ever spawned;
        // dead slots are not refilled (upstream UT2003 `SpawnParticle` only wraps the slot index
        // when respawning is enabled).
        if !desc.respawn_dead_particles && self.spawned_total >= desc.max_particles {
            return;
        }
        self.spawn_fraction += rate * dt;
        while self.spawn_fraction >= 1.0 {
            self.spawn_fraction -= 1.0;
            if self.active() >= desc.max_particles {
                break;
            }
            if !desc.respawn_dead_particles && self.spawned_total >= desc.max_particles {
                break;
            }
            self.spawn_one(desc);
            self.spawned_total += 1;
        }
    }
}

/// Maps a relative lifetime `t` to the position inside the (possibly repeated) scale.
fn repeat_u(t: f32, repeats: f32) -> f32 {
    let reps = if repeats.is_finite() && repeats > 0.0 {
        repeats
    } else {
        1.0
    };
    let t = t.clamp(0.0, 1.0);
    if reps <= 1.0 { t } else { (t * reps).fract() }
}

/// Piecewise-linear `ColorScale` lookup at relative time `t` (clamped to `0..=1`). `repeats` is
/// the number of times the scale is repeated across the lifetime (0/1 = once). Alpha is returned
/// as 255: the wiki states the stored color alpha is unused by most draw styles, so the renderer
/// derives transparency from the texture and the fade factors.
pub fn sample_color(scale: &[(f32, [u8; 4])], t: f32, repeats: f32) -> [u8; 4] {
    if scale.is_empty() {
        return [255, 255, 255, 255];
    }
    let u = repeat_u(t, repeats);
    let Some((a, b, f)) = keyed_pair(scale, u) else {
        return [255, 255, 255, 255];
    };
    [
        lerp_u8(a[0], b[0], f),
        lerp_u8(a[1], b[1], f),
        lerp_u8(a[2], b[2], f),
        255,
    ]
}

fn lerp_u8(a: u8, b: u8, f: f32) -> u8 {
    (f32::from(a) + (f32::from(b) - f32::from(a)) * f)
        .round()
        .clamp(0.0, 255.0) as u8
}

/// Piecewise-linear `SizeScale` lookup at relative time `t`.
pub fn sample_size(scale: &[(f32, f32)], t: f32, repeats: f32) -> f32 {
    if scale.is_empty() {
        return 1.0;
    }
    let u = repeat_u(t, repeats);
    let Some((a, b, f)) = keyed_pair(scale, u) else {
        return 1.0;
    };
    a + (b - a) * f
}

/// Returns the two keys bracketing `u` (mapped from the relative lifetime) and the blend
/// fraction between them. Callers with an empty scale must handle `None`.
fn keyed_pair<T: Copy>(scale: &[(f32, T)], u: f32) -> Option<(T, T, f32)> {
    let first = scale.first()?;
    let last = scale.last()?;
    let t = if last.0 > first.0 {
        first.0 + (last.0 - first.0) * u
    } else {
        u
    };
    if t <= first.0 {
        return Some((first.1, first.1, 0.0));
    }
    let mut prev = *first;
    for &next in &scale[1..] {
        if t <= next.0 {
            let span = next.0 - prev.0;
            let f = if span.abs() > 1e-6 {
                ((t - prev.0) / span).clamp(0.0, 1.0)
            } else {
                0.0
            };
            return Some((prev.1, next.1, f));
        }
        prev = next;
    }
    Some((prev.1, prev.1, 0.0))
}

/// Fade factor per component (RGB + alpha) from the `FadeInFactor`/`FadeOutFactor` planes.
/// Upstream (UT2003 reference): a factor component `1` starts/ends at `0`, `0.5` at half.
pub fn fade_factors(desc: &EmitterDesc, t: f32) -> [f32; 4] {
    let mut out = [1.0f32; 4];
    if desc.fade_in && desc.fade_in_end_time > 1e-6 {
        let progress = (t / desc.fade_in_end_time).clamp(0.0, 1.0);
        for (i, o) in out.iter_mut().enumerate() {
            *o *= 1.0 - desc.fade_in_factor[i] * (1.0 - progress);
        }
    }
    if desc.fade_out && desc.fade_out_start_time >= 0.0 && t > desc.fade_out_start_time {
        let span = desc.lifetime[1] - desc.fade_out_start_time;
        let progress = if span.abs() > 1e-6 {
            ((t - desc.fade_out_start_time) / span).clamp(0.0, 1.0)
        } else {
            1.0
        };
        for (i, o) in out.iter_mut().enumerate() {
            *o *= 1.0 - desc.fade_out_factor[i] * progress;
        }
    }
    out
}

/// Subdivision frame at relative time `t`: blends `SubdivisionStart..End` when
/// `BlendBetweenSubdivisions`, otherwise uses `SubdivisionStart`. Random subdivision is chosen at
/// spawn (see [`EmitterSim::spawn_one`]) and is not deterministic per frame.
fn subdivision_frame(desc: &EmitterDesc, t: f32) -> u32 {
    let frames = desc.subdivisions_u.max(1) * desc.subdivisions_v.max(1);
    let (start, end) = (desc.subdivision_start, desc.subdivision_end);
    let frame = if desc.blend_between_subdivisions && end != start {
        start as f32 + (end - start) as f32 * t.clamp(0.0, 1.0)
    } else {
        start as f32
    };
    (frame.round().max(0.0) as u32).min(frames.saturating_sub(1))
}

/// Whether a sub-emitter spawns at level start. For a triggered actor
/// (`TrigerredEmitter`/`TrigerredExplosionEmitter`/`PloufEmitter`) `PostBeginPlay` assigns
/// `Disabled = bInitiallyOn` to every sub-emitter, overriding the sub-emitter's own value;
/// otherwise the sub-emitter's decoded `Disabled` decides.
pub fn initially_enabled(system: &ParticleSystem, desc: &EmitterDesc) -> bool {
    if system.triggered {
        system.initially_active
    } else {
        !desc.disabled
    }
}

/// Simulator of one [`ParticleSystem`]: one [`EmitterSim`] per sub-emitter.
#[derive(Debug, Clone)]
pub struct ParticleSystemSim {
    /// Per-sub-emitter simulations, in `ParticleSystem::emitters` order.
    pub emitters: Vec<EmitterSim>,
}

impl ParticleSystemSim {
    /// Creates a simulator for `system` with deterministic per-sub-emitter seeds and the
    /// level-start enabled state.
    pub fn new(system: &ParticleSystem) -> Self {
        Self {
            emitters: system
                .emitters
                .iter()
                .enumerate()
                .map(|(i, d)| {
                    let mut sim = EmitterSim::new(i);
                    sim.set_enabled(initially_enabled(system, d));
                    sim
                })
                .collect(),
        }
    }

    /// Steps every sub-emitter by `dt`.
    pub fn step(&mut self, system: &ParticleSystem, dt: f32) {
        for (i, e) in self.emitters.iter_mut().enumerate() {
            if let Some(desc) = system.emitters.get(i) {
                e.step(desc, dt);
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Survey
// ---------------------------------------------------------------------------------------------

/// Campaign/map survey of the emitter classes, sub-emitter classes and properties in use.
#[derive(Debug, Clone, Default)]
pub struct MapSurvey {
    /// Emitter actor short class -> count.
    pub actors: BTreeMap<String, usize>,
    /// Sub-emitter short class -> count.
    pub sub_emitters: BTreeMap<String, usize>,
    /// Property (lower-case) -> number of sub-emitters that use it (map block or inherited).
    pub properties: BTreeMap<String, usize>,
    /// Unsupported property -> count.
    pub unsupported_properties: BTreeMap<String, usize>,
    /// Unsupported/not-modelled sub-emitter class -> count.
    pub unsupported_classes: BTreeMap<String, usize>,
    /// Emitter actor class -> number of its systems active at level start.
    pub initially_active: BTreeMap<String, usize>,
    /// Emitter actor class -> number of its systems inactive at level start.
    pub initially_inactive: BTreeMap<String, usize>,
    /// Number of `Emitter` actors decoded.
    pub systems: usize,
    /// Number of decoded sub-emitters.
    pub sub_emitter_total: usize,
    /// `fail.*` counters (any particle decode/import failure).
    pub failures: BTreeMap<String, usize>,
}

impl MapSurvey {
    /// Extracts the survey from the `particle.*` counters of an imported scene.
    pub fn from_scene(scene: &crate::WorldScene) -> Self {
        let mut out = Self::default();
        for (key, value) in &scene.counters {
            if key.starts_with("fail.particle") {
                out.failures.insert(key.clone(), *value);
            }
            let Some(rest) = key.strip_prefix("particle.") else {
                continue;
            };
            if let Some(class) = rest.strip_prefix("actor.") {
                out.actors.insert(class.to_owned(), *value);
            } else if let Some(class) = rest.strip_prefix("subemitter.") {
                out.sub_emitters.insert(class.to_owned(), *value);
            } else if let Some(prop) = rest.strip_prefix("unsupported_property (") {
                out.unsupported_properties
                    .insert(prop.trim_end_matches(')').to_owned(), *value);
            } else if let Some(class) = rest.strip_prefix("unsupported_class.") {
                out.unsupported_classes.insert(class.to_owned(), *value);
            } else if let Some(class) = rest.strip_prefix("initially_active.") {
                out.initially_active.insert(class.to_owned(), *value);
            } else if let Some(class) = rest.strip_prefix("initially_inactive.") {
                out.initially_inactive.insert(class.to_owned(), *value);
            } else if let Some(prop) = rest.strip_prefix("property.") {
                out.properties.insert(prop.to_owned(), *value);
            }
            if key == "particle.systems" {
                out.systems = *value;
            }
        }
        out.sub_emitter_total = out.sub_emitters.values().sum();
        out
    }
}

/// Imports `map` and returns its emitter survey (uses the same import path as the viewer).
pub fn survey_map(cache: &mut crate::PackageCache, map: &str) -> Result<MapSurvey, String> {
    let scene = crate::import_map(cache, map)?;
    Ok(MapSurvey::from_scene(&scene))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn values(pairs: Vec<(&str, ParticleValue)>) -> BTreeMap<String, ParticleValue> {
        pairs.into_iter().map(|(k, v)| (k.to_owned(), v)).collect()
    }

    #[test]
    fn decode_reads_documented_fields_and_marks_unknown_ones() {
        let v = values(vec![
            ("maxparticles", ParticleValue::Int(8)),
            ("lifetimerange", ParticleValue::Range([6.0, 8.0])),
            (
                "startvelocityrange",
                ParticleValue::RangeVector([[-1.0, 1.0]; 3]),
            ),
            ("acceleration", ParticleValue::Vector([2.0, 2.0, 5.0])),
            ("usecolorscale", ParticleValue::Bool(true)),
            (
                "colorscale",
                ParticleValue::ColorScale(vec![
                    (1.0, [0, 0, 0, 0]),
                    (0.0, [255, 255, 255, 0]),
                    (0.5, [128, 128, 128, 0]),
                ]),
            ),
            ("drawstyle", ParticleValue::Byte(5)),
            (
                "startsizerange",
                ParticleValue::RangeVector([[50.0, 75.0]; 3]),
            ),
            ("usescollision", ParticleValue::Bool(true)),
        ]);
        let d = decode_emitter("SpriteEmitter", "SpriteEmitter6", &v);
        assert_eq!(d.max_particles, 8);
        assert_eq!(d.lifetime, [6.0, 8.0]);
        assert_eq!(d.acceleration, [2.0, 2.0, 5.0]);
        assert_eq!(d.draw_style, 5);
        // The scale is sorted by relative time.
        assert_eq!(d.color_scale[0].0, 0.0);
        assert_eq!(d.color_scale[1].0, 0.5);
        assert_eq!(d.color_scale[2].0, 1.0);
        // An unmodelled property is reported, not ignored.
        assert!(d.unsupported.contains(&"usescollision".to_owned()));
    }

    #[test]
    fn decode_uses_documented_defaults_for_absent_fields() {
        let d = decode_emitter("SpriteEmitter", "x", &BTreeMap::new());
        assert_eq!(d.lifetime, [4.0, 4.0]);
        assert_eq!(d.draw_style, 3);
        assert!(d.respawn_dead_particles);
        assert!(d.automatic_initial_spawning);
        assert_eq!(d.start_size, [[100.0, 100.0]; 3]);
        assert_eq!(d.shape, EmitterShape::Sprite);
    }

    #[test]
    fn beam_and_spark_are_marked_unsupported() {
        let d = decode_emitter("BeamEmitter", "b", &BTreeMap::new());
        assert!(matches!(d.shape, EmitterShape::Unsupported(_)));
    }

    #[test]
    fn triggered_family_starts_inactive_unless_b_initially_on() {
        let desc = simple_desc();
        let mut system = ParticleSystem {
            path: "TrigerredEmitter1".into(),
            transform: xiii_decode::common::actor_to_bevy([0.0; 3], [0; 3], [1.0; 3]),
            auto_destroy: true,
            auto_reset: false,
            time_till_reset: [0.0, 0.0],
            triggered: true,
            initially_active: false,
            emitters: vec![desc.clone()],
        };
        // Default `bInitiallyOn=false`: a triggered actor is inactive at level start.
        assert!(!initially_enabled(&system, &desc));
        // A map instance with `bInitiallyOn=true` starts active.
        system.initially_active = true;
        assert!(initially_enabled(&system, &desc));
        // A non-triggered actor follows the sub-emitter's own `Disabled`.
        system.triggered = false;
        assert!(initially_enabled(&system, &desc));
        let mut disabled = desc.clone();
        disabled.disabled = true;
        assert!(!initially_enabled(&system, &disabled));
    }

    #[test]
    fn color_scale_interpolates_and_repeats() {
        let scale = vec![(0.0, [0, 0, 0, 0]), (1.0, [100, 200, 50, 0])];
        assert_eq!(sample_color(&scale, 0.5, 0.0), [50, 100, 25, 255]);
        assert_eq!(sample_color(&scale, 0.0, 0.0), [0, 0, 0, 255]);
        assert_eq!(sample_color(&scale, 1.0, 0.0), [100, 200, 50, 255]);
        // Two repeats: t=0.25 is the middle of the first repetition; t=0.5 restarts at the first
        // key of the second repetition.
        assert_eq!(sample_color(&scale, 0.25, 2.0), [50, 100, 25, 255]);
        assert_eq!(sample_color(&scale, 0.5, 2.0), [0, 0, 0, 255]);
        // Empty scale falls back to opaque white.
        assert_eq!(sample_color(&[], 0.3, 0.0), [255, 255, 255, 255]);
        // Out-of-range relative time is clamped.
        assert_eq!(sample_color(&scale, 2.0, 1.0), [100, 200, 50, 255]);
    }

    #[test]
    fn size_scale_interpolates_and_handles_unsorted_key_times() {
        let scale = vec![(0.0, 0.5), (0.4, 2.0), (1.0, 1.0)];
        assert!((sample_size(&scale, 0.4, 0.0) - 2.0).abs() < 1e-5);
        assert!((sample_size(&scale, 0.0, 0.0) - 0.5).abs() < 1e-5);
        assert!((sample_size(&scale, 1.0, 0.0) - 1.0).abs() < 1e-5);
        assert!((sample_size(&[], 0.5, 0.0) - 1.0).abs() < 1e-5);
        // A degenerate single-key scale never divides by zero.
        assert!((sample_size(&[(0.3, 2.0)], 0.9, 0.0) - 2.0).abs() < 1e-6);
    }

    fn simple_desc() -> EmitterDesc {
        decode_emitter(
            "SpriteEmitter",
            "test",
            &values(vec![
                ("maxparticles", ParticleValue::Int(10)),
                ("lifetimerange", ParticleValue::Range([1.0, 1.0])),
                ("initialparticlespersecond", ParticleValue::Float(20.0)),
                ("automaticinitialspawning", ParticleValue::Bool(false)),
                (
                    "startsizerange",
                    ParticleValue::RangeVector([[1.0, 1.0]; 3]),
                ),
                (
                    "startvelocityrange",
                    ParticleValue::RangeVector([[-20.0, 20.0], [-20.0, 20.0], [-5.0, 5.0]]),
                ),
                (
                    "startlocationrange",
                    ParticleValue::RangeVector([[-2.0, 2.0], [-2.0, 2.0], [0.0, 1.0]]),
                ),
            ]),
        )
    }

    #[test]
    fn spawn_rate_reaches_but_does_not_exceed_max_particles() {
        let desc = simple_desc();
        let mut sim = EmitterSim::new(0);
        // 20 particles/s for 1 s of 60 Hz steps -> 10 (capped), plus the pool cap.
        for _ in 0..60 {
            sim.step(&desc, 1.0 / 60.0);
        }
        assert!(
            sim.active() <= desc.max_particles,
            "active {} > max {}",
            sim.active(),
            desc.max_particles
        );
        assert!(sim.active() >= 9, "expected the rate to fill the pool");
    }

    #[test]
    fn no_respawn_particles_expire_and_do_not_restart() {
        let mut desc = simple_desc();
        desc.respawn_dead_particles = false;
        desc.lifetime = [0.5, 0.5];
        desc.max_particles = 3;
        desc.initial_particles_per_second = 100.0;
        let mut sim = EmitterSim::new(0);
        for _ in 0..60 {
            sim.step(&desc, 1.0 / 60.0);
        }
        // After 1 s every 0.5 s particle is dead and no respawn happens.
        assert_eq!(sim.active(), 0, "particles: {:?}", sim.particles);
    }

    #[test]
    fn zero_max_particles_spawns_nothing_without_panicking() {
        let mut desc = simple_desc();
        desc.max_particles = 0;
        let mut sim = EmitterSim::new(0);
        for _ in 0..30 {
            sim.step(&desc, 0.1);
        }
        assert_eq!(sim.active(), 0);
    }

    #[test]
    fn zero_and_negative_dt_are_ignored() {
        let desc = simple_desc();
        let mut sim = EmitterSim::new(0);
        sim.step(&desc, 0.0);
        sim.step(&desc, -1.0);
        sim.step(&desc, f32::NAN);
        assert_eq!(sim.active(), 0);
    }

    #[test]
    fn simulation_is_deterministic_for_the_same_seed() {
        let desc = simple_desc();
        let mut a = EmitterSim::new(7);
        let mut b = EmitterSim::new(7);
        for _ in 0..120 {
            a.step(&desc, 1.0 / 60.0);
            b.step(&desc, 1.0 / 60.0);
        }
        let pa: Vec<_> = a.particles.iter().map(|p| p.position).collect();
        let pb: Vec<_> = b.particles.iter().map(|p| p.position).collect();
        assert_eq!(pa, pb);
        // Different seeds decorrelate.
        let mut c = EmitterSim::new(8);
        for _ in 0..120 {
            c.step(&desc, 1.0 / 60.0);
        }
        let pc: Vec<_> = c.particles.iter().map(|p| p.position).collect();
        assert_ne!(pa, pc);
    }

    #[test]
    fn fade_factors_start_at_reduced_and_end_at_zero() {
        let mut desc = simple_desc();
        desc.fade_in = true;
        desc.fade_in_end_time = 1.0;
        desc.fade_in_factor = [1.0, 1.0, 1.0, 1.0];
        desc.fade_out = true;
        desc.fade_out_start_time = 0.5;
        desc.fade_out_factor = [1.0, 1.0, 1.0, 1.0];
        desc.lifetime = [1.0, 1.0];
        assert_eq!(fade_factors(&desc, 0.0), [0.0, 0.0, 0.0, 0.0]);
        let mid = fade_factors(&desc, 0.5);
        assert!((mid[0] - 0.5).abs() < 1e-6, "{mid:?}");
        // At the end of the lifetime the fade-out has taken it to zero.
        assert_eq!(fade_factors(&desc, 1.0), [0.0, 0.0, 0.0, 0.0]);
    }

    #[test]
    fn subdivision_blend_is_monotonic_and_clamped() {
        let mut desc = simple_desc();
        desc.subdivisions_u = 4;
        desc.subdivisions_v = 1;
        desc.subdivision_start = 0;
        desc.subdivision_end = 3;
        desc.blend_between_subdivisions = true;
        assert_eq!(subdivision_frame(&desc, 0.0), 0);
        assert_eq!(subdivision_frame(&desc, 1.0), 3);
        // Out-of-range endpoints cannot exceed the atlas frame count.
        desc.subdivision_end = 99;
        assert!(subdivision_frame(&desc, 1.0) < 4);
    }
}

/// Opt-in corpus survey (`XIII_GOG_DIR`); prints SKIPPED otherwise. Prints per-map emitter
/// actor classes, sub-emitter classes and the properties in use, plus campaign-wide totals.
#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::PackageCache;

    /// Campaign maps (single-player) surveyed. Multiplayer `DM_*`/`CTF_*` maps are excluded.
    const CAMPAIGN: &[&str] = &[
        "Plage00", "Plage01", "Banque01", "Bateau01", "Base01", "Amos01", "Hual01a", "Hual01b",
        "Hual02", "Hual04a", "Hual04c", "Kello01a", "Kello01b", "Palace01", "PRock01a", "PRock03",
        "PRock04a", "PRock04b", "SMarin01", "SMarin02", "SPADS01", "SPADS02a", "SPADS02b",
        "SSH101a", "SSH101b", "SSH101c", "SSH102b", "Sanc01", "Sanc02a", "Sanc02b", "Sanc03",
        "Toits01", "USA01", "USA02",
    ];

    fn gog_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        if path.is_relative() {
            Some(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../..")
                    .join(path),
            )
        } else {
            Some(path)
        }
    }

    #[test]
    fn opt_in_emitter_survey() {
        let Some(root) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut cache = PackageCache::open(&root).expect("open install");
        let mut actors: BTreeMap<String, usize> = BTreeMap::new();
        let mut subs: BTreeMap<String, usize> = BTreeMap::new();
        let mut props: BTreeMap<String, usize> = BTreeMap::new();
        let mut unsupported: BTreeMap<String, usize> = BTreeMap::new();
        let mut unsupported_classes: BTreeMap<String, usize> = BTreeMap::new();
        let mut initially_active: BTreeMap<String, usize> = BTreeMap::new();
        let mut initially_inactive: BTreeMap<String, usize> = BTreeMap::new();
        let mut failures: BTreeMap<String, usize> = BTreeMap::new();
        let mut with_emitters = 0usize;
        let mut total_systems = 0usize;
        let mut total_subs = 0usize;
        for map in CAMPAIGN {
            let survey = match survey_map(&mut cache, map) {
                Ok(s) => s,
                Err(e) => {
                    panic!("survey {map}: {e}");
                }
            };
            if survey.sub_emitter_total > 0 {
                with_emitters += 1;
            }
            total_systems += survey.systems;
            total_subs += survey.sub_emitter_total;
            println!(
                "[particles] {map}: systems {} sub-emitters {} actors {:?} subclasses {:?}",
                survey.systems, survey.sub_emitter_total, survey.actors, survey.sub_emitters
            );
            if !survey.unsupported_properties.is_empty() {
                println!(
                    "[particles]   {map} unsupported properties: {:?}",
                    survey.unsupported_properties
                );
            }
            for (k, v) in survey.actors {
                *actors.entry(k).or_default() += v;
            }
            for (k, v) in survey.sub_emitters {
                *subs.entry(k).or_default() += v;
            }
            for (k, v) in survey.properties {
                *props.entry(k).or_default() += v;
            }
            for (k, v) in survey.unsupported_properties {
                *unsupported.entry(k).or_default() += v;
            }
            for (k, v) in survey.unsupported_classes {
                *unsupported_classes.entry(k).or_default() += v;
            }
            for (k, v) in survey.initially_active {
                *initially_active.entry(k).or_default() += v;
            }
            for (k, v) in survey.initially_inactive {
                *initially_inactive.entry(k).or_default() += v;
            }
            for (k, v) in survey.failures {
                *failures.entry(k).or_default() += v;
            }
        }
        println!(
            "[particles] campaign: {} maps ({} with emitters), {} systems, {} sub-emitters",
            CAMPAIGN.len(),
            with_emitters,
            total_systems,
            total_subs
        );
        println!("[particles] actor classes: {actors:?}");
        println!("[particles] sub-emitter classes: {subs:?}");
        println!("[particles] properties in use: {props:?}");
        println!("[particles] unsupported properties: {unsupported:?}");
        println!("[particles] unsupported classes: {unsupported_classes:?}");
        println!("[particles] initially active per class: {initially_active:?}");
        println!("[particles] initially inactive per class: {initially_inactive:?}");
        println!("[particles] failures: {failures:?}");

        // Structural facts measured from the corpus.
        assert!(
            with_emitters >= 30,
            "expected emitters in most campaign maps"
        );
        assert!(actors.contains_key("Emitter"), "Engine.Emitter must occur");
        assert!(
            subs.contains_key("SpriteEmitter") && subs.contains_key("MeshEmitter"),
            "sprite and mesh sub-emitters must occur"
        );
        for f in [
            "maxparticles",
            "lifetimerange",
            "startvelocityrange",
            "drawstyle",
        ] {
            assert!(props.contains_key(f), "property {f} must be in use");
        }
        assert!(
            failures.is_empty(),
            "particle import failures in the campaign: {failures:?}"
        );
    }

    /// Opt-in: for each campaign map, the emitter system nearest the PlayerStart (used to pick
    /// a screenshot viewpoint where the default view shows an effect).
    #[test]
    fn opt_in_nearest_effect_to_player_start() {
        let Some(root) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut cache = PackageCache::open(&root).expect("open install");
        for map in CAMPAIGN {
            let scene = match crate::import_map(&mut cache, map) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Some((ps, _)) = scene.player_start else {
                continue;
            };
            let mut best: Option<(f32, &ParticleSystem, &EmitterDesc)> = None;
            // Second pick: the non-triggered, initially-active system closest to the PlayerStart
            // view direction (used to choose the required `effects.png` map/actor).
            let mut best_front: Option<(f32, f32, &ParticleSystem, &EmitterDesc)> = None;
            for s in &scene.particle_systems {
                for e in &s.emitters {
                    let t = s.transform.translation;
                    let d =
                        ((t[0] - ps[0]).powi(2) + (t[1] - ps[1]).powi(2) + (t[2] - ps[2]).powi(2))
                            .sqrt();
                    if best.as_ref().is_none_or(|(bd, _, _)| d < *bd) {
                        best = Some((d, s, e));
                    }
                    if !s.triggered && initially_enabled(s, e) {
                        let dir = [t[0] - ps[0], t[1] - ps[1], t[2] - ps[2]];
                        let len = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
                        if len < 1e-3 || d < 2.0 {
                            continue;
                        }
                        let (yaw, pitch) =
                            crate::rotator_to_yaw_pitch(scene.player_start.unwrap().1);
                        let fwd = [
                            -yaw.sin() * pitch.cos(),
                            pitch.sin(),
                            -yaw.cos() * pitch.cos(),
                        ];
                        let dot = (fwd[0] * dir[0] + fwd[1] * dir[1] + fwd[2] * dir[2]) / len;
                        let angle = dot.clamp(-1.0, 1.0).acos().to_degrees();
                        if best_front.as_ref().is_none_or(|(ba, _, _, _)| angle < *ba) {
                            best_front = Some((angle, d, s, e));
                        }
                    }
                }
            }
            if let Some((angle, d, s, e)) = best_front
                && angle < 35.0
            {
                println!(
                    "[front] {map}: angle {:.0} deg {:.1} m {} {} style {} size {:?} tex {}",
                    angle,
                    d,
                    s.path,
                    e.class,
                    e.draw_style,
                    e.start_size[0],
                    e.texture
                        .map(|i| scene.textures[i].label.clone())
                        .unwrap_or_default()
                );
            }
            if let Some((d, s, e)) = best {
                let size = e.start_size[0];
                let tex = e
                    .texture
                    .map(|i| scene.textures[i].label.clone())
                    .unwrap_or_default();
                println!(
                    "[nearest] {map}: {:.1} m {} {} style {} size {:?} tex {} at ({:.1},{:.1},{:.1})",
                    d,
                    s.path,
                    e.class,
                    e.draw_style,
                    size,
                    tex,
                    s.transform.translation[0],
                    s.transform.translation[1],
                    s.transform.translation[2]
                );
            }
        }
    }

    /// Opt-in detail dump of one map's emitter systems (positions and sub-emitters), used to
    /// choose screenshot viewpoints. Reads `XIII_PARTICLE_MAP` (default `Plage01`).
    #[test]
    fn opt_in_emitter_details() {
        let Some(root) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let map = std::env::var("XIII_PARTICLE_MAP").unwrap_or_else(|_| "Plage01".to_owned());
        let mut cache = PackageCache::open(&root).expect("open install");
        let scene = crate::import_map(&mut cache, &map).expect("import");
        if let Some((p, rot)) = scene.player_start {
            println!(
                "[particles] {map} player start bevy ({:.1},{:.1},{:.1}) rot {rot:?}",
                p[0], p[1], p[2]
            );
        }
        for s in &scene.particle_systems {
            let t = &s.transform.translation;
            println!(
                "[particles] {map} {} at bevy ({:.1},{:.1},{:.1}) m, {} sub-emitters, triggered {} initially_active {}",
                s.path,
                t[0],
                t[1],
                t[2],
                s.emitters.len(),
                s.triggered,
                s.initially_active
            );
            for e in &s.emitters {
                let tex = e
                    .texture
                    .map(|i| scene.textures[i].label.clone())
                    .unwrap_or_else(|| e.mesh_ref.map(|_| "<mesh>".to_owned()).unwrap_or_default());
                println!(
                    "[particles]   {} {} max {} style {} lifetime {:?} tex {}",
                    e.class, e.name, e.max_particles, e.draw_style, e.lifetime, tex
                );
            }
        }
    }
}
