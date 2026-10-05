//! Per-frame zone fog and ambient selection for the viewer and `--play`.
//!
//! The camera's current zone is found from the imported `WorldScene::fog` table using the same
//! BSP point classifier the importer used. The selected [`FogParams`] drive a Bevy
//! [`DistanceFog`] on the camera (linear `FogFalloff`, metres) and an [`AmbientLight`] on the
//! camera. When a zone has no usable interval the fog colour's alpha is set to zero, which
//! disables the effect (`bevy_pbr`'s `linear_fog` multiplies by the colour alpha); this is
//! visible in the overlay counters, never a silent skip.
//!
//! The sky camera uses the sky zone's own fog, as UE2 does (the sky is a separate zone).

use bevy::pbr::{DistanceFog, FogFalloff};
use bevy::prelude::*;

use xiii_world::WorldScene;
use xiii_world::fog::FogParams;

use super::SkyCamera;

/// Imported fog/ambient table plus the sky-zone index, shared by the viewer and `--play`.
#[derive(Resource, Clone)]
pub struct FogContext {
    /// Per-zone fog table.
    pub fog: xiii_world::fog::SceneFog,
    /// First sky zone index, when any.
    pub sky_zone: Option<u8>,
}

impl FogContext {
    /// Builds the context from an imported scene.
    pub fn new(scene: &WorldScene) -> Self {
        Self {
            fog: scene.fog.clone(),
            sky_zone: scene.sky_zones.first().map(|z| *z as u8),
        }
    }

    /// Fog at a Bevy-space point, falling back to the sky zone when the point is outside the BSP.
    pub fn params_at(&self, point: Vec3) -> Option<&FogParams> {
        self.fog.params_at(point.to_array(), self.sky_zone)
    }
}

/// Whether fog rendering is disabled (`XIII_VIEWER_NO_FOG` set). Used for before/after captures
/// and quick diagnosis; the effect is otherwise always applied.
pub fn fog_disabled() -> bool {
    std::env::var_os("XIII_VIEWER_NO_FOG").is_some()
}

/// Converts a zone's [`FogParams`] into a Bevy [`DistanceFog`]. An unfogged zone yields a
/// transparent colour (alpha 0), which disables the effect.
pub fn distance_fog(params: &FogParams) -> DistanceFog {
    let rgb = params.to_bgr();
    match params.distances_m() {
        Some((start, end)) => DistanceFog {
            color: Color::srgba(rgb[0], rgb[1], rgb[2], 1.0),
            falloff: FogFalloff::Linear { start, end },
            ..default()
        },
        // Alpha 0 disables the blend (`fog_color.a *= intensity` then `mix(.., fog_color.a)`).
        None => DistanceFog {
            color: Color::srgba(rgb[0], rgb[1], rgb[2], 0.0),
            falloff: FogFalloff::Linear {
                start: 0.0,
                end: 1.0,
            },
            ..default()
        },
    }
}

/// Converts a zone's ambient light into a Bevy [`AmbientLight`] (`None` when the zone has none).
///
/// Note: every diagnostic surface material in this runtime is `unlit`, so Bevy's ambient light
/// has no visible effect on them; the component is set for correctness and the selected values
/// are reported in the overlay. This is stated, not hidden.
pub fn ambient_light(params: &FogParams) -> Option<AmbientLight> {
    let rgb = params.ambient.linear_rgb()?;
    Some(AmbientLight {
        color: Color::srgb(rgb[0], rgb[1], rgb[2]),
        brightness: 1.0,
        ..default()
    })
}

/// Re-selects fog and ambient for every camera each frame. The main camera uses the zone
/// containing it; the sky camera uses its own (sky) zone's fog.
pub fn update_fog(
    ctx: Res<FogContext>,
    disabled: Res<FogDisabled>,
    mut main: Query<(&Transform, &mut DistanceFog, &mut AmbientLight), Without<SkyCamera>>,
    mut sky: Query<(&SkyCamera, &mut DistanceFog, &mut AmbientLight), With<SkyCamera>>,
) {
    // Main camera.
    for (transform, mut fog, mut ambient) in &mut main {
        let params = if disabled.0 {
            FogParams::none()
        } else {
            ctx.params_at(transform.translation)
                .cloned()
                .unwrap_or_else(FogParams::none)
        };
        *fog = distance_fog(&params);
        *ambient = ambient_or_none(&params);
    }
    // Sky camera: its own zone's fog (a sky zone is a zone like any other).
    for (cam, mut fog, mut ambient) in &mut sky {
        let params = if disabled.0 {
            FogParams::none()
        } else {
            ctx.fog
                .params_for_zone(cam.sky_zone.unwrap_or(0))
                .cloned()
                .unwrap_or_else(FogParams::none)
        };
        *fog = distance_fog(&params);
        *ambient = ambient_or_none(&params);
    }
}

/// `XIII_VIEWER_NO_FOG` disables fog selection (a diagnostic resource).
#[derive(Resource, Default)]
pub struct FogDisabled(pub bool);

/// The ambient component value for a zone: black if the zone has none (so a previous zone's
/// light does not linger).
fn ambient_or_none(params: &FogParams) -> AmbientLight {
    ambient_light(params).unwrap_or_else(|| AmbientLight {
        color: Color::NONE,
        brightness: 0.0,
        ..default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_world::fog::{FogFalloff as WorldFalloff, ZoneAmbient};

    #[test]
    fn fogged_zone_maps_to_linear_distance_fog() {
        let p = FogParams {
            color_bytes: [0, 0, 255, 0],
            falloff: WorldFalloff::Linear {
                start: 10.0,
                end: 20.0,
            },
            ambient: ZoneAmbient::default(),
        };
        let f = distance_fog(&p);
        match f.falloff {
            FogFalloff::Linear { start, end } => {
                assert!((start - 10.0).abs() < 1e-5 && (end - 20.0).abs() < 1e-5);
            }
            other => panic!("expected linear falloff, got {other:?}"),
        }
        assert!((f.color.alpha() - 1.0).abs() < 1e-5);
    }

    #[test]
    fn unfogged_zone_disables_by_transparent_alpha() {
        let f = distance_fog(&FogParams::none());
        assert!(f.color.alpha() < 1e-6, "alpha must disable the blend");
    }

    #[test]
    fn ambient_none_when_brightness_absent() {
        assert!(ambient_light(&FogParams::none()).is_none());
        let p = FogParams {
            ambient: ZoneAmbient {
                brightness: Some(255),
                ..Default::default()
            },
            ..FogParams::none()
        };
        assert!(ambient_light(&p).is_some());
    }
}
