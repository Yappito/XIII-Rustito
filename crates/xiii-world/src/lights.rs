//! UE2 dynamic light actors (`Engine.Light` subclasses) placed in maps.
//!
//! XIII's lights are ordinary `Engine.Light` (or subclass) actors whose tagged properties carry
//! the UE2 HSB colour and radius fields. Most map lights are `bStatic=true` and baked into the
//! static-mesh vertex colours / lightmaps, so the renderer must **not** draw them again or the
//! baked lighting is double-counted. This module therefore selects only the lights the original
//! engine treats as runtime-dynamic:
//!
//! - `Engine.Light` itself and `Gameplay.Sunlight`/`Gameplay.Spotlight` default to `bStatic=true`
//!   and are skipped unless the map turns them dynamic or gives them a time-varying `LightType`
//!   (pulse/blink/flicker/strobe/subtle-pulse/fade-out);
//! - `Gameplay.TriggerLight`, `XIDCine.ScriptedLight` and `XIDCine.MovableLight` default to
//!   `bStatic=false` (measured `xiii-tool script disasm`) and are always included;
//! - `XIII.MuzzleLight` (runtime-spawned by the Beretta) defaults to `bStatic=false` and is
//!   included by the `--play` VM sync.
//!
//! Evidence conventions:
//!
//! - `ELightType`/`ELightEffect` orders are the UT2003 `Actor` enums
//!   (`beyondunrealwiki.github.io` Actor/Enums page, last edited 2003-10-04), matching the
//!   observed class defaults: `Gameplay.Spotlight.LightEffect = 8` (`LE_StaticSpot`) and
//!   `Gameplay.Sunlight.LightEffect = 20` (`LE_Sunlight`).
//! - `LightSaturation` is inverted relative to HSV: 0 is the pure hue and 255 is white
//!   (UnrealWiki Actor/Lighting: "the higher the value, the less rich the colour becomes"), and
//!   the wiki's `Chroma2RGB` routine is reproduced here. The exact retail integer conversion is
//!   not decoded from the DLL, so the mapping is labelled a **hypothesis**.
//! - `LightRadius` is not Unreal units; the legacy reference measures a factor of about 27
//!   between the byte and the reach in UU (UnrealWiki Actor/Lighting). That factor is used and
//!   labelled an estimate.
//!
//! Nothing is dropped silently: every light class and property in use is counted (`light.*`),
//! and unsupported properties are listed by name.

use std::collections::BTreeMap;
use std::sync::Arc;

use xiii_decode::common::{
    BevyTransform, Props, UNREAL_UNITS_PER_METER, to_bevy_position, to_bevy_scale,
};
use xiii_decode::model::level::ActorPlacement;
use xiii_package::{Limits, ObjectRef, Package, PropertyValue, StructValue};
use xiii_script::Value;
use xiii_script::vm::ClassLayout;

use crate::{ClassDefaults, Importer, Loaded, placement_transform};

/// `ELightType` (UT2003 `Actor` enum order). Values are the raw tagged byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LightType {
    /// `LT_None`: the light is off.
    None,
    /// `LT_Steady`: constant brightness.
    Steady,
    /// `LT_Pulse`: smooth on/off.
    Pulse,
    /// `LT_Blink`: square on/off.
    Blink,
    /// `LT_Flicker`: irregular flicker.
    Flicker,
    /// `LT_Strobe`: fast on/off.
    Strobe,
    /// `LT_BackdropLight`: backdrop-only (no world lighting).
    BackdropLight,
    /// `LT_SubtlePulse`: slow pulse.
    SubtlePulse,
    /// `LT_TexturePaletteOnce`.
    TexturePaletteOnce,
    /// `LT_TexturePaletteLoop`.
    TexturePaletteLoop,
    /// `LT_FadeOut`: fade to black.
    FadeOut,
    /// A value above the decoded enum range (kept, never silently remapped).
    Unknown(u8),
}

impl LightType {
    /// Decodes the raw byte.
    pub fn from_byte(b: u8) -> Self {
        match b {
            0 => Self::None,
            1 => Self::Steady,
            2 => Self::Pulse,
            3 => Self::Blink,
            4 => Self::Flicker,
            5 => Self::Strobe,
            6 => Self::BackdropLight,
            7 => Self::SubtlePulse,
            8 => Self::TexturePaletteOnce,
            9 => Self::TexturePaletteLoop,
            10 => Self::FadeOut,
            other => Self::Unknown(other),
        }
    }

    /// Stable lowercase name used in counters and the survey.
    pub fn name(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Steady => "steady",
            Self::Pulse => "pulse",
            Self::Blink => "blink",
            Self::Flicker => "flicker",
            Self::Strobe => "strobe",
            Self::BackdropLight => "backdrop",
            Self::SubtlePulse => "subtle_pulse",
            Self::TexturePaletteOnce => "palette_once",
            Self::TexturePaletteLoop => "palette_loop",
            Self::FadeOut => "fade_out",
            Self::Unknown(_) => "unknown",
        }
    }

    /// Whether the type varies with time (candidate for a non-baked light).
    pub fn is_time_varying(self) -> bool {
        matches!(
            self,
            Self::Pulse
                | Self::Blink
                | Self::Flicker
                | Self::Strobe
                | Self::SubtlePulse
                | Self::FadeOut
        )
    }
}

/// `ELightEffect` (UT2003 `Actor` enum order).
pub fn light_effect_name(value: u8) -> &'static str {
    const NAMES: [&str; 22] = [
        "none",
        "torch_waver",
        "fire_waver",
        "watery_shimmer",
        "searchlight",
        "slow_wave",
        "fast_wave",
        "cloud_cast",
        "static_spot",
        "shock",
        "disco",
        "warp",
        "spotlight",
        "non_incidence",
        "shell",
        "omni_bump_map",
        "interference",
        "cylinder",
        "rotor",
        "negative",
        "sunlight",
        "quadratic_non_incidence",
    ];
    NAMES.get(value as usize).copied().unwrap_or("unknown")
}

/// Converts a UE2 light colour (`LightHue`, `LightSaturation`, `LightBrightness`) to linear-ish
/// RGB in `0..=1`.
///
/// Implements the UnrealWiki `Chroma2RGB` model: the hue byte maps red(0) -> green(85) ->
/// blue(170) -> red(255); the saturation byte mixes toward white (0 = pure colour, 255 = white);
/// the brightness byte scales the result. The engine's own integer routine is not decoded, so
/// this is the documented reference model (**hypothesis** for exact retail output).
pub fn hsb_to_rgb(hue: u8, saturation: u8, brightness: f32) -> [f32; 3] {
    let h = hue as f32 / 85.0;
    let (mut r, mut g, mut b) = if h <= 1.0 {
        (1.0 - h, h, 0.0)
    } else if h <= 2.0 {
        (0.0, 1.0 - (h - 1.0), h - 1.0)
    } else {
        (h - 2.0, 0.0, 1.0 - (h - 2.0))
    };
    let s = (saturation as f32 / 255.0).clamp(0.0, 1.0);
    r += s * (1.0 - r);
    g += s * (1.0 - g);
    b += s * (1.0 - b);
    let v = (brightness / 255.0).clamp(0.0, 1.0);
    [r * v, g * v, b * v]
}

/// Maximum luminous power (lumens) used for a `LightBrightness` of 255.
///
/// Bevy's `PointLight.intensity` is luminous power in lumens. UE2 has no photometric unit for
/// `LightBrightness`; this constant is a diagnostic calibration so a normal map light is visible
/// on the unlit baked surfaces, **not** a decoded value. Brightness is applied quadratically
/// (a hypothesis: perceived light output is super-linear in the 0..255 byte).
pub const LIGHT_MAX_LUMENS: f32 = 250_000.0;

/// `LightBrightness` byte to Bevy luminous power (lumens).
pub fn brightness_to_lumens(brightness: f32) -> f32 {
    let v = (brightness / 255.0).clamp(0.0, 1.0);
    LIGHT_MAX_LUMENS * v * v
}

/// Estimated Unreal units of reach per `LightRadius` unit (legacy reference: "about 27").
pub const UNREAL_LIGHT_RADIUS_TO_UU: f32 = 27.0;

/// `LightRadius` byte to a Bevy light cutoff range in metres, clamped to a sane diagnostic range.
pub fn radius_to_meters(radius: u8) -> f32 {
    (radius as f32 * UNREAL_LIGHT_RADIUS_TO_UU / UNREAL_UNITS_PER_METER).clamp(1.0, 200.0)
}

/// Time-varying brightness multiplier in `0..=1` for a pulse/blink/flicker/strobe type.
///
/// `period` and `phase` are the raw `LightPeriod`/`LightPhase` bytes. The mapping of those bytes
/// to seconds is **not** decoded from the engine; a period of 32 (the class default) is treated as
/// one second and other values scale linearly, which reproduces the observed flicker intent but
/// not the retail timing (**hypothesis**).
pub fn type_brightness_scale(light_type: LightType, period: u8, phase: u8, time: f32) -> f32 {
    if !time.is_finite() {
        return 1.0;
    }
    let period_s = if period == 0 {
        1.0
    } else {
        period as f32 / 32.0
    };
    let phase01 = phase as f32 / 255.0;
    let t = time / period_s + phase01;
    match light_type {
        LightType::None => 0.0,
        LightType::Steady | LightType::BackdropLight => 1.0,
        LightType::Pulse | LightType::SubtlePulse => 0.5 + 0.5 * (std::f32::consts::TAU * t).sin(),
        LightType::Blink | LightType::Strobe => {
            if t.rem_euclid(1.0) < 0.5 {
                1.0
            } else {
                0.0
            }
        }
        // A cheap deterministic hash in place of the engine's native random flicker.
        LightType::Flicker => {
            let x = (t * 2654435761.0).sin() * 43758.547;
            0.35 + 0.65 * x.rem_euclid(1.0)
        }
        LightType::FadeOut => (1.0 - t.rem_euclid(1.0)).clamp(0.0, 1.0),
        LightType::TexturePaletteOnce | LightType::TexturePaletteLoop | LightType::Unknown(_) => {
            1.0
        }
    }
}

/// One decoded map light that the renderer should draw at runtime.
#[derive(Debug, Clone)]
pub struct SceneLight {
    /// Actor object path.
    pub path: String,
    /// Effective placement in Bevy space.
    pub transform: BevyTransform,
    /// Raw actor `Location` in Unreal units (for diagnostics).
    pub location_uu: [f32; 3],
    /// `LightType`.
    pub light_type: LightType,
    /// `LightEffect` byte.
    pub effect: u8,
    /// `LightBrightness` (map value, else class default).
    pub brightness: f32,
    /// `LightHue`.
    pub hue: u8,
    /// `LightSaturation`.
    pub saturation: u8,
    /// `LightRadius`.
    pub radius: u8,
    /// `LightPeriod`.
    pub period: u8,
    /// `LightPhase`.
    pub phase: u8,
    /// `LightCone`.
    pub cone: u8,
    /// `bDynamicLight`.
    pub dynamic: bool,
    /// `bStatic` (true = baked into the static geometry).
    pub b_static: bool,
    /// `bActorLight`.
    pub actor_light: bool,
    /// `bHidden`.
    pub hidden: bool,
}

impl SceneLight {
    /// Colour in linear-ish RGB (`0..=1`) at relative time `t` seconds, including the
    /// type-based brightness modulation.
    pub fn color_at(&self, time: f32) -> [f32; 3] {
        let scale = type_brightness_scale(self.light_type, self.period, self.phase, time);
        let c = hsb_to_rgb(self.hue, self.saturation, self.brightness);
        [c[0] * scale, c[1] * scale, c[2] * scale]
    }

    /// Effective luminous power at relative time `t` seconds.
    pub fn lumens_at(&self, time: f32) -> f32 {
        brightness_to_lumens(self.brightness)
            * type_brightness_scale(self.light_type, self.period, self.phase, time)
    }

    /// Cutoff range in metres.
    pub fn range_m(&self) -> f32 {
        radius_to_meters(self.radius)
    }

    /// Whether the light contributes light at all (its type is not `LT_None`).
    pub fn emits(&self) -> bool {
        self.light_type != LightType::None
    }

    /// Whether the renderer should draw this map light at runtime: an emitting light that is not
    /// purely baked. `bDynamicLight`, a non-static class (`ScriptedLight`/`TriggerLight`/
    /// `MovableLight`) or a time-varying `LightType` all qualify; a steady `bStatic` light does
    /// not (it is already in the baked geometry).
    pub fn render_dynamic(&self) -> bool {
        self.emits() && (self.dynamic || !self.b_static || self.light_type.is_time_varying())
    }
}

/// Effective light-property values of an actor.
#[derive(Debug, Clone, Copy, Default)]
pub struct LightValues {
    /// `LightType`.
    pub light_type: u8,
    /// `LightEffect`.
    pub effect: u8,
    /// `LightBrightness`.
    pub brightness: f32,
    /// `LightHue`.
    pub hue: u8,
    /// `LightSaturation`.
    pub saturation: u8,
    /// `LightRadius`.
    pub radius: u8,
    /// `LightPeriod`.
    pub period: u8,
    /// `LightPhase`.
    pub phase: u8,
    /// `LightCone`.
    pub cone: u8,
    /// `bDynamicLight`.
    pub dynamic: bool,
    /// `bStatic`.
    pub b_static: bool,
    /// `bActorLight`.
    pub actor_light: bool,
    /// `bHidden`.
    pub hidden: bool,
}

/// Documented `Engine.Actor`/`Engine.Light` defaults used when the map and class chain omit a
/// property (`Engine.Light` measured with `xiii-tool script defaults`: `LightType=1`,
/// `LightBrightness=150`, `LightSaturation=255`, `LightRadius=64`, `LightPeriod=32`,
/// `LightCone=128`; `bStatic=true`).
fn documented_default(name: &str) -> Option<LightScalar> {
    Some(match name {
        "lighttype" => LightScalar::Byte(1),
        "lighteffect" | "lightphase" => LightScalar::Byte(0),
        "lightbrightness" => LightScalar::Float(150.0),
        "lightsaturation" => LightScalar::Byte(255),
        "lightradius" => LightScalar::Byte(64),
        "lightperiod" => LightScalar::Byte(32),
        "lightcone" => LightScalar::Byte(128),
        "bstatic" => LightScalar::Bool(true),
        _ => return None,
    })
}

/// A scalar read from a map block, a class default or a documented default.
#[derive(Debug, Clone, Copy, PartialEq)]
enum LightScalar {
    Float(f32),
    Int(i32),
    Byte(u8),
    Bool(bool),
}

impl LightScalar {
    fn as_f32(self) -> Option<f32> {
        match self {
            Self::Float(v) => Some(v),
            Self::Int(v) => Some(v as f32),
            Self::Byte(v) => Some(v as f32),
            Self::Bool(_) => None,
        }
    }

    fn as_u8(self) -> Option<u8> {
        match self {
            Self::Byte(v) => Some(v),
            Self::Int(v) => u8::try_from(v).ok(),
            Self::Float(v) => Some(v.clamp(0.0, 255.0) as u8),
            Self::Bool(_) => None,
        }
    }

    fn as_bool(self) -> Option<bool> {
        match self {
            Self::Bool(v) => Some(v),
            _ => None,
        }
    }
}

fn from_property(v: &PropertyValue) -> Option<LightScalar> {
    Some(match v {
        PropertyValue::Float(x) => LightScalar::Float(*x),
        PropertyValue::Int(i) => LightScalar::Int(*i),
        PropertyValue::Byte(b) => LightScalar::Byte(*b),
        PropertyValue::Bool(b) => LightScalar::Bool(*b),
        _ => return None,
    })
}

fn from_script(v: &Value) -> Option<LightScalar> {
    Some(match v {
        Value::Float(x) => LightScalar::Float(*x),
        Value::Int(i) => LightScalar::Int(*i),
        Value::Byte(b) => LightScalar::Byte(*b),
        Value::Bool(b) => LightScalar::Bool(*b),
        _ => return None,
    })
}

/// Reads one property: the map's tagged value first, then the inherited class default, then the
/// documented default.
fn effective_scalar(
    package: &Package,
    props: &xiii_package::ObjectProperties,
    layout: &ClassLayout,
    name: &str,
) -> Option<LightScalar> {
    let p = Props::new(package, props);
    if let Some(v) = p.get(name).and_then(|p| from_property(&p.value)) {
        return Some(v);
    }
    if let Some(slot) = layout.slot_by_name(name)
        && let Some(default) = layout.defaults.get(slot.base)
        && let Some(v) = from_script(default)
    {
        return Some(v);
    }
    documented_default(name)
}

fn eff_f32(
    package: &Package,
    props: &xiii_package::ObjectProperties,
    layout: &ClassLayout,
    name: &str,
    default: f32,
) -> f32 {
    effective_scalar(package, props, layout, name)
        .and_then(LightScalar::as_f32)
        .unwrap_or(default)
}

fn eff_u8(
    package: &Package,
    props: &xiii_package::ObjectProperties,
    layout: &ClassLayout,
    name: &str,
    default: u8,
) -> u8 {
    effective_scalar(package, props, layout, name)
        .and_then(LightScalar::as_u8)
        .unwrap_or(default)
}

fn eff_bool(
    package: &Package,
    props: &xiii_package::ObjectProperties,
    layout: &ClassLayout,
    name: &str,
    default: bool,
) -> bool {
    effective_scalar(package, props, layout, name)
        .and_then(LightScalar::as_bool)
        .unwrap_or(default)
}

/// Reads the `LightValues` of an actor from its tagged block plus inherited class defaults.
pub fn decode_light_values(
    package: &Package,
    props: &xiii_package::ObjectProperties,
    layout: &ClassLayout,
) -> LightValues {
    LightValues {
        light_type: eff_u8(package, props, layout, "LightType", 1),
        effect: eff_u8(package, props, layout, "LightEffect", 0),
        brightness: eff_f32(package, props, layout, "LightBrightness", 150.0),
        hue: eff_u8(package, props, layout, "LightHue", 0),
        saturation: eff_u8(package, props, layout, "LightSaturation", 255),
        radius: eff_u8(package, props, layout, "LightRadius", 64),
        period: eff_u8(package, props, layout, "LightPeriod", 32),
        phase: eff_u8(package, props, layout, "LightPhase", 0),
        cone: eff_u8(package, props, layout, "LightCone", 128),
        dynamic: eff_bool(package, props, layout, "bDynamicLight", false),
        b_static: eff_bool(package, props, layout, "bStatic", true),
        actor_light: eff_bool(package, props, layout, "bActorLight", false),
        hidden: eff_bool(package, props, layout, "bHidden", false),
    }
}

/// Placement fields of a light actor, read from the map block (class/engine fallbacks are applied
/// by [`ClassDefaults::resolve`]).
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

/// Decodes every map light actor and appends the drawable ones to `im.scene.lights`. Called by
/// [`crate::import_map`]. All light classes and properties are counted; unsupported properties
/// are listed.
pub(crate) fn import_lights(
    im: &mut Importer<'_>,
    map_pkg: &Arc<Loaded>,
    defaults: &mut ClassDefaults,
) {
    let package = &map_pkg.package;
    for i in 0..package.exports().len() {
        if package.exports()[i].serial_size == 0 {
            continue;
        }
        let Some(class) = package.export_class_path(i) else {
            continue;
        };
        let short = class.rsplit('.').next().unwrap_or("").to_owned();
        let Ok(layout) = defaults.layout(class) else {
            continue;
        };
        if !layout.chain_names.iter().any(|n| n == "light") {
            continue;
        }
        let props = match package.read_object_properties(&map_pkg.data, i, &Limits::default()) {
            Ok(p) => p,
            Err(e) => {
                im.scene.fail(
                    "fail.light.properties",
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
        for prop in &props.block.properties {
            let name = package.property_name(prop).to_ascii_lowercase();
            im.scene.count(&format!("light.property.{name}"), 1);
        }
        let values = decode_light_values(package, &props, &layout);
        im.scene.count(&format!("light.actor.{short}"), 1);
        im.scene.count(
            &format!(
                "light.type.{}",
                LightType::from_byte(values.light_type).name()
            ),
            1,
        );
        im.scene.count(
            &format!("light.effect.{}", light_effect_name(values.effect)),
            1,
        );
        if values.b_static {
            im.scene.count("light.static", 1);
        } else {
            im.scene.count("light.non_static", 1);
        }
        if values.dynamic {
            im.scene.count("light.dynamic_flag", 1);
        }
        // Effective placement: map property, else inherited class default, else engine default.
        let p = Props::new(package, &props);
        let placement = actor_placement(package, i, &p);
        let (effective, _) = match defaults.resolve(&placement.class, &placement) {
            Ok(v) => v,
            Err(e) => {
                im.scene
                    .fail("fail.light.placement", format!("{}: {e}", placement.path));
                continue;
            }
        };
        let light_type = LightType::from_byte(values.light_type);
        let scene_light = SceneLight {
            path: placement.path.clone(),
            transform: placement_transform(&effective),
            location_uu: effective.location,
            light_type,
            effect: values.effect,
            brightness: values.brightness,
            hue: values.hue,
            saturation: values.saturation,
            radius: values.radius,
            period: values.period,
            phase: values.phase,
            cone: values.cone,
            dynamic: values.dynamic,
            b_static: values.b_static,
            actor_light: values.actor_light,
            hidden: values.hidden,
        };
        let render = scene_light.render_dynamic();
        if render {
            im.scene.count("light.render_dynamic", 1);
        } else {
            im.scene.count("light.baked_or_off", 1);
        }
        im.scene.lights.push(scene_light);
    }
}

/// Per-map light survey used by the opt-in corpus tests.
#[derive(Debug, Clone, Default)]
pub struct LightSurvey {
    /// Light actor class -> count.
    pub classes: BTreeMap<String, usize>,
    /// `LightType` name -> count.
    pub types: BTreeMap<String, usize>,
    /// `LightEffect` name -> count.
    pub effects: BTreeMap<String, usize>,
    /// Property name -> count of lights that serialize it.
    pub properties: BTreeMap<String, usize>,
    /// Number of lights drawn at runtime.
    pub render_dynamic: usize,
    /// Number of lights skipped as baked/off.
    pub baked_or_off: usize,
    /// `fail.light.*` counters.
    pub failures: BTreeMap<String, usize>,
}

impl LightSurvey {
    /// Extracts the survey from a scene's `light.*` counters.
    pub fn from_scene(scene: &crate::WorldScene) -> Self {
        let mut out = Self::default();
        for (key, value) in &scene.counters {
            if key.starts_with("fail.light") {
                out.failures.insert(key.clone(), *value);
            }
            let Some(rest) = key.strip_prefix("light.") else {
                continue;
            };
            if let Some(class) = rest.strip_prefix("actor.") {
                out.classes.insert(class.to_owned(), *value);
            } else if let Some(t) = rest.strip_prefix("type.") {
                out.types.insert(t.to_owned(), *value);
            } else if let Some(e) = rest.strip_prefix("effect.") {
                out.effects.insert(e.to_owned(), *value);
            } else if let Some(prop) = rest.strip_prefix("property.") {
                out.properties.insert(prop.to_owned(), *value);
            } else if rest == "render_dynamic" {
                out.render_dynamic = *value;
            } else if rest == "baked_or_off" {
                out.baked_or_off = *value;
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn light_type_enum_order_matches_ut2003() {
        assert_eq!(LightType::from_byte(0), LightType::None);
        assert_eq!(LightType::from_byte(1), LightType::Steady);
        assert_eq!(LightType::from_byte(2), LightType::Pulse);
        assert_eq!(LightType::from_byte(4), LightType::Flicker);
        assert_eq!(LightType::from_byte(7), LightType::SubtlePulse);
        assert_eq!(LightType::from_byte(10), LightType::FadeOut);
        // Out-of-range values are preserved, never remapped to a valid light.
        assert_eq!(LightType::from_byte(99), LightType::Unknown(99));
    }

    #[test]
    fn hsb_endpoints() {
        // Pure red at the hue origin / wrap, pure green at 85, pure blue at 170.
        assert_eq!(hsb_to_rgb(0, 0, 255.0), [1.0, 0.0, 0.0]);
        let green = hsb_to_rgb(85, 0, 255.0);
        assert!((green[1] - 1.0).abs() < 1e-6 && green[0].abs() < 1e-6);
        let blue = hsb_to_rgb(170, 0, 255.0);
        assert!((blue[2] - 1.0).abs() < 1e-6 && blue[0].abs() < 1e-6);
        // The hue byte wraps back to red at 255.
        let wrap = hsb_to_rgb(255, 0, 255.0);
        assert!((wrap[0] - 1.0).abs() < 1e-6, "{wrap:?}");
        // Full saturation is white regardless of hue.
        assert_eq!(hsb_to_rgb(0, 255, 255.0), [1.0, 1.0, 1.0]);
    }

    #[test]
    fn hsb_brightness_scales_and_saturation_mixes_white() {
        let pure = hsb_to_rgb(0, 0, 128.0);
        assert!((pure[0] - 128.0 / 255.0).abs() < 1e-6 && pure[1].abs() < 1e-6);
        // Half-desaturated red is red mixed half way to white.
        let mixed = hsb_to_rgb(0, 128, 255.0);
        assert!((mixed[1] - 128.0 / 255.0).abs() < 1e-6, "{mixed:?}");
        assert!((mixed[2] - 128.0 / 255.0).abs() < 1e-6, "{mixed:?}");
        assert!((mixed[0] - 1.0).abs() < 1e-6, "{mixed:?}");
        // Zero brightness is black.
        assert_eq!(hsb_to_rgb(200, 200, 0.0), [0.0, 0.0, 0.0]);
    }

    #[test]
    fn brightness_and_radius_are_monotonic_and_bounded() {
        assert_eq!(brightness_to_lumens(0.0), 0.0);
        assert!(brightness_to_lumens(255.0) > brightness_to_lumens(128.0));
        // Out-of-range input cannot produce a negative or non-finite intensity.
        assert_eq!(brightness_to_lumens(-5.0), 0.0);
        assert!(brightness_to_lumens(1000.0).is_finite());
        assert!(radius_to_meters(0) >= 1.0);
        assert!(radius_to_meters(255) <= 200.0);
        assert!(radius_to_meters(128) > radius_to_meters(64));
    }

    fn sample(light_type: LightType, period: u8, phase: u8) -> SceneLight {
        SceneLight {
            path: "L".into(),
            transform: xiii_decode::common::actor_to_bevy([0.0; 3], [0; 3], [1.0; 3]),
            location_uu: [0.0; 3],
            light_type,
            effect: 0,
            brightness: 255.0,
            hue: 0,
            saturation: 0,
            radius: 64,
            period,
            phase,
            cone: 128,
            dynamic: false,
            b_static: true,
            actor_light: false,
            hidden: false,
        }
    }

    #[test]
    fn pulse_and_blink_stay_in_range_and_start_at_phase() {
        // Pulse with phase 0 starts at 0.5 (mid sine).
        let s = sample(LightType::Pulse, 32, 0);
        let start = type_brightness_scale(s.light_type, s.period, s.phase, 0.0);
        assert!((start - 0.5).abs() < 1e-5, "{start}");
        // Every sampled value is finite and within 0..=1.
        for i in 0..200 {
            let t = i as f32 * 0.037;
            let v = type_brightness_scale(s.light_type, s.period, s.phase, t);
            assert!((0.0..=1.0).contains(&v), "t={t} v={v}");
        }
        // Blink is binary and never NaN on a non-finite time.
        let b = sample(LightType::Blink, 32, 0);
        let v = type_brightness_scale(b.light_type, b.period, b.phase, f32::NAN);
        assert_eq!(v, 1.0);
        // Steady is exactly 1; None is exactly 0.
        assert_eq!(type_brightness_scale(LightType::Steady, 0, 0, 12.0), 1.0);
        assert_eq!(type_brightness_scale(LightType::None, 0, 0, 12.0), 0.0);
    }

    #[test]
    fn render_selection_skips_baked_steady_lights() {
        let mut l = sample(LightType::Steady, 32, 0);
        // Baked steady light: not drawn.
        assert!(!l.render_dynamic());
        // Non-static steady light: drawn.
        l.b_static = false;
        assert!(l.render_dynamic());
        // Dynamic flag: drawn.
        l.b_static = true;
        l.dynamic = true;
        assert!(l.render_dynamic());
        // A baked steady light turned off: never drawn.
        l.dynamic = false;
        l.light_type = LightType::None;
        assert!(!l.render_dynamic());
        // A baked flicker light: drawn (its animation cannot be baked).
        l.light_type = LightType::Flicker;
        assert!(l.render_dynamic());
    }

    #[test]
    fn documented_defaults_are_used_and_unknown_names_return_none() {
        assert_eq!(documented_default("lighttype"), Some(LightScalar::Byte(1)));
        assert_eq!(documented_default("bstatic"), Some(LightScalar::Bool(true)));
        assert_eq!(documented_default("not_a_property"), None);
    }

    #[test]
    fn effect_names_cover_the_enum_and_fall_back() {
        assert_eq!(light_effect_name(0), "none");
        assert_eq!(light_effect_name(8), "static_spot");
        assert_eq!(light_effect_name(20), "sunlight");
        assert_eq!(light_effect_name(200), "unknown");
    }
}

#[cfg(test)]
mod local_tests {
    use super::*;
    use crate::PackageCache;

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
    fn opt_in_light_survey() {
        let Some(root) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut cache = PackageCache::open(&root).expect("open install");
        let mut classes = BTreeMap::new();
        let mut types = BTreeMap::new();
        let mut effects = BTreeMap::new();
        let mut render = 0usize;
        let mut baked = 0usize;
        let mut failures = BTreeMap::new();
        for map in CAMPAIGN {
            let scene = match crate::import_map(&mut cache, map) {
                Ok(s) => s,
                Err(e) => panic!("import {map}: {e}"),
            };
            let survey = LightSurvey::from_scene(&scene);
            render += survey.render_dynamic;
            baked += survey.baked_or_off;
            for (k, v) in survey.classes {
                *classes.entry(k).or_insert(0usize) += v;
            }
            for (k, v) in survey.types {
                *types.entry(k).or_insert(0usize) += v;
            }
            for (k, v) in survey.effects {
                *effects.entry(k).or_insert(0usize) += v;
            }
            for (k, v) in survey.failures {
                *failures.entry(k).or_insert(0usize) += v;
            }
        }
        println!("[lights] campaign classes: {classes:?}");
        println!("[lights] campaign types: {types:?}");
        println!("[lights] campaign effects: {effects:?}");
        println!("[lights] render_dynamic {render}, baked_or_off {baked}");
        println!("[lights] failures: {failures:?}");
        assert!(classes.contains_key("Light"), "Engine.Light must occur");
        assert!(types.contains_key("steady"), "steady lights must occur");
        assert!(
            types.contains_key("flicker") || types.contains_key("pulse"),
            "time-varying lights must occur"
        );
        assert!(render > 0, "some map lights must render dynamically");
        assert!(failures.is_empty(), "light import failures: {failures:?}");
    }

    /// Opt-in: prints the drawable dynamic lights of one map (`XIII_LIGHT_MAP`, default `Hual02`)
    /// with their Bevy positions, used to choose a screenshot viewpoint.
    #[test]
    fn opt_in_light_details() {
        let Some(root) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let map = std::env::var("XIII_LIGHT_MAP").unwrap_or_else(|_| "Hual02".to_owned());
        let mut cache = PackageCache::open(&root).expect("open install");
        let scene = crate::import_map(&mut cache, &map).expect("import");
        if let Some((p, rot)) = scene.player_start {
            println!(
                "[lights] {map} player start bevy ({:.1},{:.1},{:.1}) rot {rot:?}",
                p[0], p[1], p[2]
            );
        }
        for l in &scene.lights {
            if !l.render_dynamic() {
                continue;
            }
            let t = l.transform.translation;
            println!(
                "[lights] {map} {} {} type={} brightness={} hue={} sat={} radius={} period={} phase={} at bevy ({:.1},{:.1},{:.1}) m",
                l.path,
                l.light_effect_name(),
                l.light_type.name(),
                l.brightness,
                l.hue,
                l.saturation,
                l.radius,
                l.period,
                l.phase,
                t[0],
                t[1],
                t[2]
            );
        }
    }
}

impl SceneLight {
    /// Convenience for diagnostics: `LightEffect` name.
    pub fn light_effect_name(&self) -> &'static str {
        light_effect_name(self.effect)
    }

    /// Bevy-space position of the light origin (metres), for diagnostics.
    pub fn bevy_position(&self) -> [f32; 3] {
        to_bevy_position(self.location_uu)
    }

    /// Bevy-space uniform-ish scale applied to the light (diagnostic only).
    pub fn bevy_scale(&self) -> [f32; 3] {
        to_bevy_scale(self.transform.scale)
    }
}
