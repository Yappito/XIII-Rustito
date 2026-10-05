//! Dynamic-light rendering (item5h).
//!
//! The imported world is drawn with **unlit** materials and baked vertex colours, so static map
//! lights are already in the geometry. To add the runtime lights without double-counting the
//! baked lighting, this module uses a **light-only additive pass**:
//!
//! - the existing unlit textured pass is left untouched (it carries the baked colours);
//! - a second copy of every opaque surface is spawned with a *lit* `StandardMaterial` in
//!   `AlphaMode::Add` and a white base colour, so it contributes only the dynamic-light term;
//! - the camera's `AmbientLight` is forced to zero while the receivers exist (it has no effect on
//!   the unlit pass and would otherwise add a constant offset everywhere).
//!
//! Point lights are spawned from the decoded [`xiii_world::lights::SceneLight`]s in the viewer and
//! from the live VM actors in `--play` (see `play::sync_vm_lights`), so map-placed non-static
//! lights and the runtime `XIII.MuzzleLight` follow their actors.

use std::collections::HashMap;

use bevy::camera::visibility::RenderLayers;
use bevy::prelude::*;

use xiii_decode::common::actor_to_bevy;
use xiii_script::{ObjectId, Value, Vm};
use xiii_world::WorldScene;
use xiii_world::lights::{LightType, SceneLight, hsb_to_rgb};

/// Diagnostic off-switch for dynamic lighting (`XIII_VIEWER_NO_LIGHTS`), like
/// `XIII_VIEWER_NO_PARTICLES`.
pub const ENV_NO_LIGHTS: &str = "XIII_VIEWER_NO_LIGHTS";

/// Whether the additive receiver pass and dynamic lights are disabled.
pub fn lights_disabled() -> bool {
    std::env::var_os(ENV_NO_LIGHTS).is_some()
}

/// The static map lights of the viewer, indexed by [`SceneLightEntity::index`].
#[derive(Resource, Default)]
pub struct LightRenderData {
    /// All decoded map lights (the drawable ones are selected with
    /// [`SceneLight::render_dynamic`]).
    pub lights: Vec<SceneLight>,
}

/// A light entity backed by a map light at `index` in [`LightRenderData`].
#[derive(Component)]
pub struct SceneLightEntity {
    /// Index into [`LightRenderData::lights`].
    pub index: usize,
}

/// Marker for the additive light-receiver geometry (one per opaque surface).
#[derive(Component)]
pub struct LightReceiver;

/// Bevy `PointLight` of one [`SceneLight`] at relative time `t` seconds.
pub fn point_light_for(light: &SceneLight, t: f32) -> PointLight {
    let chroma = hsb_to_rgb(light.hue, light.saturation, 255.0);
    PointLight {
        color: Color::srgb(chroma[0], chroma[1], chroma[2]),
        intensity: light.lumens_at(t),
        range: light.range_m(),
        shadow_maps_enabled: false,
        ..default()
    }
}

/// A zero ambient light used while the receiver pass exists (prevents double-counting the zone
/// ambient that the baked vertex colours already contain).
pub fn receiver_ambient() -> AmbientLight {
    AmbientLight {
        color: Color::NONE,
        brightness: 0.0,
        ..default()
    }
}

/// Spawns one `PointLight` per drawable map light. Returns how many were spawned.
pub fn spawn_scene_lights(commands: &mut Commands, scene: &WorldScene) -> usize {
    if lights_disabled() {
        return 0;
    }
    let mut n = 0;
    for (index, light) in scene.lights.iter().enumerate() {
        if !light.render_dynamic() {
            continue;
        }
        commands.spawn((
            point_light_for(light, 0.0),
            super::transform_from(&light.transform),
            RenderLayers::layer(super::MAIN_LAYER),
            SceneLightEntity { index },
            Name::new(format!("light {}", light.path)),
        ));
        n += 1;
    }
    n
}

/// Builds one additive receiver material per distinct resolved material. The base texture tints
/// the added light by the surface albedo; the base colour is white so the pass is light-only.
pub fn receiver_materials(
    materials: &mut Assets<StandardMaterial>,
    image_handles: &[Handle<Image>],
    scene: &WorldScene,
) -> HashMap<usize, Handle<StandardMaterial>> {
    let mut out = HashMap::new();
    for (index, resolved) in scene.materials.iter().enumerate() {
        let Some(texture) = resolved.base else {
            continue;
        };
        let Some(handle) = image_handles.get(texture) else {
            continue;
        };
        out.insert(
            index,
            materials.add(StandardMaterial {
                base_color: Color::WHITE,
                base_color_texture: Some(handle.clone()),
                unlit: false,
                alpha_mode: AlphaMode::Add,
                cull_mode: if resolved.two_sided {
                    None
                } else {
                    Some(bevy::render::render_resource::Face::Back)
                },
                ..default()
            }),
        );
    }
    out
}

/// Recomputes every map light's colour/intensity each frame (pulse/blink/flicker/strobe).
/// Registered only by the viewer; `--play` drives lights from the VM instead.
pub fn update_scene_lights(
    time: Res<Time>,
    data: Res<LightRenderData>,
    mut lights: Query<(&SceneLightEntity, &mut PointLight)>,
) {
    let t = time.elapsed_secs();
    for (entity, mut light) in &mut lights {
        let Some(scene_light) = data.lights.get(entity.index) else {
            continue;
        };
        let chroma = hsb_to_rgb(scene_light.hue, scene_light.saturation, 255.0);
        light.color = Color::srgb(chroma[0], chroma[1], chroma[2]);
        light.intensity = scene_light.lumens_at(t);
        light.range = scene_light.range_m();
    }
}

/// Builds a [`SceneLight`] from a live VM actor's UE2 light properties, or `None` when the actor
/// is not an actor-class light. Map-placed `TriggerLight`/`ScriptedLight` actors and the
/// runtime `XIII.MuzzleLight` are all handled by this single path.
pub fn scene_light_from_vm(vm: &Vm<'_>, id: ObjectId) -> Option<SceneLight> {
    let object = vm.objects.get(id as usize)?;
    if !object.is_actor
        || object.deleted
        || object.name.starts_with("Default__")
        || !vm.is_a(id, "Light")
    {
        return None;
    }
    let read_byte = |name: &str, default: u8| match vm.get_property(id, name) {
        Some(Value::Byte(b)) => *b,
        Some(Value::Int(i)) => u8::try_from(*i).unwrap_or(default),
        _ => default,
    };
    let read_float = |name: &str, default: f32| match vm.get_property(id, name) {
        Some(Value::Float(f)) => *f,
        Some(Value::Int(i)) => *i as f32,
        Some(Value::Byte(b)) => f32::from(*b),
        _ => default,
    };
    let read_bool = |name: &str, default: bool| match vm.get_property(id, name) {
        Some(Value::Bool(b)) => *b,
        _ => default,
    };
    let location = vm.location_prop(id)?;
    let rotation = vm.rotation_prop(id).unwrap_or([0; 3]);
    let light_type = LightType::from_byte(read_byte("LightType", 1));
    Some(SceneLight {
        path: object.name.clone(),
        transform: actor_to_bevy(location, rotation, [1.0; 3]),
        location_uu: location,
        light_type,
        effect: read_byte("LightEffect", 0),
        brightness: read_float("LightBrightness", 150.0),
        hue: read_byte("LightHue", 0),
        saturation: read_byte("LightSaturation", 255),
        radius: read_byte("LightRadius", 64),
        period: read_byte("LightPeriod", 32),
        phase: read_byte("LightPhase", 0),
        cone: read_byte("LightCone", 128),
        dynamic: read_bool("bDynamicLight", false),
        b_static: read_bool("bStatic", true),
        actor_light: read_bool("bActorLight", false),
        hidden: read_bool("bHidden", false),
    })
}

/// Diagnostic: the light-receiver ambient when the pass is enabled.
pub fn receiver_ambient_if_enabled() -> Option<AmbientLight> {
    (!lights_disabled()).then(receiver_ambient)
}

/// Re-exported so callers can name the light-selection predicate without importing the world
/// crate's module path.
pub fn is_render_dynamic(light: &SceneLight) -> bool {
    light.render_dynamic()
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_decode::common::BevyTransform;

    fn light(light_type: LightType, brightness: f32) -> SceneLight {
        SceneLight {
            path: "L".into(),
            transform: BevyTransform {
                translation: [1.0, 2.0, 3.0],
                rotation: [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]],
                scale: [1.0; 3],
            },
            location_uu: [0.0; 3],
            light_type,
            effect: 0,
            brightness,
            hue: 34,
            saturation: 34,
            radius: 12,
            period: 32,
            phase: 0,
            cone: 128,
            dynamic: false,
            b_static: false,
            actor_light: true,
            hidden: false,
        }
    }

    #[test]
    fn muzzle_light_colour_is_warm_and_intensity_finite() {
        let l = light(LightType::Steady, 255.0);
        let pl = point_light_for(&l, 0.0);
        let c = pl.color.to_linear();
        // Hue 34 / sat 34 is a warm (R > G > B) amber.
        assert!(c.red > c.green && c.green > c.blue, "{c:?}");
        assert!(pl.intensity.is_finite() && pl.intensity > 0.0);
        assert!(pl.intensity <= xiii_world::lights::LIGHT_MAX_LUMENS + 1.0);
        assert!((pl.range - xiii_world::lights::radius_to_meters(12)).abs() < 1e-6);
    }

    #[test]
    fn off_light_has_zero_intensity() {
        let l = light(LightType::None, 255.0);
        assert_eq!(point_light_for(&l, 0.0).intensity, 0.0);
    }

    #[test]
    fn type_brightness_scale_reexport_matches_world() {
        assert_eq!(
            xiii_world::lights::type_brightness_scale(LightType::Steady, 0, 0, 5.0),
            1.0
        );
        let l = light(LightType::Blink, 255.0);
        // Blink can be on or off but always finite.
        assert!(point_light_for(&l, 0.0).intensity.is_finite());
    }
}
