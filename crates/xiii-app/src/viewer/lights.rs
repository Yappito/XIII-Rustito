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

/// World-space bounds used to avoid drawing receivers outside every active light range.
#[derive(Component, Clone, Copy, Debug)]
pub struct LightReceiverBounds {
    pub center: Vec3,
    pub half_size: Vec3,
}

/// Computes a conservative world-space AABB from mesh vertices and the placed transform.
pub fn receiver_bounds(positions: &[[f32; 3]], transform: Transform) -> LightReceiverBounds {
    let matrix = transform.to_matrix();
    let mut min = Vec3::splat(f32::INFINITY);
    let mut max = Vec3::splat(f32::NEG_INFINITY);
    for position in positions {
        let point = matrix.transform_point3(Vec3::from_array(*position));
        min = min.min(point);
        max = max.max(point);
    }
    if positions.is_empty() {
        min = transform.translation;
        max = transform.translation;
    }
    LightReceiverBounds {
        center: (min + max) * 0.5,
        half_size: (max - min) * 0.5,
    }
}

/// Diagnostic switch used for the image-preserving A/B comparison.
pub const ENV_NO_LIGHT_CULL: &str = "XIII_VIEWER_NO_LIGHT_CULL";
/// Freezes animated map lights and UV materials at time zero for deterministic screenshots.
pub const ENV_FREEZE_ANIMATION: &str = "XIII_VIEWER_FREEZE_ANIMATION";

const CULL_CELL_SIZE: f32 = 32.0;
const MAX_INDEXED_HALF_EXTENT: f32 = 32.0;

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
    let t = if std::env::var_os(ENV_FREEZE_ANIMATION).is_some() {
        0.0
    } else {
        time.elapsed_secs()
    };
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

/// AABB/sphere test using squared distances; boundary touching counts as an intersection.
pub fn sphere_intersects_aabb(center: Vec3, radius: f32, min: Vec3, max: Vec3) -> bool {
    if !radius.is_finite()
        || radius < 0.0
        || !center.is_finite()
        || !min.is_finite()
        || !max.is_finite()
    {
        return false;
    }
    let closest = center.clamp(min.min(max), min.max(max));
    closest.distance_squared(center) <= radius * radius
}

fn point_light_reaches(light: &PointLight, transform: &Transform, min: Vec3, max: Vec3) -> bool {
    light.intensity.is_finite()
        && light.intensity > 0.0
        && light.range > 0.0
        && sphere_intersects_aabb(transform.translation, light.range, min, max)
}

fn cell(point: Vec3) -> (i32, i32, i32) {
    (
        (point.x / CULL_CELL_SIZE).floor() as i32,
        (point.y / CULL_CELL_SIZE).floor() as i32,
        (point.z / CULL_CELL_SIZE).floor() as i32,
    )
}

/// Computes the visible receiver set from bounds and current light components. Ordinary
/// receivers are bucketed by center; queries include their maximum half-extent before the exact
/// sphere/AABB test. Oversized receivers use an exact-test list so one BSP/terrain section cannot
/// expand a query across the map.
fn receiver_visibility<'a>(
    bounds: &[(Vec3, Vec3)],
    lights: impl Iterator<Item = (&'a PointLight, &'a Transform)>,
) -> Vec<bool> {
    let mut buckets: HashMap<(i32, i32, i32), Vec<usize>> = HashMap::new();
    let mut max_half = Vec3::ZERO;
    let mut oversized = Vec::new();
    for (index, (center, half)) in bounds.iter().copied().enumerate() {
        if half.max_element() > MAX_INDEXED_HALF_EXTENT {
            oversized.push(index);
        } else {
            max_half = max_half.max(half);
            buckets.entry(cell(center)).or_default().push(index);
        }
    }

    let mut visible = vec![false; bounds.len()];
    for (light, transform) in lights {
        let radius = light.range;
        if light.intensity <= 0.0 || !radius.is_finite() || radius <= 0.0 {
            continue;
        }
        let center = transform.translation;
        let query_half = Vec3::splat(radius) + max_half;
        let lo = cell(center - query_half);
        let hi = cell(center + query_half);
        for x in lo.0..=hi.0 {
            for y in lo.1..=hi.1 {
                for z in lo.2..=hi.2 {
                    let Some(candidates) = buckets.get(&(x, y, z)) else {
                        continue;
                    };
                    for &index in candidates {
                        let (receiver_center, half) = bounds[index];
                        if !visible[index]
                            && point_light_reaches(
                                light,
                                transform,
                                receiver_center - half,
                                receiver_center + half,
                            )
                        {
                            visible[index] = true;
                        }
                    }
                }
            }
        }
        for &index in &oversized {
            if visible[index] {
                continue;
            }
            let (receiver_center, half) = bounds[index];
            if point_light_reaches(
                light,
                transform,
                receiver_center - half,
                receiver_center + half,
            ) {
                visible[index] = true;
            }
        }
    }

    visible
}

/// Updates receiver visibility from current `PointLight`s. Runs after the viewer or VM light
/// sync, so changed transforms, ranges and lifetimes are reflected before rendering.
pub fn cull_receivers(
    mut receivers: Query<(Entity, &LightReceiverBounds, &mut Visibility)>,
    lights: Query<(&PointLight, &Transform)>,
) {
    if std::env::var_os(ENV_NO_LIGHT_CULL).is_some() {
        return;
    }

    let mut entities = Vec::new();
    let mut bounds = Vec::new();
    for (entity, receiver, _) in &receivers {
        entities.push(entity);
        bounds.push((receiver.center, receiver.half_size.abs()));
    }
    let visible = receiver_visibility(&bounds, lights.iter());
    for (entity, is_visible) in entities.into_iter().zip(visible) {
        if let Ok((_, _, mut visibility)) = receivers.get_mut(entity) {
            let wanted = if is_visible {
                Visibility::Visible
            } else {
                Visibility::Hidden
            };
            if *visibility != wanted {
                *visibility = wanted;
            }
        }
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

    #[test]
    fn receiver_cull_includes_exact_range_boundary() {
        let light = PointLight {
            intensity: 10.0,
            range: 2.0,
            ..default()
        };
        let transform = Transform::from_xyz(0.0, 0.0, 0.0);
        assert!(point_light_reaches(
            &light,
            &transform,
            Vec3::new(2.0, -0.5, -0.5),
            Vec3::new(3.0, 0.5, 0.5),
        ));
        let bounds = [(Vec3::new(2.5, 0.0, 0.0), Vec3::new(0.5, 0.5, 0.5))];
        assert_eq!(
            receiver_visibility(&bounds, std::iter::once((&light, &transform))),
            vec![true]
        );
    }

    #[test]
    fn receiver_cull_ignores_zero_range_even_at_touching_point() {
        let light = PointLight {
            intensity: 10.0,
            range: 0.0,
            ..default()
        };
        assert!(!point_light_reaches(
            &light,
            &Transform::IDENTITY,
            Vec3::ZERO,
            Vec3::ZERO,
        ));
        let bounds = [(Vec3::ZERO, Vec3::ZERO)];
        assert_eq!(
            receiver_visibility(&bounds, std::iter::once((&light, &Transform::IDENTITY)),),
            vec![false]
        );
    }

    #[test]
    fn receiver_spanning_light_is_kept_even_when_its_center_is_outside_range() {
        let light = PointLight {
            intensity: 10.0,
            range: 1.0,
            ..default()
        };
        let transform = Transform::from_xyz(0.0, 0.0, 0.0);
        assert!(point_light_reaches(
            &light,
            &transform,
            Vec3::new(0.5, -0.25, -0.25),
            Vec3::new(5.0, 0.25, 0.25),
        ));
        let bounds = [(Vec3::new(2.75, 0.0, 0.0), Vec3::new(2.25, 0.25, 0.25))];
        assert_eq!(
            receiver_visibility(&bounds, std::iter::once((&light, &transform))),
            vec![true],
            "cell broadphase must keep a wide receiver whose center is outside range"
        );
    }

    #[test]
    fn moved_light_stops_reaching_receiver() {
        let light = PointLight {
            intensity: 10.0,
            range: 1.0,
            ..default()
        };
        let receiver_min = Vec3::new(-0.5, -0.5, -0.5);
        let receiver_max = Vec3::splat(0.5);
        assert!(point_light_reaches(
            &light,
            &Transform::IDENTITY,
            receiver_min,
            receiver_max
        ));
        assert!(!point_light_reaches(
            &light,
            &Transform::from_xyz(10.0, 0.0, 0.0),
            receiver_min,
            receiver_max,
        ));
        let bounds = [(Vec3::ZERO, Vec3::splat(0.5))];
        let moved = Transform::from_xyz(10.0, 0.0, 0.0);
        assert_eq!(
            receiver_visibility(&bounds, std::iter::once((&light, &moved))),
            vec![false]
        );
    }

    #[test]
    fn oversized_receiver_uses_fallback_and_is_not_lost_by_spatial_grid() {
        let light = PointLight {
            intensity: 10.0,
            range: 1.0,
            ..default()
        };
        let transform = Transform::IDENTITY;
        let bounds = [(Vec3::new(40.0, 0.0, 0.0), Vec3::new(40.0, 1.0, 1.0))];
        assert_eq!(
            receiver_visibility(&bounds, std::iter::once((&light, &transform))),
            vec![true]
        );
    }

    #[test]
    fn transformed_receiver_bounds_enclose_rotated_scaled_vertices() {
        let transform = Transform {
            translation: Vec3::new(10.0, 2.0, -3.0),
            rotation: Quat::from_rotation_y(std::f32::consts::FRAC_PI_2),
            scale: Vec3::new(2.0, 1.0, 1.0),
        };
        let bounds = receiver_bounds(&[[-1.0, 0.0, 0.0], [1.0, 0.0, 0.0]], transform);
        assert!((bounds.center - transform.translation).length() < 1e-5);
        assert!((bounds.half_size.z - 2.0).abs() < 1e-5);
        assert!(bounds.half_size.x < 1e-5);
    }
}
