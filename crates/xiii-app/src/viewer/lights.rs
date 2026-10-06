//! Dynamic-light rendering (item5h).
//!
//! The imported world is drawn with **unlit** materials and baked vertex colours, so static map
//! lights are already in the geometry. To add the runtime lights without double-counting the
//! baked lighting, this module uses a **light-only additive pass**:
//!
//! - the existing unlit textured pass is left untouched (it carries the baked colours, the zone
//!   fog and the in-shader tonemapping);
//! - a second copy of every opaque surface is spawned with a lit [`ReceiverMaterial`] in
//!   `AlphaMode::Add` and a white base colour, so it contributes only the dynamic-light term.
//!
//! The receiver pass must add exactly zero where no light reaches (item31, measured on Plage01:
//! before, up to 4/255 was added to 41% of the frame with no light in range). Everything in
//! Bevy's PBR path that is not direct lighting is therefore removed from it: zone ambient (black
//! ambient occlusion; the camera keeps the zone ambient for lit actors), distance fog
//! (`fog_enabled: false`; blending towards the fog colour would add it a second time), and the
//! in-shader tonemapping and deband dither of a non-HDR camera (`tonemap(0)` is not 0, and the
//! dither adds noise). The added light is therefore not tonemapped.
//!
//! Point lights are spawned from the decoded [`xiii_world::lights::SceneLight`]s in the viewer and
//! from the live VM actors in `--play` (see `play::sync_vm_lights`), so map-placed non-static
//! lights and the runtime `XIII.MuzzleLight` follow their actors.

use std::collections::HashMap;

use bevy::camera::visibility::RenderLayers;
use bevy::mesh::MeshVertexBufferLayoutRef;
use bevy::pbr::{
    ExtendedMaterial, MaterialExtension, MaterialExtensionKey, MaterialExtensionPipeline,
};
use bevy::prelude::*;
use bevy::render::render_resource::{
    AsBindGroup, RenderPipelineDescriptor, SpecializedMeshPipelineError,
};
use bevy::shader::ShaderDefVal;

/// The additive light-receiver material: a lit `StandardMaterial` whose pipeline drops the
/// post-lighting terms that are not light (see the module docs).
pub type ReceiverMaterial = ExtendedMaterial<StandardMaterial, ReceiverExtension>;

/// Shader-pipeline extension of [`ReceiverMaterial`]; it has no data of its own.
#[derive(Asset, AsBindGroup, TypePath, Debug, Clone, Default)]
pub struct ReceiverExtension {}

impl MaterialExtension for ReceiverExtension {
    fn specialize(
        _pipeline: &MaterialExtensionPipeline,
        descriptor: &mut RenderPipelineDescriptor,
        _layout: &MeshVertexBufferLayoutRef,
        _key: MaterialExtensionKey<Self>,
    ) -> Result<(), SpecializedMeshPipelineError> {
        if let Some(fragment) = &mut descriptor.fragment {
            strip_post_lighting_defs(&mut fragment.shader_defs);
        }
        Ok(())
    }
}

/// Removes the in-shader tonemapping and deband dither from a receiver fragment shader.
pub fn strip_post_lighting_defs(defs: &mut Vec<ShaderDefVal>) {
    defs.retain(|def| {
        !matches!(def, ShaderDefVal::Bool(name, _)
            if name == "TONEMAP_IN_SHADER" || name == "DEBAND_DITHER")
    });
}

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
    materials: &mut Assets<ReceiverMaterial>,
    images: &mut Assets<Image>,
    image_handles: &[Handle<Image>],
    scene: &WorldScene,
) -> HashMap<usize, Handle<ReceiverMaterial>> {
    let no_ambient = images.add(no_ambient_occlusion());
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
            materials.add(receiver_material(
                handle.clone(),
                no_ambient.clone(),
                resolved.two_sided,
            )),
        );
    }
    out
}

/// One additive receiver material: white base colour tinted by the surface texture, lit, added.
pub fn receiver_material(
    texture: Handle<Image>,
    no_ambient: Handle<Image>,
    two_sided: bool,
) -> ReceiverMaterial {
    ReceiverMaterial {
        base: StandardMaterial {
            base_color: Color::WHITE,
            base_color_texture: Some(texture),
            unlit: false,
            alpha_mode: AlphaMode::Add,
            // The base pass already carries the zone fog and ambient; in an additive pass either
            // would be added a second time (fog blends towards the fog colour, and the camera's
            // zone ambient stays on for lit actors). Black ambient occlusion zeroes the
            // ambient/indirect term for this pass only.
            fog_enabled: false,
            occlusion_texture: Some(no_ambient),
            cull_mode: if two_sided {
                None
            } else {
                Some(bevy::render::render_resource::Face::Back)
            },
            ..default()
        },
        extension: ReceiverExtension::default(),
    }
}

/// A 1x1 black ambient-occlusion texture: removes the ambient/indirect term from a material while
/// leaving direct (point-light) lighting untouched.
pub fn no_ambient_occlusion() -> Image {
    Image::new_fill(
        bevy::render::render_resource::Extent3d {
            width: 1,
            height: 1,
            depth_or_array_layers: 1,
        },
        bevy::render::render_resource::TextureDimension::D2,
        &[0, 0, 0, 255],
        bevy::render::render_resource::TextureFormat::Rgba8Unorm,
        bevy::asset::RenderAssetUsages::RENDER_WORLD,
    )
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

/// Re-exported so callers can name the light-selection predicate without importing the world
/// crate's module path.
pub fn is_render_dynamic(light: &SceneLight) -> bool {
    light.render_dynamic()
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_decode::common::BevyTransform;

    /// item31: the receiver pass must add nothing but direct light. Each non-light term found
    /// adding a constant on Plage01 (ambient, fog, in-shader tonemapping, dither) stays removed.
    #[test]
    fn receiver_material_is_light_only() {
        let m = receiver_material(Handle::default(), Handle::default(), false);
        assert_eq!(m.base.alpha_mode, AlphaMode::Add);
        assert!(!m.base.unlit);
        assert!(!m.base.fog_enabled, "fog would be added a second time");
        assert!(
            m.base.occlusion_texture.is_some(),
            "zone ambient must be occluded"
        );
        assert_eq!(m.base.emissive, LinearRgba::BLACK);
        assert_eq!(m.base.base_color, Color::WHITE);
        assert!(
            receiver_material(Handle::default(), Handle::default(), true)
                .base
                .cull_mode
                .is_none()
        );

        let occlusion = no_ambient_occlusion();
        assert_eq!(occlusion.data.as_deref(), Some(&[0u8, 0, 0, 255][..]));
    }

    #[test]
    fn receiver_shader_drops_tonemapping_and_dither_only() {
        let mut defs: Vec<ShaderDefVal> = vec![
            "TONEMAP_IN_SHADER".into(),
            "DEBAND_DITHER".into(),
            "VERTEX_UVS_A".into(),
            ShaderDefVal::UInt("MAX_DIRECTIONAL_LIGHTS".into(), 10),
        ];
        strip_post_lighting_defs(&mut defs);
        assert_eq!(
            defs,
            vec![
                ShaderDefVal::from("VERTEX_UVS_A"),
                ShaderDefVal::UInt("MAX_DIRECTIONAL_LIGHTS".into(), 10),
            ]
        );
    }

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
