//! Per-zone distance fog and ambient parameters: decoding, unit conversion and the
//! per-camera zone selection rule.
//!
//! ## What is modelled
//!
//! UE2 keeps distance fog and ambient light on the `ZoneInfo` actor that owns a BSP zone
//! (`Engine.ZoneInfo` and its `Engine.SkyZoneInfo` subclass). [`crate::zones::SceneZone`] already
//! carries the per-zone fog fields, but this module adds the **XIII-specific** fields the survey
//! found in use (`AmbientIntensity`, `bFogZone`, `bFogPerZone`) and, more importantly, the
//! resolution/selection logic the renderer needs:
//!
//! * [`FogParams`] is the normalized, renderer-ready description of one zone's fog (colour and
//!   linear start/end in **metres**) plus its ambient light.
//! * [`resolve_zone_fog`] merges the map's tagged properties with the inherited class defaults
//!   read through `xiii_script` (`ClassDefaults`). A float field the map does not tag takes the
//!   class default; every field's source is reported. This mirrors the placement resolution rule
//!   in [`crate::import_map`], not a silent zero.
//! * [`SceneFog`] holds the per-zone resolved parameters and implements
//!   [`SceneFog::zone_of_point`] / [`SceneFog::params_for_zone`] so a camera can pick its zone
//!   each frame from the same `zones::ZoneMap` classification the importer used.
//!
//! ## Evidence and units
//!
//! * `bDistanceFog`, `DistanceFogColor`, `DistanceFogStart`, `DistanceFogEnd`,
//!   `AmbientBrightness/Hue/Saturation`, `AmbientIntensity`, `bFogZone`, `bFogPerZone` are
//!   decoded by `xiii-decode` (measured; `xiii-tool props`).
//! * `DistanceFogStart`/`DistanceFogEnd` are Unreal units; the renderer wants metres, converted
//!   with the single coordinate policy (`UNREAL_UNITS_PER_METER = 90`).
//! * The `Color` channel order is **not** verified (see `xiii-package` and `zones.rs`): the packed
//!   bytes are exposed as `[u8; 4]` and [`FogParams::to_bgr`] documents the assumed order.
//!   Elsewhere in this codebase the same stored order is decoded as BGRA for static-mesh
//!   instance lighting (measured there); [`FogParams::to_bgr`] follows that measured precedent
//!   and is labelled a hypothesis for fog specifically.
//! * The UE2 distance-fog blend is linear between `DistanceFogStart` and `DistanceFogEnd`
//!   (upstream `UFog`/`FSceneRenderer::RenderFog` behaviour and the reflected property names);
//!   the exact falloff is a **hypothesis** consistent with those names.
//! * A zone with no `DistanceFog*` properties at all resolves to "no fog" rather than a default
//!   colour. `bDistanceFog = false` (present on some records) is honoured explicitly.

use xiii_decode::common::{Props, UNREAL_UNITS_PER_METER};
use xiii_decode::model::{BspNode, Model};
use xiii_package::{Limits, ObjectRef, Package, PropertyValue, StructValue};

use crate::zones::ZoneMap;

/// Falloff model of the distance fog. Only [`FogFalloff::Linear`] is decoded from the corpus;
/// the enum is kept so the hypothesis is explicit at every call site.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FogFalloff {
    /// Linear: intensity 0 at `start`, 1 at `end` (both metres).
    Linear {
        /// Distance from the camera (metres) where fog begins.
        start: f32,
        /// Distance from the camera (metres) where fog is fully opaque.
        end: f32,
    },
    /// No fog (the zone disabled it or carries no fog properties).
    Off,
}

/// Zone ambient-light parameters (`AmbientBrightness`/`Hue`/`Saturation` + `AmbientIntensity`).
///
/// UE2 ambient light is an HSV triple plus a brightness scale; the RGB conversion here is the
/// standard UE2 `Hue` (0..255 around the colour wheel), `Saturation` (0..255) and
/// `Brightness` (0..255) formula. `AmbientIntensity` (the XIII-specific byte) scales the result.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ZoneAmbient {
    /// `AmbientBrightness` byte, or the class default when untagged (`None` if neither).
    pub brightness: Option<u8>,
    /// `AmbientHue` byte.
    pub hue: Option<u8>,
    /// `AmbientSaturation` byte.
    pub saturation: Option<u8>,
    /// XIII-specific `AmbientIntensity` byte.
    pub intensity: Option<u8>,
}

impl ZoneAmbient {
    /// True when no ambient property is present.
    pub fn is_empty(&self) -> bool {
        self.brightness.is_none()
            && self.hue.is_none()
            && self.saturation.is_none()
            && self.intensity.is_none()
    }

    /// Linear RGB in `0..1` from the HSV triple, scaled by `intensity/128`.
    ///
    /// `None` when brightness is absent or zero. The HSV formula follows UE2's
    /// `UObject::GetHSVColor` / `FColor` conversion (hypothesis for XIII, which reuses UE2).
    pub fn linear_rgb(&self) -> Option<[f32; 3]> {
        let b = self.brightness?;
        if b == 0 {
            return None;
        }
        let h = self.hue.unwrap_or(0) as f32 / 255.0;
        let s = self.saturation.unwrap_or(255) as f32 / 255.0;
        let v = b as f32 / 255.0;
        // UE2 hue starts at red; sector 0..6.
        let h6 = h * 6.0;
        let i = h6.floor() as i32 % 6;
        let f = h6 - h6.floor();
        let p = v * (1.0 - s);
        let q = v * (1.0 - s * f);
        let t = v * (1.0 - s * (1.0 - f));
        let rgb = match i {
            0 => [v, t, p],
            1 => [q, v, p],
            2 => [p, v, t],
            3 => [p, q, v],
            4 => [t, p, v],
            _ => [v, p, q],
        };
        let scale = self.intensity.map(|x| x as f32 / 128.0).unwrap_or(1.0);
        Some([rgb[0] * scale, rgb[1] * scale, rgb[2] * scale])
    }
}

/// Renderer-ready fog parameters of one zone.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FogParams {
    /// Packed `DistanceFogColor` bytes as stored (channel order unverified; see [`Self::to_bgr`]).
    pub color_bytes: [u8; 4],
    /// Falloff (linear in metres, or off).
    pub falloff: FogFalloff,
    /// Zone ambient light.
    pub ambient: ZoneAmbient,
}

impl FogParams {
    /// A zone with no fog.
    pub fn none() -> Self {
        Self {
            color_bytes: [0; 4],
            falloff: FogFalloff::Off,
            ambient: ZoneAmbient::default(),
        }
    }

    /// True when the zone has a usable linear fog interval (start < end, end > 0).
    pub fn is_fogged(&self) -> bool {
        matches!(self.falloff, FogFalloff::Linear { start, end } if end > start && end > 0.0)
    }

    /// Linear start/end in metres, if fogged.
    pub fn distances_m(&self) -> Option<(f32, f32)> {
        match self.falloff {
            FogFalloff::Linear { start, end } if self.is_fogged() => Some((start, end)),
            _ => None,
        }
    }

    /// Colour as linear RGB `0..1`, interpreting the stored bytes as BGRA (blue, green, red,
    /// alpha) — the order measured for `StaticMeshInstance` vertex colours and used by
    /// `xiii-script`'s `Color` members (`b`,`g`,`r`,`a` on little-endian). **Hypothesis** for
    /// fog specifically; the raw bytes stay authoritative in [`Self::color_bytes`].
    pub fn to_bgr(&self) -> [f32; 3] {
        let c = self.color_bytes;
        [
            f32::from(c[2]) / 255.0,
            f32::from(c[1]) / 255.0,
            f32::from(c[0]) / 255.0,
        ]
    }
}

/// The zone actor fields this module reads, merged map-property-first then class-default.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ResolvedZoneFog {
    /// Fog parameters.
    pub params: FogParams,
    /// `bDistanceFog` was present in the map and false (fog disabled).
    pub disabled_by_map: bool,
    /// `DistanceFogColor` was absent from the map (raw bytes left zero).
    pub color_untagged: bool,
    /// `DistanceFogStart` came from the class default.
    pub start_from_class_default: bool,
    /// `DistanceFogEnd` came from the class default.
    pub end_from_class_default: bool,
}

/// Raw `DistanceFog*` fields of a map property block.
#[derive(Debug, Clone, Copy, Default)]
struct RawFog {
    color: Option<[u8; 4]>,
    start: Option<f32>,
    end: Option<f32>,
    enabled: Option<bool>,
    ambient: ZoneAmbient,
}

/// A `Color` struct property value.
fn color(p: &Props, name: &str) -> Option<[u8; 4]> {
    match &p.get(name)?.value {
        PropertyValue::Struct(StructValue::Color(c)) => Some(*c),
        _ => None,
    }
}

/// Reads one zone actor's tagged fog/ambient fields, `None` when the actor is not an export or
/// its property block fails (so the caller can fall back, never silently).
fn read_map_fog(package: &Package, data: &[u8], actor: ObjectRef) -> Option<RawFog> {
    let ObjectRef::Export(e) = actor else {
        return None;
    };
    let props = package
        .read_object_properties(data, e as usize, &Limits::default())
        .ok()?;
    let p = Props::new(package, &props);
    Some(RawFog {
        color: color(&p, "DistanceFogColor"),
        start: p.float("DistanceFogStart"),
        end: p.float("DistanceFogEnd"),
        enabled: p.bool("bDistanceFog"),
        ambient: ZoneAmbient {
            brightness: p.byte("AmbientBrightness"),
            hue: p.byte("AmbientHue"),
            saturation: p.byte("AmbientSaturation"),
            intensity: p.byte("AmbientIntensity"),
        },
    })
}

/// Resolved class-default float of `name`, or `None` (unknown class/property or not a float).
fn class_float_default(
    defaults: &mut crate::ClassDefaults,
    class_path: Option<&str>,
    name: &str,
) -> Option<f32> {
    let cp = class_path?;
    defaults.float_default(cp, name).ok().flatten()
}

/// Resolved class-default byte of `name` (floats are rounded and clamped; the reflected
/// `ByteProperty` defaults arrive as `Float` through `ClassDefaults`).
fn class_byte_default(
    defaults: &mut crate::ClassDefaults,
    class_path: Option<&str>,
    name: &str,
) -> Option<u8> {
    class_float_default(defaults, class_path, name).map(|v| v.round().clamp(0.0, 255.0) as u8)
}

/// Resolves one zone actor's fog/ambient from the map property block, then the inherited class
/// defaults. The map's `bDistanceFog = false` disables fog even when start/end exist.
pub fn resolve_zone_fog(
    package: &Package,
    data: &[u8],
    actor: ObjectRef,
    class_path: Option<&str>,
    mut defaults: Option<&mut crate::ClassDefaults>,
) -> ResolvedZoneFog {
    let raw = read_map_fog(package, data, actor).unwrap_or_default();
    // Resolve start/end: a tagged value wins; an untagged one takes the class default. The two
    // `*_from_class_default` flags record which fields fell back.
    let (falloff, start_from_class_default, end_from_class_default) =
        if raw.start.is_some() && raw.end.is_some() {
            let (s, e) = (raw.start.unwrap_or(0.0), raw.end.unwrap_or(0.0));
            (
                FogFalloff::Linear {
                    start: s / UNREAL_UNITS_PER_METER,
                    end: e / UNREAL_UNITS_PER_METER,
                },
                false,
                false,
            )
        } else {
            let ds = defaults
                .as_deref_mut()
                .and_then(|d| class_float_default(d, class_path, "DistanceFogStart"));
            let de = defaults
                .as_deref_mut()
                .and_then(|d| class_float_default(d, class_path, "DistanceFogEnd"));
            let start = raw.start.or(ds);
            let end = raw.end.or(de);
            let falloff = match (start, end) {
                (Some(s), Some(e)) => FogFalloff::Linear {
                    start: s / UNREAL_UNITS_PER_METER,
                    end: e / UNREAL_UNITS_PER_METER,
                },
                _ => FogFalloff::Off,
            };
            (
                falloff,
                raw.start.is_none() && ds.is_some(),
                raw.end.is_none() && de.is_some(),
            )
        };
    let mut params = FogParams {
        color_bytes: raw.color.unwrap_or([0; 4]),
        falloff,
        ambient: ZoneAmbient::default(),
    };
    let mut ambient = raw.ambient;
    if let Some(d) = defaults {
        if ambient.brightness.is_none() {
            ambient.brightness = class_byte_default(d, class_path, "AmbientBrightness");
        }
        if ambient.hue.is_none() {
            ambient.hue = class_byte_default(d, class_path, "AmbientHue");
        }
        if ambient.saturation.is_none() {
            ambient.saturation = class_byte_default(d, class_path, "AmbientSaturation");
        }
        if ambient.intensity.is_none() {
            ambient.intensity = class_byte_default(d, class_path, "AmbientIntensity");
        }
    }
    params.ambient = ambient;
    let disabled_by_map = raw.enabled == Some(false);
    if disabled_by_map {
        params.falloff = FogFalloff::Off;
    }
    ResolvedZoneFog {
        params,
        disabled_by_map,
        color_untagged: raw.color.is_none(),
        start_from_class_default,
        end_from_class_default,
    }
}

/// Per-zone resolved fog/ambient for one imported world, with the BSP point classifier.
#[derive(Debug, Clone, Default)]
pub struct SceneFog {
    /// One entry per zone index (parallel to [`crate::WorldScene::zones`]).
    pub params: Vec<FogParams>,
    /// BSP tree for per-point zone selection, or `None` when the map has no level model.
    pub zone_map: Option<ZoneMap>,
    /// The model's BSP nodes (needed by [`ZoneMap`]); empty when unavailable.
    pub nodes: Vec<BspNode>,
    /// Number of zones that disabled fog via the map's `bDistanceFog = false`.
    pub disabled_by_map: usize,
    /// Number of zones whose start/end came from a class default.
    pub from_class_default: usize,
    /// Number of zones with no usable fog interval.
    pub unfogged: usize,
    /// Number of zones whose colour property was not tagged.
    pub color_untagged: usize,
}

impl SceneFog {
    /// Builds the fog table for a decoded level.
    ///
    /// `scene_zones` is the already-built zone metadata; `model` supplies the BSP tree and the
    /// zone->actor refs. `defaults` is optional so a caller without the class packages can still
    /// get the map-tagged values.
    pub fn build(
        package: &Package,
        data: &[u8],
        scene_zones: &[crate::zones::SceneZone],
        model: Option<&Model>,
        mut defaults: Option<&mut crate::ClassDefaults>,
    ) -> Self {
        let mut out = Self {
            params: Vec::with_capacity(scene_zones.len()),
            zone_map: model.map(ZoneMap::new),
            nodes: model.map(|m| m.nodes.clone()).unwrap_or_default(),
            ..Self::default()
        };
        for z in scene_zones {
            let actor = model
                .and_then(|m| m.zones.get(z.index as usize))
                .map(|z| z.actor)
                .unwrap_or(ObjectRef::Null);
            let resolved = resolve_zone_fog(
                package,
                data,
                actor,
                z.actor_class.as_deref(),
                defaults.as_deref_mut(),
            );
            if resolved.disabled_by_map {
                out.disabled_by_map += 1;
            }
            if resolved.start_from_class_default || resolved.end_from_class_default {
                out.from_class_default += 1;
            }
            if !resolved.params.is_fogged() {
                out.unfogged += 1;
            }
            if resolved.color_untagged {
                out.color_untagged += 1;
            }
            out.params.push(resolved.params);
        }
        out
    }

    /// Zone containing a Bevy-space point (metres), using the same source-space BSP
    /// classification the importer used. `None` when the map has no BSP or the point misses the
    /// leaf/zone table. The inverse of `to_bevy_position` (`(x,y,z)_src -> (y,z,-x)/s`) is
    /// `(x,y,z)_bevy -> (-z*s, x*s, y*s)` in source units.
    pub fn zone_of_point(&self, point: [f32; 3]) -> Option<u8> {
        let map = self.zone_map.as_ref()?;
        let s = UNREAL_UNITS_PER_METER;
        let source = [-point[2] * s, point[0] * s, point[1] * s];
        map.zone_of_point(&self.nodes, source)
    }

    /// Fog/ambient of a zone index, or `None` when out of range.
    pub fn params_for_zone(&self, zone: u8) -> Option<&FogParams> {
        self.params.get(zone as usize)
    }

    /// Fog/ambient at a Bevy-space point (metres): the containing zone's parameters, else the
    /// sky zone's parameters (a point outside the BSP, e.g. the sky camera), else `None`.
    pub fn params_at(&self, point: [f32; 3], sky_zone: Option<u8>) -> Option<&FogParams> {
        if let Some(z) = self.zone_of_point(point)
            && let Some(p) = self.params_for_zone(z)
        {
            return Some(p);
        }
        sky_zone.and_then(|z| self.params_for_zone(z))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node(plane: [f32; 4]) -> BspNode {
        BspNode {
            plane,
            zone_mask: 0,
            flags: 0,
            vert_pool: 0,
            surf: 0,
            back: -1,
            front: -1,
            coplanar: -1,
            collision_bound: -1,
            render_bound: -1,
            unknown_a: 0,
            sphere: [0.0; 4],
            zone: [0, 0],
            num_vertices: 0,
            leaf: [0, 1],
            unknown_b: 0,
            first_vertex: 0,
        }
    }

    #[test]
    fn ambient_hsv_converts_and_scales() {
        // Full brightness, zero saturation = white.
        let a = ZoneAmbient {
            brightness: Some(255),
            hue: Some(0),
            saturation: Some(0),
            intensity: None,
        };
        let rgb = a.linear_rgb().unwrap();
        assert!((rgb[0] - 1.0).abs() < 1e-4);
        assert!((rgb[1] - 1.0).abs() < 1e-4);
        assert!((rgb[2] - 1.0).abs() < 1e-4);
        // Zero brightness is no light.
        let a = ZoneAmbient {
            brightness: Some(0),
            ..a
        };
        assert!(a.linear_rgb().is_none());
        // Intensity 0 scales to zero.
        let a = ZoneAmbient {
            brightness: Some(128),
            hue: Some(0),
            saturation: Some(0),
            intensity: Some(0),
        };
        assert!(a.linear_rgb().unwrap()[0].abs() < 1e-6);
    }

    #[test]
    fn fog_off_is_not_fogged() {
        let f = FogParams::none();
        assert!(!f.is_fogged());
        assert_eq!(f.distances_m(), None);
    }

    #[test]
    fn linear_fog_requires_end_after_start() {
        let f = FogParams {
            falloff: FogFalloff::Linear {
                start: 10.0,
                end: 5.0,
            },
            ..FogParams::none()
        };
        assert!(!f.is_fogged(), "an inverted interval is not usable fog");
        let f = FogParams {
            falloff: FogFalloff::Linear {
                start: 10.0,
                end: 0.0,
            },
            ..f
        };
        assert!(!f.is_fogged(), "a zero end is not usable fog");
    }

    #[test]
    fn color_bytes_are_bgra() {
        let f = FogParams {
            color_bytes: [17, 34, 51, 0],
            ..FogParams::none()
        };
        let rgb = f.to_bgr();
        assert!((rgb[0] - 51.0 / 255.0).abs() < 1e-5);
        assert!((rgb[1] - 34.0 / 255.0).abs() < 1e-5);
        assert!((rgb[2] - 17.0 / 255.0).abs() < 1e-5);
    }

    #[test]
    fn scene_fog_zone_selection_round_trips_bevy_source() {
        // One node: plane X = 0; positive -> leaf[1] (zone 1), negative -> leaf[0] (zone 0).
        let nodes = vec![node([1.0, 0.0, 0.0, 0.0])];
        let map = ZoneMap::from_parts(0, vec![Some(0), Some(1)]);
        let fog = SceneFog {
            params: vec![
                FogParams::none(),
                FogParams {
                    falloff: FogFalloff::Linear {
                        start: 1.0,
                        end: 2.0,
                    },
                    ..FogParams::none()
                },
            ],
            zone_map: Some(map),
            nodes,
            ..SceneFog::default()
        };
        // Source +X (positive side) maps to Bevy forward -Z.
        assert_eq!(fog.zone_of_point([0.0, 0.0, -1.0]), Some(1));
        // Source -X maps to Bevy +Z.
        assert_eq!(fog.zone_of_point([0.0, 0.0, 1.0]), Some(0));
        assert!(fog.params_at([0.0, 0.0, -1.0], None).unwrap().is_fogged());
        // A point with no leaf falls back to the sky zone when one is given.
        assert!(fog.params_at([0.0, 0.0, 1.0], Some(1)).is_some());
        // Out-of-range zone indices are None, not a panic.
        assert!(fog.params_for_zone(200).is_none());
    }
}
