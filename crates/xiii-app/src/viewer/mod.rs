//! Viewer modes: the map importer diagnostic (this module) and the skinned-character
//! viewer (`skinned`).
//!
//! Diagnostic map viewer (M2a): imports a map from an owned installation through
//! `xiii-install` + `xiii-decode` and shows static-mesh actors, the level BSP and terrain
//! with unlit textured `StandardMaterial`s. A fly camera starts at the PlayerStart; the
//! overlay shows the object path under the crosshair and every import counter, including
//! skipped/failed categories. This is an importer diagnostic, not a playable mission.

pub mod skinned;

use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::mesh::Indices;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, PrimitiveTopology, TextureDimension, TextureFormat};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use crate::cli::Options;
use xiii_world::{AlphaKind, MaterialSlot, WorldScene};

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
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (fly_look, fly_move, sky_follow, pick, overlay, unattended).chain(),
        );
    }
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

fn transform_from(t: &xiii_decode::common::BevyTransform) -> Transform {
    let m = Mat3::from_cols_array(&xiii_world::to_cols(&t.rotation));
    Transform {
        translation: Vec3::from_array(t.translation),
        rotation: Quat::from_mat3(&m),
        scale: Vec3::from_array(t.scale),
    }
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
pub(crate) fn spawn_sky_camera(commands: &mut Commands, position: Vec3) {
    commands.spawn((
        Camera3d::default(),
        Camera {
            order: 0,
            clear_color: ClearColorConfig::Default,
            ..default()
        },
        RenderLayers::layer(SKY_LAYER),
        Transform::from_translation(position),
        SkyCamera { position },
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
pub(crate) fn spawn_scene_geometry(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    scene: &WorldScene,
) -> Vec<Entity> {
    let image_handles: Vec<Handle<Image>> = scene
        .textures
        .iter()
        .map(|t| images.add(image_from(t)))
        .collect();
    let missing = materials.add(StandardMaterial {
        base_color: Color::srgb(1.0, 0.0, 1.0),
        unlit: true,
        ..default()
    });
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
        let mat = match &m.material {
            MaterialSlot::Texture(t) => material_cache
                .entry(*t)
                .or_insert_with(|| {
                    let tex = &scene.textures[*t];
                    materials.add(StandardMaterial {
                        base_color_texture: Some(image_handles[*t].clone()),
                        unlit: true,
                        alpha_mode: match tex.alpha {
                            AlphaKind::Opaque => AlphaMode::Opaque,
                            AlphaKind::Mask => AlphaMode::Mask(0.5),
                            AlphaKind::Blend => AlphaMode::Blend,
                        },
                        ..default()
                    })
                })
                .clone(),
            MaterialSlot::Missing(_) => missing.clone(),
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
        let entity = commands
            .spawn((
                Mesh3d(mesh_handles[o.mesh].clone()),
                MeshMaterial3d(mat_handles[o.mesh].clone()),
                RenderLayers::layer(layer),
                transform,
                Name::new(o.path.clone()),
            ))
            .id();
        entities.push(entity);
    }
    entities
}

#[allow(clippy::too_many_arguments)]
fn setup(
    mut commands: Commands,
    cfg: Res<ViewerConfig>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
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

    let _geometry = spawn_scene_geometry(
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        &scene,
    );
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
    commands.spawn((
        Camera3d::default(),
        main_camera_config(sky_enabled),
        RenderLayers::layer(MAIN_LAYER),
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
        spawn_sky_camera(&mut commands, sky_position);
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
    for (k, v) in &scene.counters {
        lines.push(format!("{v:>6} {k}"));
    }
    for z in &scene.zones {
        lines.push(format!(
            "zone {} {} {} | polygons {} objects {}{}",
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

fn pick(data: Res<PickData>, mut state: ResMut<RunState>, cams: Query<&Transform, With<FlyCam>>) {
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
}

fn overlay(
    summary: Option<Res<ImportSummary>>,
    state: Res<RunState>,
    cams: Query<&Transform, With<FlyCam>>,
    sky_cams: Query<&SkyCamera>,
    mut text: Query<&mut Text, With<OverlayText>>,
) {
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
        "{}\ncamera {:.1} {:.1} {:.1} m{sky} | problems (skip./fail. counters): {}\ncrosshair: {}\n",
        summary.title, cam.x, cam.y, cam.z, summary.problems, state.picked
    );
    for l in &summary.lines {
        s.push_str(l);
        s.push('\n');
    }
    s.push_str("WASD/QE move, Shift fast, RMB look, Esc quit. Unlit diagnostic materials; magenta = unresolved material.");
    text.0 = s;
}

fn unattended(
    mut commands: Commands,
    cfg: Res<ViewerConfig>,
    mut state: ResMut<RunState>,
    flag: Res<ShotFlag>,
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
    exit.write(AppExit::Success);
}
