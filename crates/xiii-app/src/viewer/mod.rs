//! Viewer modes: the map importer diagnostic (this module) and the skinned-character
//! viewer (`skinned`).
//!
//! Diagnostic map viewer (M2a): imports a map from an owned installation through
//! `xiii-install` + `xiii-decode` and shows static-mesh actors, the level BSP and terrain
//! with unlit textured `StandardMaterial`s. A fly camera starts at the PlayerStart; the
//! overlay shows the object path under the crosshair and every import counter, including
//! skipped/failed categories. This is an importer diagnostic, not a playable mission.

pub mod decals;
pub mod fog;
pub mod lights;
pub mod particles;
pub mod skinned;

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::core_pipeline::prepass::DepthPrepass;
use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::math::Affine2;
use bevy::mesh::Indices;
use bevy::pbr::DistanceFog;
use bevy::pbr::decal::ForwardDecalMaterial;
use bevy::prelude::*;
use bevy::render::render_resource::{
    Extent3d, Face, PrimitiveTopology, TextureDimension, TextureFormat,
};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::cli::{Lighting, Options};
use xiii_world::WorldScene;
use xiii_world::materials::{BlendMode, ResolvedMaterial, UvOp};

/// Render layer of the playable zones (drawn by the main camera).
pub(crate) const MAIN_LAYER: usize = 0;
/// Render layer of the sky zone (drawn only by the second, sky camera).
pub(crate) const SKY_LAYER: usize = 1;

/// Viewer plugin.
pub struct ViewerPlugin {
    /// Parsed options (map, game dir, unattended settings).
    pub options: Options,
}

#[derive(Resource)]
struct ViewerConfig {
    options: Options,
}

/// CPU copy of every placed object for crosshair picking.
#[derive(Resource, Default)]
struct PickData {
    objects: Vec<PickObject>,
}

struct PickObject {
    path: String,
    min: Vec3,
    max: Vec3,
    triangles: Vec<[Vec3; 3]>,
}

#[derive(Resource)]
struct ImportSummary {
    title: String,
    lines: Vec<String>,
    problems: usize,
}

#[derive(Component)]
struct FlyCam {
    yaw: f32,
    pitch: f32,
    speed: f32,
}

#[derive(Component)]
struct OverlayText;

/// Second camera that renders the sky zone. Its translation is fixed at the `SkyZoneInfo`
/// location; `sky_follow` copies the main camera's rotation every frame.
#[derive(Component)]
pub(crate) struct SkyCamera {
    pub(crate) position: Vec3,
    /// Sky zone index whose fog this camera uses.
    pub(crate) sky_zone: Option<u8>,
}

#[derive(Resource)]
struct RunState {
    start: Instant,
    frame: u64,
    shot: u8,
    shot_done: bool,
    target_at: Option<Instant>,
    picked: String,
}

#[derive(Resource, Default)]
struct ShotFlag(bool);

/// Animated texture-coordinate transform for one material handle (the transform itself lives in
/// [`StandardMaterial::uv_transform`] and is recomputed each frame from `ops` and elapsed time).
#[derive(Component)]
pub(crate) struct AnimatedUv {
    material: Handle<StandardMaterial>,
    ops: Arc<[UvOp]>,
}

impl Plugin for ViewerPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ViewerConfig {
            options: self.options.clone(),
        })
        .insert_resource(ClearColor(Color::srgb(0.45, 0.62, 0.82)))
        .insert_resource(RunState {
            start: Instant::now(),
            frame: 0,
            shot: 0,
            shot_done: false,
            target_at: None,
            picked: String::new(),
        })
        .init_resource::<ShotFlag>()
        .init_resource::<PickData>()
        .insert_resource(fog::FogDisabled(fog::fog_disabled()))
        .add_plugins(particles::ParticlePlugin)
        .add_plugins(MaterialPlugin::<lights::ReceiverMaterial>::default())
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (
                fly_look,
                fly_move,
                sky_follow,
                animate_uv,
                lights::update_scene_lights,
                fog::update_fog,
                pick,
                overlay,
                unattended,
            )
                .chain(),
        );
    }
}

/// Composes the UV transform chain at time `t` seconds. The composition order (outer op times
/// the accumulated inner transform) is a **hypothesis**: the exact UE2 texture-matrix order is
/// not verified, and almost every surface material has a single operation.
fn uv_affine(ops: &[UvOp], t: f32) -> Affine2 {
    let tau = std::f32::consts::TAU;
    let mut out = Affine2::IDENTITY;
    for op in ops {
        let m = match *op {
            UvOp::Pan { speed_u, speed_v } => {
                Affine2::from_translation(Vec2::new(speed_u * t, speed_v * t))
            }
            UvOp::Rotate {
                base,
                rate,
                center_u,
                center_v,
            } => {
                let angle = base + rate * t;
                Affine2::from_translation(Vec2::new(center_u, center_v))
                    * Affine2::from_angle(angle)
                    * Affine2::from_translation(Vec2::new(-center_u, -center_v))
            }
            UvOp::Scale { scale_u, scale_v } => Affine2::from_scale(Vec2::new(scale_u, scale_v)),
            UvOp::OscillatePan {
                amplitude_u,
                amplitude_v,
                rate_u,
                rate_v,
                phase_u,
                phase_v,
            } => Affine2::from_translation(Vec2::new(
                amplitude_u * (tau * rate_u * t + phase_u).sin(),
                amplitude_v * (tau * rate_v * t + phase_v).sin(),
            )),
            UvOp::OscillateScale {
                amplitude_u,
                amplitude_v,
                rate_u,
                rate_v,
                phase_u,
                phase_v,
            } => Affine2::from_scale(Vec2::new(
                1.0 + amplitude_u * (tau * rate_u * t + phase_u).sin(),
                1.0 + amplitude_v * (tau * rate_v * t + phase_v).sin(),
            )),
        };
        out = m * out;
    }
    out
}

/// Maps a resolved blend to Bevy's [`AlphaMode`]. `Darken` and `Invisible` have no exact Bevy
/// equivalent; `Darken` uses multiply, `Invisible` is approximated by a fully transparent blend.
fn alpha_mode(blend: BlendMode) -> AlphaMode {
    match blend {
        BlendMode::Opaque | BlendMode::Unsupported => AlphaMode::Opaque,
        BlendMode::Masked(t) => AlphaMode::Mask(t),
        BlendMode::Alpha | BlendMode::Invisible => AlphaMode::Blend,
        BlendMode::Additive => AlphaMode::Add,
        BlendMode::Modulate | BlendMode::Darken => AlphaMode::Multiply,
    }
}

/// Builds the unlit diagnostic material for a resolved material. Baked vertex colours still
/// modulate the texture; this only adds blend, two-sidedness and the initial UV transform.
fn standard_material(resolved: &ResolvedMaterial, texture: Handle<Image>) -> StandardMaterial {
    let base_color = resolved
        .color_tint
        .map_or(Color::WHITE, |c| Color::linear_rgba(c[0], c[1], c[2], c[3]));
    StandardMaterial {
        base_color,
        base_color_texture: Some(texture),
        unlit: true,
        alpha_mode: alpha_mode(resolved.blend),
        cull_mode: if resolved.two_sided {
            None
        } else {
            Some(Face::Back)
        },
        uv_transform: uv_affine(&resolved.uv_transform, 0.0),
        ..default()
    }
}

/// Recomputes every animated material's `uv_transform` from the elapsed time. Shared by the map
/// viewer and `--play` (so animated material chains move in both).
pub(crate) fn animate_uv(
    time: Res<Time>,
    animated: Query<&AnimatedUv>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = Instant::now();
    let t = time.elapsed_secs();
    for a in &animated {
        if let Some(mut mat) = materials.get_mut(&a.material) {
            mat.uv_transform = uv_affine(&a.ops, t);
        }
    }
    perf.span("animate_uv", t0);
}

fn image_from(t: &xiii_world::SceneTexture) -> Image {
    let mut img = Image::new(
        Extent3d {
            width: t.image.width,
            height: t.image.height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        t.image.pixels.clone(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        ..ImageSamplerDescriptor::linear()
    });
    img
}

pub(crate) fn transform_from(t: &xiii_decode::common::BevyTransform) -> Transform {
    let m = Mat3::from_cols_array(&xiii_world::to_cols(&t.rotation));
    Transform {
        translation: Vec3::from_array(t.translation),
        rotation: Quat::from_mat3(&m),
        scale: Vec3::from_array(t.scale),
    }
}

/// A copy of `m` with a per-vertex baked colour stream (RGBA8). Bevy's `StandardMaterial`
/// multiplies the texture by `ATTRIBUTE_COLOR`; the stored lighting value is used as a linear
/// factor (byte/255).
fn colored_mesh(m: &xiii_world::SceneMesh, colors: &[[u8; 4]]) -> Mesh {
    debug_assert_eq!(m.positions.len(), colors.len());
    let attr: Vec<[f32; 4]> = colors
        .iter()
        .map(|c| {
            [
                c[0] as f32 / 255.0,
                c[1] as f32 / 255.0,
                c[2] as f32 / 255.0,
                1.0,
            ]
        })
        .collect();
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, m.positions.clone());
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, m.normals.clone());
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, m.uvs.clone());
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, attr);
    mesh.insert_indices(Indices::U32(m.indices.clone()));
    mesh
}

/// Sky-camera position of the map's first sky zone (`SkyZoneInfo` location, Bevy metres), or
/// `None` when the map has no readable sky zone. Shared by the map viewer and `--play`.
pub(crate) fn scene_sky_position(scene: &WorldScene) -> Option<Vec3> {
    scene
        .sky_zones
        .first()
        .and_then(|z| scene.zones.get(*z as usize))
        .and_then(|z| z.location)
        .map(Vec3::from_array)
}

/// First sky zone index of a scene, for the sky camera's fog selection.
pub(crate) fn scene_sky_zone(scene: &WorldScene) -> Option<u8> {
    scene.sky_zones.first().map(|z| *z as u8)
}

/// Whether the sky camera should be spawned: a sky zone exists and `XIII_VIEWER_NO_SKY` is
/// unset (`XIII_VIEWER_NO_SKY` disables it for before/after comparison captures).
pub(crate) fn sky_camera_enabled(position: &Option<Vec3>) -> bool {
    position.is_some() && std::env::var_os("XIII_VIEWER_NO_SKY").is_none()
}

/// Main-camera configuration when a sky camera is present: drawn after (order 1), no colour
/// clear (the sky shows through) but depth still cleared so playable geometry draws on top.
pub(crate) fn main_camera_config(sky_enabled: bool) -> Camera {
    Camera {
        order: if sky_enabled { 1 } else { 0 },
        clear_color: if sky_enabled {
            ClearColorConfig::None
        } else {
            ClearColorConfig::Default
        },
        ..default()
    }
}

/// Spawns the second, sky-only camera at a fixed sky-zone position (its rotation is copied from
/// the main camera every frame by [`sky_follow`]). Shared by the map viewer and `--play`.
pub(crate) fn spawn_sky_camera(commands: &mut Commands, position: Vec3, sky_zone: Option<u8>) {
    commands.spawn((
        Camera3d::default(),
        Camera {
            order: 0,
            clear_color: ClearColorConfig::Default,
            ..default()
        },
        RenderLayers::layer(SKY_LAYER),
        DistanceFog::default(),
        AmbientLight::default(),
        Transform::from_translation(position),
        SkyCamera { position, sky_zone },
    ));
}

/// Imports `--map` from `--game-dir` (read-only). Shared with the `--play` prototype.
pub(crate) fn load_scene(opts: &Options) -> Result<WorldScene, String> {
    let game_dir = opts
        .game_dir
        .clone()
        .ok_or("--game-dir is required for --map")?;
    let map = opts.map.clone().ok_or("--map is required")?;
    let mut cache = xiii_world::PackageCache::open(&game_dir)?;
    xiii_world::import_map(&mut cache, &map)
}

/// Builds and spawns every imported mesh with its unlit diagnostic material. Shared by the map
/// viewer and the `--play` prototype so the two scene-building paths cannot drift.
#[allow(clippy::too_many_arguments)]
pub(crate) fn spawn_scene_geometry(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    receiver_materials: &mut Assets<lights::ReceiverMaterial>,
    images: &mut Assets<Image>,
    scene: &WorldScene,
    baked: bool,
    force_particles: bool,
    force_lights: bool,
) -> (Vec<Entity>, Vec<Handle<Image>>) {
    let image_handles: Vec<Handle<Image>> = scene
        .textures
        .iter()
        .map(|t| images.add(image_from(t)))
        .collect();
    // The additive light-receiver pass is spawned when dynamic lights can appear: always in
    // `--play` (the VM can spawn lights), and in the viewer when the map has a drawable light.
    let receivers = !lights::lights_disabled()
        && (force_lights || scene.lights.iter().any(lights::is_render_dynamic));
    let receiver_mats = if receivers {
        lights::receiver_materials(receiver_materials, images, &image_handles, scene)
    } else {
        std::collections::HashMap::new()
    };
    let missing = materials.add(StandardMaterial {
        base_color: Color::srgb(1.0, 0.0, 1.0),
        unlit: true,
        ..default()
    });
    // One material per distinct resolved material (not per texture): blend, two-sidedness and
    // UV animation are properties of the material object, not of the image alone.
    let mut material_cache: std::collections::HashMap<usize, Handle<StandardMaterial>> =
        Default::default();
    let mut mesh_handles = Vec::with_capacity(scene.meshes.len());
    let mut mat_handles = Vec::with_capacity(scene.meshes.len());
    for m in &scene.meshes {
        let mut mesh = Mesh::new(
            PrimitiveTopology::TriangleList,
            RenderAssetUsages::RENDER_WORLD,
        );
        mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, m.positions.clone());
        mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, m.normals.clone());
        mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, m.uvs.clone());
        mesh.insert_indices(Indices::U32(m.indices.clone()));
        mesh_handles.push(meshes.add(mesh));
        let resolved = &scene.materials[m.material_index];
        let mat = match resolved.base {
            Some(t) => material_cache
                .entry(m.material_index)
                .or_insert_with(|| {
                    materials.add(standard_material(resolved, image_handles[t].clone()))
                })
                .clone(),
            None => missing.clone(),
        };
        mat_handles.push(mat);
    }
    let sky_zone_set: HashSet<u32> = scene.sky_zones.iter().copied().collect();
    let mut entities = Vec::with_capacity(scene.objects.len());
    for o in &scene.objects {
        let transform = transform_from(&o.transform);
        // Sky-zone geometry goes on the sky-only layer; everything else (including terrain)
        // stays on the main layer, so neither camera draws the other view's geometry.
        let is_sky = o.zone.is_some_and(|z| sky_zone_set.contains(&z));
        let layer = if is_sky { SKY_LAYER } else { MAIN_LAYER };
        // Baked lighting is per placed object: give it a private mesh carrying the colour
        // stream, leaving the shared asset mesh uncoloured.
        let handle = match (&o.colors, baked) {
            (Some(colors), true) => meshes.add(colored_mesh(&scene.meshes[o.mesh], colors)),
            _ => mesh_handles[o.mesh].clone(),
        };
        let entity = commands
            .spawn((
                Mesh3d(handle),
                MeshMaterial3d(mat_handles[o.mesh].clone()),
                RenderLayers::layer(layer),
                transform,
                Name::new(o.path.clone()),
            ))
            .id();
        // Animated UV transforms are per material object, not per image; several entities may
        // share the same handle (the update system recomputes the same value for each).
        let ops = &scene.materials[scene.meshes[o.mesh].material_index].uv_transform;
        if !ops.is_empty() {
            commands.entity(entity).insert(AnimatedUv {
                material: mat_handles[o.mesh].clone(),
                ops: Arc::from(ops.clone()),
            });
        }
        // Light-only additive receiver: the same (uncoloured) geometry with a lit white
        // transparent material, so dynamic lights add to the baked unlit pass.
        if let Some(recv_mat) = receiver_mats.get(&scene.meshes[o.mesh].material_index) {
            commands.spawn((
                Mesh3d(mesh_handles[o.mesh].clone()),
                MeshMaterial3d(recv_mat.clone()),
                RenderLayers::layer(layer),
                transform,
                lights::LightReceiver,
                Name::new(format!("lightrecv {}", o.path)),
            ));
        }
        entities.push(entity);
    }
    particles::spawn_particles(
        commands,
        meshes,
        materials,
        images,
        &image_handles,
        scene,
        force_particles,
    );
    (entities, image_handles)
}

#[allow(clippy::too_many_arguments)]
fn setup(
    mut commands: Commands,
    cfg: Res<ViewerConfig>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut receiver_materials: ResMut<Assets<lights::ReceiverMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut decal_materials: ResMut<Assets<ForwardDecalMaterial<StandardMaterial>>>,
    mut pick: ResMut<PickData>,
    mut exit: MessageWriter<AppExit>,
) {
    let started = Instant::now();
    let scene = match load_scene(&cfg.options) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[viewer] import failed: {e}");
            exit.write(AppExit::error());
            return;
        }
    };
    let load_time = started.elapsed();

    let baked = cfg.options.lighting == Lighting::Baked;
    let baked_objects = if baked {
        scene.objects.iter().filter(|o| o.colors.is_some()).count()
    } else {
        0
    };
    let (geometry, image_handles) = spawn_scene_geometry(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut receiver_materials,
        &mut images,
        &scene,
        baked,
        cfg.options.particles == crate::cli::Particles::All,
        false,
    );
    let dynamic_lights = lights::spawn_scene_lights(&mut commands, &scene);
    commands.insert_resource(lights::LightRenderData {
        lights: scene.lights.clone(),
    });
    // Projector decals: one per static map-placed `Projector`/`ShadowProjector`.
    let projection_assets =
        decals::setup_projector_assets(&mut images, &image_handles, &scene, &mut decal_materials);
    let decals_spawned = decals::spawn_static_projectors(
        &mut commands,
        &scene,
        &projection_assets,
        &mut decal_materials,
    );
    commands.insert_resource(projection_assets);
    commands.insert_resource(fog::FogContext::new(&scene));
    let scene_tris: usize = scene
        .objects
        .iter()
        .map(|o| scene.meshes[o.mesh].indices.len() / 3)
        .sum();
    commands.insert_resource(crate::perf::RenderStats::new(
        scene.objects.len(),
        geometry.len(),
        meshes.len(),
        materials.len(),
        scene_tris,
    ));
    for o in &scene.objects {
        // CPU triangles for picking.
        let transform = transform_from(&o.transform);
        let m = &scene.meshes[o.mesh];
        let mat = transform.to_matrix();
        let world: Vec<Vec3> = m
            .positions
            .iter()
            .map(|p| mat.transform_point3(Vec3::from_array(*p)))
            .collect();
        let (mut min, mut max) = (Vec3::splat(f32::MAX), Vec3::splat(f32::MIN));
        for p in &world {
            min = min.min(*p);
            max = max.max(*p);
        }
        let triangles = m
            .indices
            .as_chunks::<3>()
            .0
            .iter()
            .map(|t| {
                [
                    world[t[0] as usize],
                    world[t[1] as usize],
                    world[t[2] as usize],
                ]
            })
            .collect();
        pick.objects.push(PickObject {
            path: o.path.clone(),
            min,
            max,
            triangles,
        });
    }

    // Camera: --view overrides; otherwise the PlayerStart (eye ~1.2 m above its origin).
    let (pos, yaw, pitch) = match (cfg.options.view, scene.player_start) {
        (Some(v), _) => (
            Vec3::new(v[0], v[1], v[2]),
            v[3].to_radians(),
            v[4].to_radians(),
        ),
        (None, Some((p, rot))) => {
            let (yaw, pitch) = xiii_world::rotator_to_yaw_pitch(rot);
            (Vec3::from_array(p) + Vec3::Y * 1.2, yaw, pitch)
        }
        (None, None) => (Vec3::new(0.0, 5.0, 0.0), 0.0, 0.0),
    };
    // Sky zone: a second camera at the SkyZoneInfo location, sharing the main rotation. The
    // main camera then does not clear colour (the sky shows through) but still clears depth
    // (its `Camera3d` default), so playable geometry is drawn on top of the sky.
    // `XIII_VIEWER_NO_SKY` disables the sky camera for before/after comparison captures.
    let sky_position = scene_sky_position(&scene);
    let sky_enabled = sky_camera_enabled(&sky_position);
    // The main camera carries its zone's fog/ambient each frame (`fog::update_fog`) and the
    // depth prepass the forward decals need.
    let start_params = scene
        .fog
        .params_at(pos.to_array(), scene_sky_zone(&scene))
        .cloned()
        .unwrap_or_else(xiii_world::fog::FogParams::none);
    commands.spawn((
        Camera3d::default(),
        main_camera_config(sky_enabled),
        RenderLayers::layer(MAIN_LAYER),
        DepthPrepass,
        fog::distance_fog(&start_params),
        fog::ambient_light(&start_params).unwrap_or_else(|| AmbientLight {
            color: Color::NONE,
            brightness: 0.0,
            ..default()
        }),
        Transform::from_translation(pos).with_rotation(Quat::from_euler(
            EulerRot::YXZ,
            yaw,
            pitch,
            0.0,
        )),
        FlyCam {
            yaw,
            pitch,
            speed: 8.0,
        },
    ));
    if sky_enabled && let Some(sky_position) = sky_position {
        spawn_sky_camera(&mut commands, sky_position, scene_sky_zone(&scene));
    }
    let mut lines = Vec::new();
    let tris: usize = scene
        .objects
        .iter()
        .map(|o| scene.meshes[o.mesh].indices.len() / 3)
        .sum();
    lines.push(format!(
        "objects {} | meshes {} | textures {} | triangles {} | import {:.2}s",
        scene.objects.len(),
        scene.meshes.len(),
        scene.textures.len(),
        tris,
        load_time.as_secs_f32()
    ));
    lines.push(format!(
        "lighting {} | placed objects with baked vertex colours {}",
        if baked { "baked" } else { "off" },
        baked_objects
    ));
    let two_sided = scene.materials.iter().filter(|m| m.two_sided).count();
    let uv_animated = scene
        .materials
        .iter()
        .filter(|m| !m.uv_transform.is_empty())
        .count();
    let unsupported: usize = scene.materials.iter().map(|m| m.unsupported.len()).sum();
    lines.push(format!(
        "materials {} | two-sided {} | uv-animated {} | unsupported features {}",
        scene.materials.len(),
        two_sided,
        uv_animated,
        unsupported
    ));
    for (k, v) in &scene.counters {
        lines.push(format!("{v:>6} {k}"));
    }
    for z in &scene.zones {
        let p = scene.fog.params_for_zone(z.index as u8);
        let fog = match p {
            Some(f) if f.is_fogged() => {
                let (s, e) = f.distances_m().unwrap_or((0.0, 0.0));
                format!(
                    "fog {s:.1}-{e:.1} m bgr({:.2},{:.2},{:.2})",
                    f.to_bgr()[0],
                    f.to_bgr()[1],
                    f.to_bgr()[2]
                )
            }
            Some(_) => "fog off".to_owned(),
            None => "fog ?".to_owned(),
        };
        let amb = p
            .and_then(|f| f.ambient.linear_rgb())
            .map(|c| format!("ambient({:.2},{:.2},{:.2})", c[0], c[1], c[2]))
            .unwrap_or_else(|| "ambient none".to_owned());
        lines.push(format!(
            "zone {} {} {} | polygons {} objects {} | {fog} | {amb}{}",
            z.index,
            if z.is_sky { "SKY" } else { "playable" },
            z.actor_path.as_deref().unwrap_or("(none)"),
            z.polygon_count,
            z.object_count,
            z.location
                .map(|l| format!(" @ ({:.1}, {:.1}, {:.1}) m", l[0], l[1], l[2]))
                .unwrap_or_default()
        ));
    }
    lines.push(format!(
        "fog zones {} (fogged {} unfogged {} from-class-default {} disabled-by-map {}) | projectors map-placed {} (shadow {}) decals spawned {}",
        scene.fog.params.len(),
        scene.fog.params.iter().filter(|p| p.is_fogged()).count(),
        scene.fog.unfogged,
        scene.fog.from_class_default,
        scene.fog.disabled_by_map,
        scene.projectors.len(),
        scene
            .projectors
            .iter()
            .filter(|p| xiii_world::projectors::ProjectorDef::is_shadow_class(&p.def.class_path))
            .count(),
        decals_spawned,
    ));
    lines.push(format!(
        "dynamic lights {} of {} map lights | light-only additive receiver pass {}",
        dynamic_lights,
        scene.lights.len(),
        if lights::lights_disabled() {
            "disabled (XIII_VIEWER_NO_LIGHTS)"
        } else {
            "enabled"
        }
    ));
    if let Some(p) = sky_position {
        lines.push(format!(
            "sky camera at ({:.1}, {:.1}, {:.1}) m",
            p.x, p.y, p.z
        ));
    }
    println!("[viewer] map {:?}", cfg.options.map);
    for l in &lines {
        println!("[viewer] {l}");
    }
    for (k, ex) in &scene.examples {
        println!("[viewer] example {k}: {ex}");
    }
    if let Some((p, r)) = scene.player_start {
        println!("[viewer] player start (bevy m) {p:?} rotator {r:?}");
    }
    println!(
        "[viewer] camera start {pos:?} yaw {:.1} pitch {:.1}",
        yaw.to_degrees(),
        pitch.to_degrees()
    );
    if let Some(z) = scene.fog.zone_of_point(pos.to_array()) {
        match scene.fog.params_for_zone(z) {
            Some(p) if p.is_fogged() => {
                let (s, e) = p.distances_m().unwrap_or((0.0, 0.0));
                println!(
                    "[viewer] camera start zone {z} fog {s:.1}-{e:.1} m bgr({:.2},{:.2},{:.2})",
                    p.to_bgr()[0],
                    p.to_bgr()[1],
                    p.to_bgr()[2]
                );
            }
            _ => println!("[viewer] camera start zone {z} has no fog"),
        }
    } else {
        println!("[viewer] camera start point is outside the BSP zone tree");
    }
    commands.insert_resource(ImportSummary {
        title: format!(
            "XIII map viewer (diagnostic) - {}",
            cfg.options.map.as_deref().unwrap_or("?")
        ),
        problems: scene.problem_total(),
        lines,
    });

    commands.spawn((
        OverlayText,
        Text::new("loading"),
        TextFont {
            font_size: FontSize::Px(13.0),
            ..default()
        },
        TextColor(Color::srgb(0.95, 0.95, 0.85)),
        Node {
            position_type: PositionType::Absolute,
            top: px(6),
            left: px(6),
            padding: UiRect::all(px(5)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
    ));
    // Crosshair.
    commands.spawn((
        Text::new("+"),
        TextFont {
            font_size: FontSize::Px(22.0),
            ..default()
        },
        TextColor(Color::srgb(1.0, 0.2, 0.2)),
        Node {
            position_type: PositionType::Absolute,
            top: percent(50),
            left: percent(50),
            margin: UiRect {
                left: px(-6),
                top: px(-13),
                ..default()
            },
            ..default()
        },
    ));
}

fn fly_look(
    buttons: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>,
    mut cams: Query<(&mut FlyCam, &mut Transform)>,
) {
    if buttons.just_pressed(MouseButton::Right) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
    if buttons.just_released(MouseButton::Right) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
    if !buttons.pressed(MouseButton::Right) || motion.delta == Vec2::ZERO {
        return;
    }
    for (mut cam, mut t) in &mut cams {
        cam.yaw -= motion.delta.x * 0.003;
        cam.pitch = (cam.pitch - motion.delta.y * 0.003).clamp(-1.54, 1.54);
        t.rotation = Quat::from_euler(EulerRot::YXZ, cam.yaw, cam.pitch, 0.0);
    }
}

fn fly_move(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut exit: MessageWriter<AppExit>,
    mut cams: Query<(&FlyCam, &mut Transform)>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        exit.write(AppExit::Success);
    }
    let mut axis = Vec3::ZERO;
    for (key, dir) in [
        (KeyCode::KeyW, Vec3::NEG_Z),
        (KeyCode::KeyS, Vec3::Z),
        (KeyCode::KeyA, Vec3::NEG_X),
        (KeyCode::KeyD, Vec3::X),
        (KeyCode::KeyQ, Vec3::NEG_Y),
        (KeyCode::KeyE, Vec3::Y),
    ] {
        if keys.pressed(key) {
            axis += dir;
        }
    }
    if axis == Vec3::ZERO {
        return;
    }
    let boost = if keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]) {
        5.0
    } else {
        1.0
    };
    for (cam, mut t) in &mut cams {
        let delta =
            (t.rotation * Vec3::new(axis.x, 0.0, axis.z) + Vec3::Y * axis.y).normalize_or_zero();
        t.translation += delta * cam.speed * boost * time.delta_secs();
    }
}

/// Keeps the sky camera at its fixed `SkyZoneInfo` position with the main camera's rotation
/// (UE2 renders the sky from the zone's location with the player view direction, no parallax;
/// the decoded `SkyZoneInfo` has no parallax property).
pub(crate) fn sky_follow(
    main: Query<&Transform, (With<Camera3d>, Without<SkyCamera>)>,
    mut sky: Query<(&mut Transform, &SkyCamera)>,
) {
    let Some(main) = main.iter().next() else {
        return;
    };
    for (mut t, cam) in &mut sky {
        t.translation = cam.position;
        t.rotation = main.rotation;
    }
}

fn ray_aabb(o: Vec3, d: Vec3, min: Vec3, max: Vec3) -> Option<f32> {
    let inv = d.recip();
    let t0 = (min - o) * inv;
    let t1 = (max - o) * inv;
    let tmin = t0.min(t1).max_element();
    let tmax = t0.max(t1).min_element();
    (tmax >= tmin.max(0.0)).then_some(tmin.max(0.0))
}

fn ray_tri(o: Vec3, d: Vec3, t: &[Vec3; 3]) -> Option<f32> {
    let e1 = t[1] - t[0];
    let e2 = t[2] - t[0];
    let p = d.cross(e2);
    let det = e1.dot(p);
    if det.abs() < 1e-9 {
        return None;
    }
    let inv = 1.0 / det;
    let s = o - t[0];
    let u = s.dot(p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = s.cross(e1);
    let v = d.dot(q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let dist = e2.dot(q) * inv;
    (dist > 0.0).then_some(dist)
}

fn pick(
    data: Res<PickData>,
    mut state: ResMut<RunState>,
    cams: Query<&Transform, With<FlyCam>>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = Instant::now();
    state.frame += 1;
    if state.frame % 6 != 1 {
        return;
    }
    let Ok(t) = cams.single() else { return };
    let (o, d) = (t.translation, t.rotation * Vec3::NEG_Z);
    let mut best: Option<(f32, &str)> = None;
    for obj in &data.objects {
        let Some(near) = ray_aabb(o, d, obj.min, obj.max) else {
            continue;
        };
        if best.is_some_and(|(b, _)| near > b) {
            continue;
        }
        for tri in &obj.triangles {
            if let Some(dist) = ray_tri(o, d, tri)
                && best.is_none_or(|(b, _)| dist < b)
            {
                best = Some((dist, &obj.path));
            }
        }
    }
    state.picked = match best {
        Some((dist, path)) => format!("{path} @ {dist:.1} m"),
        None => "-".into(),
    };
    perf.span("pick", t0);
}

fn overlay(
    summary: Option<Res<ImportSummary>>,
    state: Res<RunState>,
    cams: Query<&Transform, With<FlyCam>>,
    sky_cams: Query<&SkyCamera>,
    emitters: Query<&particles::ParticleEmitterRender>,
    mut perf: ResMut<crate::perf::Perf>,
    mut text: Query<&mut Text, With<OverlayText>>,
) {
    let t0 = Instant::now();
    let (Some(summary), Ok(mut text)) = (summary, text.single_mut()) else {
        return;
    };
    let cam = cams.single().map(|t| t.translation).unwrap_or_default();
    let sky = sky_cams
        .iter()
        .next()
        .map(|c| {
            format!(
                " | sky camera {:.1} {:.1} {:.1} m",
                c.position.x, c.position.y, c.position.z
            )
        })
        .unwrap_or_default();
    let mut s = format!(
        "{}\ncamera {:.1} {:.1} {:.1} m{sky} | problems (skip./fail. counters): {} | live particles {}\ncrosshair: {}\n",
        summary.title,
        cam.x,
        cam.y,
        cam.z,
        summary.problems,
        particles::live_particle_count(&emitters),
        state.picked
    );
    for l in &summary.lines {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str("WASD/QE move, Shift fast, RMB look, Esc quit. Baked vertex lighting modulates the texture (--lighting off to compare); magenta = unresolved material.");
    text.0 = s;
    perf.span("overlay", t0);
}

fn unattended(
    mut commands: Commands,
    cfg: Res<ViewerConfig>,
    mut state: ResMut<RunState>,
    flag: Res<ShotFlag>,
    mut perf: ResMut<crate::perf::Perf>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(secs) = cfg.options.exit_after_secs else {
        return;
    };
    let elapsed = state.start.elapsed().as_secs_f32();
    if flag.0 {
        state.shot_done = true;
    }
    if let Some(path) = &cfg.options.screenshot
        && state.shot == 0
        && elapsed >= secs * 0.75
    {
        let path: PathBuf = path.clone();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path))
            .observe(|_: On<ScreenshotCaptured>, mut f: ResMut<ShotFlag>| f.0 = true);
        state.shot = 1;
    }
    if elapsed < secs {
        return;
    }
    let reached = *state.target_at.get_or_insert_with(Instant::now);
    if state.shot == 1 && !state.shot_done && reached.elapsed() < Duration::from_secs(5) {
        return;
    }
    println!(
        "[viewer] exit after {:.1}s, {} frames, screenshot {}",
        elapsed,
        state.frame,
        match (&cfg.options.screenshot, state.shot_done) {
            (Some(p), true) => format!("saved {}", p.display()),
            (Some(p), false) => format!("NOT confirmed {}", p.display()),
            (None, _) => "none".into(),
        }
    );
    println!("[viewer] crosshair at exit: {}", state.picked);
    perf.request_final();
    exit.write(AppExit::Success);
}

#[cfg(test)]
mod uv_tests {
    use super::uv_affine;
    use bevy::prelude::Vec2;
    use xiii_world::materials::UvOp;

    #[test]
    fn pan_is_zero_at_t_zero_and_speed_times_t_after() {
        let ops = [UvOp::Pan {
            speed_u: 0.5,
            speed_v: -0.25,
        }];
        let p0 = uv_affine(&ops, 0.0).transform_point2(Vec2::new(0.3, 0.4));
        assert!((p0 - Vec2::new(0.3, 0.4)).length() < 1e-6);
        let p1 = uv_affine(&ops, 2.0).transform_point2(Vec2::new(0.0, 0.0));
        assert!((p1 - Vec2::new(1.0, -0.5)).length() < 1e-5, "{p1:?}");
    }

    #[test]
    fn rotation_keeps_the_pivot_fixed() {
        let ops = [UvOp::Rotate {
            base: 0.0,
            rate: 1.0,
            center_u: 0.5,
            center_v: 0.5,
        }];
        let c = uv_affine(&ops, 1.7).transform_point2(Vec2::new(0.5, 0.5));
        assert!((c - Vec2::new(0.5, 0.5)).length() < 1e-5, "{c:?}");
    }

    #[test]
    fn oscillating_scale_is_identity_at_zero_phase_and_t_zero() {
        let ops = [UvOp::OscillateScale {
            amplitude_u: 0.2,
            amplitude_v: 0.3,
            rate_u: 0.5,
            rate_v: 0.5,
            phase_u: 0.0,
            phase_v: 0.0,
        }];
        let p = uv_affine(&ops, 0.0).transform_point2(Vec2::new(0.25, 0.25));
        assert!((p - Vec2::new(0.25, 0.25)).length() < 1e-6, "{p:?}");
        // A quarter period of a sine is the positive amplitude peak.
        let q = uv_affine(&ops, 0.5).transform_point2(Vec2::new(1.0, 1.0));
        assert!(
            (q.x - 1.2).abs() < 1e-4 && (q.y - 1.3).abs() < 1e-4,
            "{q:?}"
        );
    }

    #[test]
    fn empty_chain_is_identity() {
        let p = uv_affine(&[], 12.0).transform_point2(Vec2::new(0.7, 0.1));
        assert!((p - Vec2::new(0.7, 0.1)).length() < 1e-6);
    }
}
