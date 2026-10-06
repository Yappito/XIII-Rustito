//! First-person movement prototype (`--play` / `--play-script`).
//!
//! This is a **movement prototype**, not gameplay: no script VM, no weapons and no AI. It
//! imports a map, spawns the player extent box at the PlayerStart with the same UE2-style
//! FindSpot raise-and-drop `--collision-test` uses, and walks it with the box-query collision
//! soup and `xiii_collision::walk_move`.
//!
//! The interactive mode is selected with `--play`; the deterministic headless mode with
//! `--play-script <file>`. Both drive the same [`sim::PlayerSim`] in `FixedUpdate` at 60 Hz.
//! Fixed 60 Hz is a **hypothesis** (UE2 used variable ticks); see [`FIXED_HZ`].

pub mod cartoon;
pub mod cinematics;
pub mod cutscene;
pub mod footsteps;
pub mod hud;
pub mod movement_modes;
pub mod movers;
pub mod pawns;
pub mod script;
pub mod session;
pub mod sim;
pub mod travel;
pub mod voice;
pub mod weapons;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use bevy::camera::visibility::RenderLayers;
use bevy::ecs::system::{NonSend, NonSendMut};
use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::mesh::skinning::SkinnedMeshInverseBindposes;
use bevy::pbr::decal::ForwardDecalMaterial;
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
use bevy::time::Fixed;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow, Window};

use xiii_collision::CollisionWorld;
use xiii_decode::common::{UNREAL_UNITS_PER_METER, to_bevy_direction, to_bevy_position};
use xiii_install::{Installation, OpenOptions};
use xiii_script::Value;
use xiii_world::physics::bevy_to_unreal_position;

use crate::cli::Options;
use crate::collision;
use crate::viewer;

use sim::{Input, PlayerParams, PlayerSim};

/// Fixed simulation rate in Hz. **Hypothesis**: UE2 ran variable-length ticks; 60 Hz is a
/// deterministic prototype choice, not a decoded XIII value.
pub const FIXED_HZ: f64 = 60.0;

/// Print a position trace every this many fixed ticks (0.5 s at 60 Hz).
const TRACE_EVERY: u64 = 30;

/// Player plugin for `--play`.
pub struct PlayPlugin {
    /// Parsed options (map, game dir, script, unattended settings).
    pub options: Options,
}

#[derive(Resource)]
pub(crate) struct PlayConfig {
    options: Options,
}

#[derive(Resource)]
struct ParamsRes(PlayerParams);

/// Decoded map movement volumes (water/ladder) adapted to the simulation query.
#[derive(Resource)]
struct MotionRes(movement_modes::VolumeMotion);

#[derive(Resource)]
struct SimRes(PlayerSim);

#[derive(Resource)]
struct WorldRes {
    world: CollisionWorld,
    sources: Vec<String>,
    movers: movers::MoverCollision,
}

/// Render entities per map actor (the part of the scene object path before `" -> "`), for the
/// one-way sync of actors the VM moves.
#[derive(Resource, Default)]
struct RenderSync {
    entities: HashMap<String, Vec<Entity>>,
}

#[derive(Resource)]
struct ScriptRes {
    drive: Option<script::Drive>,
}

/// Player footstep cadence for `--play` (surface lookup table + accumulator). See
/// [`footsteps`] for the evidence (notify-driven in the original; synthesised here because the
/// player pawn has no third-person animation).
#[derive(Resource)]
struct FootstepRes(footsteps::FootstepDriver);

#[derive(Resource)]
struct TraceState {
    tick: u64,
    saved_total: u64,
    start: Instant,
    exit_secs: Option<f32>,
    shot: u8,
    shot_done: bool,
    target_at: Option<Instant>,
    /// Set when a level transition completed: the unattended run screenshots the new map a short
    /// time later and exits, independent of the original budget.
    post_travel_at: Option<Instant>,
    /// item21: set when the in-game cutscene video began playing (unattended + screenshot runs
    /// only): the screenshot fires [`cutscene::VIDEO_SHOT_AFTER_SECS`] into playback and the run
    /// exits once the shot is confirmed, independent of the original budget.
    video_exit_at: Option<Instant>,
}

#[derive(Resource, Default)]
struct ShotFlag(bool);

#[derive(Component)]
pub(crate) struct PlayCam;

#[derive(Component)]
struct PlayOverlay;

/// Bevy light entities driven by live VM light actors: map-placed `TriggerLight`/
/// `ScriptedLight`/`MovableLight` and runtime-spawned lights such as the Beretta's
/// `XIII.MuzzleLight`. The VM owns the actors; this host map only mirrors them.
#[derive(Resource, Default)]
struct RuntimeLights {
    entities: HashMap<xiii_script::ObjectId, Entity>,
    /// Lights mirrored on the last sync (diagnostic overlay).
    active: usize,
    /// Total lights spawned since startup (diagnostic overlay).
    spawned: u64,
    /// Live VM actors of class `MuzzleLight` seen on the last sync (diagnostic overlay).
    muzzle_actors: usize,
    /// Live VM actors whose class name contains `Attach` (diagnostic overlay).
    attach_actors: usize,
}

impl Plugin for PlayPlugin {
    fn build(&self, app: &mut App) {
        // Load the script session before the window opens. `Session` holds `Rc`-based VM state
        // (it is `!Send`), so it lives in a non-send resource on the main thread; a load failure
        // is stored and reported by `setup`, which exits with an error.
        let mut options = self.options.clone();
        if let Some(slot) = options.load {
            match options
                .save_dir
                .clone()
                .map(Ok)
                .unwrap_or_else(crate::save::default_save_dir)
                .and_then(|dir| crate::save::SaveStore::open(dir).read(slot))
            {
                Ok(saved) => {
                    options.map = Some(saved.map);
                }
                Err(e) => {
                    eprintln!("[save] load slot {slot} failed: {e}");
                }
            }
        }
        let game_dir = options.game_dir.clone().unwrap_or_default();
        let map = options.map.clone().unwrap_or_default();
        let t0 = Instant::now();
        let mut session = if options.load.is_some() {
            session::Session::open_checkpoint(&game_dir, &map)
        } else {
            session::Session::open(&game_dir, &map)
        };
        println!(
            "[play] script session open (scripts, begin-play, providers): {:.2}s",
            t0.elapsed().as_secs_f32()
        );
        let cutscene_host = session.as_mut().ok().and_then(|s| {
            let dir = Path::new(&game_dir);
            if !dir.is_dir() {
                return None;
            }
            let host = cutscene::install(
                s.vm_mut(),
                dir,
                options.audio == crate::cli::Audio::On,
            );
            println!(
                "[play] item21 cutscene host installed (VideoPlayer clips are decoded and played fullscreen)"
            );
            Some(host)
        });
        if let Ok(s) = session.as_mut() {
            s.enable_native_timers(options.perf_natives);
        }
        app.insert_non_send(session);
        app.add_plugins((crate::video::CutscenePlugin, cutscene::CutsceneSystems));
        app.insert_non_send(cutscene::CutsceneHost(cutscene_host));
        app.insert_resource(PlayConfig { options })
            .insert_resource(ClearColor(Color::srgb(0.45, 0.62, 0.82)))
            .insert_resource(Time::<Fixed>::from_hz(FIXED_HZ))
            .init_resource::<ShotFlag>()
            .init_resource::<RenderSync>()
            .init_resource::<weapons::WeaponView>()
            .init_resource::<RuntimeLights>()
            .add_plugins(viewer::particles::ParticlePlugin)
            .add_plugins(MaterialPlugin::<viewer::lights::ReceiverMaterial>::default())
            .init_resource::<cinematics::CinematicState>()
            .init_resource::<cartoon::CartoonState>()
            .init_resource::<cartoon::CartoonRenderTarget>()
            .init_resource::<viewer::decals::RuntimeProjectorDecals>()
            .insert_resource(viewer::fog::FogDisabled(viewer::fog::fog_disabled()))
            .add_systems(Startup, setup)
            .add_systems(FixedUpdate, (fixed_step, travel).chain())
            .add_systems(
                Update,
                (
                    controls,
                    grab_cursor,
                    mouse_look,
                    cinematics::collect,
                    sync_camera,
                    cinematics::draw,
                    viewer::sky_follow,
                    viewer::animate_uv,
                    viewer::fog::update_fog,
                    viewer::decals::update_runtime_projectors,
                    sync_vm_particle_emitters,
                    (sync_vm_lights, viewer::lights::cull_receivers).chain(),
                    pawns::update_pawns,
                    weapons::update_weapon_view,
                    hud::refresh,
                    cartoon::collect,
                    cartoon::sync_render_target,
                    hud::draw,
                    overlay,
                    unattended,
                )
                    .chain(),
            )
            .add_systems(Last, (cinematics::report_exit, cartoon::report_exit));
    }
}

/// Resolved player parameters plus the startup report lines.
struct ResolvedParams {
    params: PlayerParams,
    lines: Vec<String>,
}

/// Resolves the player parameters from the inherited class defaults of the pawn class named by
/// `Default.ini` -> GameInfo `DefaultPlayerClassName` (the same resolution `--collision-test`
/// uses), and gravity from the decoded `Engine.PhysicsVolume.Gravity` class default. Every
/// value is reported with its source; missing optional values are reported as such.
fn resolve_params(game_dir: &Path) -> Result<ResolvedParams, String> {
    let install = Installation::open(game_dir, &OpenOptions::default())
        .map_err(|e| format!("opening installation for class defaults: {e}"))?;
    let (set, gameinfo, pawn_path) = collision::resolve_player_class(&install)?;
    let layout = collision::class_layout_of(&set, &pawn_path, None)?;
    let mut lines = Vec::new();
    lines.push(format!(
        "player class {pawn_path} via Default.ini DefaultGame={gameinfo} -> DefaultPlayerClassName"
    ));
    lines.push(format!(
        "  inheritance chain: {}",
        layout.chain_names.join(" <- ")
    ));
    let req = |name: &str| collision::layout_float(&layout, name);
    let opt = |name: &str| {
        let v = collision::layout_float_opt(&layout, name);
        v.is_finite().then_some(v)
    };
    let bool_opt = |name: &str| -> Option<bool> {
        let slot = layout.slot_by_name(name)?;
        match layout.defaults.get(slot.base) {
            Some(Value::Bool(b)) => Some(*b),
            _ => None,
        }
    };
    let radius = req("CollisionRadius")?;
    let height = req("CollisionHeight")?;
    let eye = req("BaseEyeHeight")?;
    let ground = req("GroundSpeed")?;
    let jump = req("JumpZ")?;
    let accel = opt("AccelRate");
    let air = opt("AirControl");
    let walking = opt("WalkingPct");
    let maxfall = opt("MaxFallSpeed");
    // Crouch/ladder/water properties decoded from the same inherited class defaults.
    let crouch_radius = opt("CrouchRadius").unwrap_or(radius);
    let crouch_height = opt("CrouchHeight").unwrap_or(height);
    let crouching_pct = opt("CrouchingPct").unwrap_or(0.3);
    let water_speed = opt("WaterSpeed").unwrap_or(300.0);
    let ladder_speed = opt("LadderSpeed").unwrap_or(200.0);
    let buoyancy = opt("Buoyancy").unwrap_or(0.0);
    let under_water_time = opt("UnderWaterTime").unwrap_or(0.0);
    let b_can_crouch = bool_opt("bCanCrouch").unwrap_or(true);
    let b_can_climb_ladders = bool_opt("bCanClimbLadders").unwrap_or(false);
    for (name, value, src) in [
        ("CollisionRadius", radius, "class-default"),
        ("CollisionHeight (half)", height, "class-default"),
        ("BaseEyeHeight", eye, "class-default"),
        ("GroundSpeed", ground, "class-default"),
        ("JumpZ", jump, "class-default"),
    ] {
        lines.push(format!("  {name} = {value} UU [{src}]"));
    }
    let opt_line = |name: &str, v: Option<f32>, when_missing: &str| match v {
        Some(x) => format!("  {name} = {x} [{when_missing}]"),
        None => format!("  {name} = not decoded ({when_missing})"),
    };
    lines.push(opt_line(
        "AccelRate",
        accel,
        if accel.is_some() {
            "class-default UU/s^2"
        } else {
            "instant acceleration"
        },
    ));
    lines.push(opt_line(
        "AirControl",
        air,
        if air.is_some() {
            "class-default fraction"
        } else {
            "0 (no air control)"
        },
    ));
    lines.push(opt_line(
        "WalkingPct",
        walking,
        if walking.is_some() {
            "class-default Shift multiplier"
        } else {
            "0.5 (upstream UE2 Pawn default)"
        },
    ));
    lines.push(opt_line(
        "MaxFallSpeed",
        maxfall,
        if maxfall.is_some() {
            "class-default UU/s"
        } else {
            "unclamped"
        },
    ));
    // Gravity: UE2 keeps it on PhysicsVolume (`ZoneGravity` lives there). The decoded
    // `Engine.PhysicsVolume` class default is the source; a placed per-map volume override is
    // not decoded here.
    let gravity = match collision::class_layout_of(&set, "Engine.PhysicsVolume", Some("Engine")) {
        Ok(pv) => collision::layout_vector(&pv, "Gravity"),
        Err(_) => None,
    };
    let (gravity, gravity_src) = match gravity {
        Some(g) => (g, "decoded Engine.PhysicsVolume.Gravity class default"),
        None => (
            [0.0, 0.0, -950.0],
            "upstream UE2 default (PhysicsVolume.Gravity not decoded)",
        ),
    };
    lines.push(format!(
        "  Gravity = ({}, {}, {}) UU/s^2 [{gravity_src}]",
        gravity[0], gravity[1], gravity[2]
    ));
    if let Ok(zi) = collision::class_layout_of(&set, "Engine.ZoneInfo", Some("Engine")) {
        lines.push(format!(
            "  ZoneInfo.ZoneGravity = {} (not decoded; UE2 keeps gravity on PhysicsVolume)",
            match collision::layout_vector(&zi, "ZoneGravity") {
                Some(v) => format!("({}, {}, {})", v[0], v[1], v[2]),
                None => "absent".to_owned(),
            }
        ));
    }
    // Crouch/ladder/water evidence, each with its decoded inherited default.
    lines.push(format!(
        "  CrouchHeight (half) = {crouch_height} UU, CrouchRadius = {crouch_radius} UU [class-default]"
    ));
    lines.push(format!(
        "  CrouchingPct = {crouching_pct}, WaterSpeed = {water_speed} UU/s, LadderSpeed = {ladder_speed} UU/s [class-default]"
    ));
    lines.push(format!(
        "  Buoyancy = {buoyancy}, UnderWaterTime = {under_water_time} s, bCanCrouch = {b_can_crouch}, bCanClimbLadders = {b_can_climb_ladders} [class-default]"
    ));
    lines.push(format!(
        "  fixed step {} Hz (hypothesis; UE2 used variable ticks); box half extents ({radius}, {height}, {radius}) UU",
        FIXED_HZ
    ));
    Ok(ResolvedParams {
        params: PlayerParams {
            radius_uu: radius,
            height_uu: height,
            crouch_radius_uu: crouch_radius,
            crouch_height_uu: crouch_height,
            base_eye_height_uu: eye,
            ground_speed: ground,
            jump_z: jump,
            accel_rate: accel,
            air_control: air,
            walking_pct: walking,
            crouching_pct,
            water_speed,
            ladder_speed,
            buoyancy,
            under_water_time,
            b_can_crouch,
            b_can_climb_ladders,
            max_fall_speed: maxfall,
            gravity_z: gravity[2],
        },
        lines,
    })
}

/// The tick spacing in seconds; the same value both modes print.
const DT: f32 = 1.0 / FIXED_HZ as f32;

#[allow(clippy::too_many_arguments)]
fn setup(
    mut commands: Commands,
    cfg: Res<PlayConfig>,
    mut session: NonSendMut<Result<session::Session, String>>,
    mut sync: ResMut<RenderSync>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut receiver_materials: ResMut<Assets<viewer::lights::ReceiverMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut bindposes: ResMut<Assets<SkinnedMeshInverseBindposes>>,
    mut decal_materials: ResMut<Assets<ForwardDecalMaterial<StandardMaterial>>>,
    mut exit: MessageWriter<AppExit>,
) {
    let session = match session.as_mut() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[play] script session failed: {e}");
            exit.write(AppExit::error());
            return;
        }
    };
    match setup_inner(
        &mut commands,
        &cfg.options,
        session,
        &mut sync,
        &mut meshes,
        &mut materials,
        &mut receiver_materials,
        &mut images,
        &mut bindposes,
        &mut decal_materials,
        false,
    ) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("[play] setup failed: {e}");
            exit.write(AppExit::error());
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn setup_inner(
    commands: &mut Commands,
    opts: &Options,
    session: &mut session::Session,
    sync: &mut RenderSync,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    receiver_materials: &mut Assets<viewer::lights::ReceiverMaterial>,
    images: &mut Assets<Image>,
    bindposes: &mut Assets<SkinnedMeshInverseBindposes>,
    decal_materials: &mut Assets<ForwardDecalMaterial<StandardMaterial>>,
    // True when this is a level-transition reload: the input-script cursor and the travel
    // timing in `TraceState` must survive, so they are not re-inserted.
    resetting: bool,
) -> Result<(), String> {
    let started = Instant::now();
    let game_dir = opts
        .game_dir
        .clone()
        .ok_or("--game-dir is required for --play")?;
    let scene = viewer::load_scene(opts)?;
    // Give the VM's physics provider the mover brushes too (the player simulation builds its own
    // dynamic collision below).
    session.register_movers(&scene);
    let resolved = resolve_params(&game_dir)?;
    let params = resolved.params;
    // Movement volumes (water/ladder brushes) for the crouch/ladder/swim/fall modes. A failed
    // import is reported, never silent; the modes then simply have no volumes.
    let motion = match xiii_world::movement_volumes::MovementVolumes::import(
        &game_dir,
        opts.map.as_deref().unwrap_or(""),
    ) {
        Ok(v) => {
            println!("[play] movement volumes: {}", v.summary());
            for d in &v.diagnostics {
                println!("[play]   volume diagnostic: {d}");
            }
            movement_modes::VolumeMotion::new(v)
        }
        Err(e) => {
            println!("[play] movement volumes unavailable ({e}); ladder/water modes disabled");
            movement_modes::VolumeMotion::default()
        }
    };
    println!(
        "[play] loaded {:?} in {:.2}s ({} objects, {} collision triangles in the box soup)",
        opts.map.as_deref().unwrap_or("?"),
        started.elapsed().as_secs_f32(),
        scene.objects.len(),
        scene.collision_box.len()
    );
    for l in &resolved.lines {
        println!("[play] {l}");
    }
    println!(
        "[play] script VM: {} live actors, {} active after level start; {}",
        session.live_actors(),
        session.active_actors(),
        session.bootstrap_note
    );
    println!("[play] player login path: script={}", session.login_script);
    println!("[play] hit boxes: {}", session.hitbox_summary());
    for e in &session.hitbox_errors {
        println!("[play]   hit-box mesh failed: {e}");
    }
    println!(
        "[play] localisation: language={} localized class-default overrides={}",
        session.localization_language, session.localized_overrides
    );
    println!(
        "[play] video clips: {} Bink header(s) read, {} timed (VideoPlayer.GetStatus uses the real duration)",
        session.video_clips, session.video_timed
    );
    println!("[play] objectives: {}", session.objective_summary());
    let pawns_now = session.player_pawn_actors();
    println!(
        "[play] player pawns: {} live XIIIPlayerPawn actor(s): {}",
        pawns_now.len(),
        pawns_now
            .iter()
            .map(|(_, n)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    for b in &session.blocked {
        println!("[play]   script path blocked: {b}");
    }

    let mover_states = session.mover_states();
    let (mut world, mover_collision) = movers::MoverCollision::build(&scene, &mover_states);
    mover_collision.update(&mut world, &mover_states);
    println!(
        "[play] movers: {} collision objects from {} live mover actors ({} static triangles): {}",
        mover_collision.count(),
        mover_states.len(),
        world.triangle_count(),
        mover_collision.names().join(", ")
    );
    let sources = scene.collision_sources.clone();
    let (ps_bevy, rot) = scene.player_start.ok_or("map has no PlayerStart")?;
    let spawn = collision::place_spawn(&world, ps_bevy, params.half_extents_bevy())?;
    println!(
        "[play] spawn: PlayerStart {:?} UU -> box centre {:?} UU, raise {:.2} UU, floor {:.2} UU below the centre",
        bevy_to_unreal_position(ps_bevy),
        bevy_to_unreal_position(spawn.position),
        spawn.raise * UNREAL_UNITS_PER_METER,
        (spawn.position[1] - spawn.floor) * UNREAL_UNITS_PER_METER
    );
    // The host movement simulation owns the player pawn's `Location`/`Velocity`/`Rotation`.
    // The script login chain (when it ran) created the pawn at the PlayerStart; the position
    // comes from the host's FindSpot placement (the raw PlayerStart overlaps the floor) and the
    // facing from the pawn's script-set Rotation. The host writes the placed position back to
    // the VM on the first tick. Same rule as the scripted path in `run_script`.
    let mut start_center = bevy_to_unreal_position(spawn.position);
    let mut start_rot = match (session.login_script, session.player_rotation()) {
        (1, Some(r)) => {
            println!(
                "[play] attaching host movement to the script-created pawn {} (VM location {:?} UU, rot {:?})",
                session.player_name,
                session.player_location(),
                r
            );
            r
        }
        _ => rot,
    };
    if let Some(slot) = opts.load {
        let dir = opts
            .save_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(crate::save::default_save_dir)?;
        let saved = crate::save::SaveStore::open(dir).read(slot)?;
        start_center = session.restore_checkpoint(&saved)?;
        start_rot = saved.rotation;
        println!(
            "[save] restored slot {slot}: map={} checkpoint={} health={} objectives={} inventory={}",
            saved.map,
            saved.checkpoint_number,
            saved.health,
            saved.objectives.len(),
            saved.inventory.len()
        );
    }
    let yaw = start_rot[1] as f32 * std::f32::consts::TAU / 65536.0;
    let mut sim = PlayerSim::new(start_center, yaw);
    sim.grounded = true;

    // Optional deterministic script.
    let (drive, script_last) = match &opts.play_script {
        Some(path) => {
            let script = script::Script::load(path)?;
            println!(
                "[play] input script {}: {} events, last at {:.2}s",
                path.display(),
                script.events.len(),
                script.last_time()
            );
            (Some(script::Drive::new(&script)), Some(script.last_time()))
        }
        None => (None, None),
    };
    let exit_secs = opts
        .exit_after_secs
        .or_else(|| script_last.map(|t| t + 2.0));

    // Render entities per map actor for the one-way sync of VM-moved actors. The scene object
    // path starts with the map export name (`Actor -> Package.Mesh`).
    let (geometry, image_handles) = viewer::spawn_scene_geometry(
        commands,
        meshes,
        materials,
        receiver_materials,
        images,
        &scene,
        opts.lighting == crate::cli::Lighting::Baked,
        opts.particles == crate::cli::Particles::All,
        true,
    );
    for (o, entity) in scene.objects.iter().zip(&geometry) {
        let actor = o
            .path
            .split_once(" -> ")
            .map_or(o.path.as_str(), |(a, _)| a)
            .to_owned();
        sync.entities.entry(actor).or_default().push(*entity);
    }
    // Fog table (per-zone, camera-selected each frame) and the static map projectors' decals.
    let projection_assets =
        viewer::decals::setup_projector_assets(images, &image_handles, &scene, decal_materials);
    let decals_spawned = viewer::decals::spawn_static_projectors(
        commands,
        &scene,
        &projection_assets,
        decal_materials,
    );
    commands.insert_resource(projection_assets);
    commands.insert_resource(viewer::decals::GroundQuery::from_scene(&scene));
    commands.insert_resource(viewer::fog::FogContext::new(&scene));
    println!(
        "[play] fog: {} zones ({} fogged, {} from class default, {} disabled by map); projectors: {} static ({decals_spawned} decals)",
        scene.fog.params.len(),
        scene.fog.params.iter().filter(|p| p.is_fogged()).count(),
        scene.fog.from_class_default,
        scene.fog.disabled_by_map,
        scene.projectors.len(),
    );
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

    let eye = to_bevy_position(sim.eye_location(&params));
    let sky_position = viewer::scene_sky_position(&scene);
    let sky_enabled = viewer::sky_camera_enabled(&sky_position);
    let start_params = scene
        .fog
        .params_at(eye, viewer::scene_sky_zone(&scene))
        .cloned()
        .unwrap_or_else(xiii_world::fog::FogParams::none);
    commands.spawn((
        Camera3d::default(),
        viewer::main_camera_config(sky_enabled),
        RenderLayers::layer(viewer::MAIN_LAYER),
        bevy::core_pipeline::prepass::DepthPrepass,
        viewer::fog::distance_fog(&start_params),
        viewer::fog::ambient_light(&start_params).unwrap_or_else(|| AmbientLight {
            color: Color::NONE,
            brightness: 0.0,
            ..default()
        }),
        Transform::from_translation(Vec3::from_array(eye)),
        PlayCam,
    ));
    if sky_enabled && let Some(p) = sky_position {
        viewer::spawn_sky_camera(commands, p, viewer::scene_sky_zone(&scene));
        println!(
            "[play] sky camera at ({:.1}, {:.1}, {:.1}) m from the map's sky zone",
            p.x, p.y, p.z
        );
    } else {
        println!("[play] no sky zone camera (no readable SkyZoneInfo)");
    }
    commands.spawn((
        PlayOverlay,
        Text::new("initialising"),
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
    println!(
        "[play] camera start {:?} m, yaw {:.1} deg | controls WASD, mouse look, Space jump, Shift walk, C crouch, E use, Esc quit",
        Vec3::from_array(eye),
        yaw.to_degrees()
    );
    // GPU-skinned map pawns, placed and posed from the VM every frame (player pawn excluded).
    let pawn_scene = pawns::setup_pawns(
        commands, session, &game_dir, meshes, materials, images, bindposes,
    );
    println!(
        "[play] pawns: {} rendered from {} decoded mesh(es); skipped {:?}{}",
        pawn_scene.instances.len(),
        pawn_scene.models,
        pawn_scene.skipped,
        if pawn_scene.attachments.is_empty() {
            String::new()
        } else {
            format!(
                "; attachments not rendered: {}",
                pawn_scene.attachments.join(", ")
            )
        }
    );
    if pawn_scene.bone_controls_not_applied > 0 {
        println!(
            "[play] pawns: {} carry item3g bone-controller state (SpineYawControl/SetBoneDirection) \
             which is not applied to the pose",
            pawn_scene.bone_controls_not_applied
        );
    }
    for inst in &pawn_scene.instances {
        let seq = session
            .vm()
            .actor_animation(inst.id)
            .and_then(|a| {
                a.channels
                    .iter()
                    .filter(|c| c.active)
                    .min_by_key(|c| c.channel)
                    .map(|c| c.sequence.clone())
            })
            .unwrap_or_else(|| "<bind>".to_owned());
        let mesh = session
            .vm()
            .mesh_object(inst.id)
            .map(|(p, _)| p)
            .unwrap_or_else(|| "?".to_owned());
        let loc = session
            .vm()
            .vector_prop(inst.id, "Location")
            .map(|l| format!("({:.1}, {:.1}, {:.1})", l[0], l[1], l[2]))
            .unwrap_or_else(|| "?".to_owned());
        let class = session
            .vm()
            .set()
            .path(session.vm().objects[inst.id as usize].class);
        println!(
            "[play]   pawn {} class {class} mesh {mesh} at {loc} UU, sequence {seq}",
            inst.name
        );
    }
    for f in &pawn_scene.failures {
        println!("[play] pawn mesh failed: {f}");
    }
    commands.insert_resource(pawn_scene);
    // Script-drawn HUD: decode the fonts, create the Canvas and install the VM font provider.
    let hud_runtime = hud::setup(session, game_dir.as_path(), images)?;
    commands.insert_resource(hud_runtime);
    commands.insert_resource(ParamsRes(params));
    commands.insert_resource(MotionRes(motion));
    commands.insert_resource(SimRes(sim));
    commands.insert_resource(footsteps::SurfaceSounds::from_scene(&scene));
    commands.insert_resource(FootstepRes(footsteps::FootstepDriver::new()));
    commands.insert_resource(WorldRes {
        world,
        sources,
        movers: mover_collision,
    });
    if !resetting {
        commands.insert_resource(ScriptRes { drive });
        commands.insert_resource(TraceState {
            tick: 0,
            saved_total: 0,
            start: Instant::now(),
            exit_secs,
            shot: 0,
            shot_done: false,
            target_at: None,
            post_travel_at: None,
            video_exit_at: None,
        });
    } else {
        // The reload owns a fresh per-map runtime, but the input-script cursor (`ScriptRes`) and
        // the unattended timer must survive; the screenshot step moves to a short post-travel
        // tail.
        let now = Instant::now();
        commands.queue(move |world: &mut World| {
            if let Some(mut st) = world.get_resource_mut::<TraceState>() {
                st.post_travel_at = Some(now);
                st.shot = 0;
                st.shot_done = false;
                st.target_at = None;
            }
        });
    }
    println!(
        "[play] setup complete in {:.2}s",
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

fn read_keyboard(keys: &ButtonInput<KeyCode>, buttons: &ButtonInput<MouseButton>) -> Input {
    let mut forward = 0.0;
    if keys.pressed(KeyCode::KeyW) {
        forward += 1.0;
    }
    if keys.pressed(KeyCode::KeyS) {
        forward -= 1.0;
    }
    let mut right = 0.0;
    if keys.pressed(KeyCode::KeyD) {
        right += 1.0;
    }
    if keys.pressed(KeyCode::KeyA) {
        right -= 1.0;
    }
    Input {
        forward,
        right,
        jump: keys.just_pressed(KeyCode::Space),
        walk: keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
        use_action: keys.just_pressed(KeyCode::KeyE),
        fire: buttons.just_pressed(MouseButton::Left),
        // XIII binds `C=Duck` (DefUser.ini); ControlLeft is accepted as the conventional
        // alternative the task names.
        crouch: keys.any_pressed([KeyCode::KeyC, KeyCode::ControlLeft]),
    }
}

/// Reach of the `use` action in metres. A little over the pawn's 34 UU radius so the player must
/// be at the door, matching the game's short interaction range.
const USE_REACH_M: f32 = 3.0;

/// Eye position and view direction of `sim` in Bevy space (metres).
fn use_ray(sim: &PlayerSim, params: &PlayerParams) -> ([f32; 3], [f32; 3]) {
    let eye = to_bevy_position(sim.eye_location(params));
    let (sy, cy) = sim.yaw.sin_cos();
    let (sp, cp) = sim.pitch.sin_cos();
    let fwd = [cp * cy, cp * sy, sp];
    (eye, to_bevy_direction(fwd))
}

/// Actor name (the part before `" -> "`) of the nearest collision hit along `origin + t*dir`.
fn ray_target(
    world: &CollisionWorld,
    sources: &[String],
    origin: [f32; 3],
    dir: [f32; 3],
    reach: f32,
) -> Option<String> {
    let end = [
        origin[0] + dir[0] * reach,
        origin[1] + dir[1] * reach,
        origin[2] + dir[2] * reach,
    ];
    let hit = world.ray(origin, end)?;
    let src = sources.get(hit.source as usize)?;
    Some(
        src.split_once(" -> ")
            .map_or_else(|| src.clone(), |(a, _)| a.to_owned()),
    )
}

/// Performs the `E`/`use` action: a forward ray picks the mover in front and runs the VM's own
/// lock/unlock/open chain ([`session::Session::use_mover`]).
fn perform_use(
    sess: &mut session::Session,
    world: &CollisionWorld,
    sources: &[String],
    sim: &PlayerSim,
    params: &PlayerParams,
) {
    let (origin, dir) = use_ray(sim, params);
    match ray_target(world, sources, origin, dir, USE_REACH_M) {
        Some(target) => {
            // A mover is used through its own lock/unlock/open chain; a dead pawn in front is
            // searched (the engine `Grab` interaction). `use_target` tries both, in that order.
            let outcome = sess.use_target(&target);
            println!("[play] use {target}: {outcome:?}");
        }
        None => println!("[play] use: nothing in reach"),
    }
}

/// One deterministic fixed step, driving the same [`PlayerSim`] as the headless mode.
///
/// Order (reviewer ownership decision, hypothesis against UE2's tick order): player input ->
/// player movement -> write pawn state to the VM -> VM touch update -> `Vm::tick` -> drain
/// presentation events -> sync VM-moved actors to the render transforms. [`Session::step`] owns
/// the VM half; this system owns the movement half.
#[allow(clippy::too_many_arguments)]
fn fixed_step(
    time: Res<Time<Fixed>>,
    mut sim: ResMut<SimRes>,
    params: Res<ParamsRes>,
    mut world: ResMut<WorldRes>,
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut script: ResMut<ScriptRes>,
    mut state: ResMut<TraceState>,
    mut session: NonSendMut<Result<session::Session, String>>,
    sync: Res<RenderSync>,
    motion: Res<MotionRes>,
    mut footsteps: ResMut<FootstepRes>,
    surfaces: Res<footsteps::SurfaceSounds>,
    cfg: Res<PlayConfig>,
    mut transforms: Query<&mut Transform>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let audio_enabled = cfg.options.audio == crate::cli::Audio::On;
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let elapsed = state.tick as f32 * DT;
    // Scripted cutscenes freeze the player (`CineController2.Interpret` FPC/FPL ->
    // `NoControl`/`NoMove`, and `CameraView`/`PlayingVideo`). The host owns the player pawn's
    // movement, so it must zero the movement input itself; the VM's state machine only sets the
    // state. See `cinematics`.
    let suppressed = match &*session {
        Ok(sess) => cinematics::input_suppressed(sess),
        Err(_) => false,
    };
    if let Some(drive) = script.drive.as_mut() {
        let tracked = drive.tracking_actor().and_then(|name| {
            (*session).as_ref().ok().and_then(|sess| {
                let id = sess.vm().find_live_object(name)?;
                Some((name.to_owned(), sess.vm().vector_prop(id, "Location")))
            })
        });
        if let Some((name, location)) = tracked {
            drive.set_track_location(Some(&name), location);
        } else {
            drive.set_track_location(None, None);
        }
    }
    // Always advance the script so an explicit `take_control` diagnostic can be read even while a cutscene
    // suppresses input; the axis input and the other command queues are dropped when suppressed.
    let (mut input, weapons, goals, equip, use_named, search) = match script.drive.as_mut() {
        Some(drive) => {
            let input = drive.advance(elapsed, &mut sim.0);
            (
                input,
                drive.take_weapons(),
                drive.take_goals(),
                drive.take_equip(),
                drive.take_use_named(),
                drive.take_search(),
            )
        }
        None => (
            read_keyboard(&keys, &buttons),
            Vec::new(),
            Vec::new(),
            false,
            Vec::new(),
            Vec::new(),
        ),
    };
    let control = script
        .drive
        .as_mut()
        .is_some_and(script::Drive::take_control);
    if suppressed {
        input = Input::default();
    }
    let (weapons, goals, equip, use_named, search) = if suppressed {
        (Vec::new(), Vec::new(), false, Vec::new(), Vec::new())
    } else {
        (weapons, goals, equip, use_named, search)
    };
    let use_action = input.use_action;
    let fire = input.fire;
    let t0 = Instant::now();
    if motion.0.is_empty() {
        sim.0
            .step(dt, &world.world, &params.0, input, &world.sources);
    } else {
        sim.0.step_with_modes(
            dt,
            &world.world,
            &params.0,
            input,
            &world.sources,
            &motion.0,
        );
    }
    perf.span("player_sim", t0);
    // Player footsteps: the original fires the `PlayFootStep` notify from the third-person walk
    // animation, which `--play` does not render, so the host synthesises it from the same
    // surface lookup the script would read from `LastCollidedMaterial`. Emit only when audio is
    // enabled; the queue is drained by the audio plugin.
    let t0 = Instant::now();
    if audio_enabled
        && let Some(step) =
            footsteps
                .0
                .advance(dt, &sim.0, &params.0, &world.world, &surfaces, input.walk)
    {
        crate::audio::queue_request(crate::audio::SoundRequest::footstep(
            state.tick as f64 * f64::from(DT),
            "XIIIPlayerPawn".to_owned(),
            step.sound,
        ));
    }
    perf.span("footsteps", t0);
    if let Ok(sess) = session.as_mut() {
        let t0 = Instant::now();
        let modes = session::PlayerVMModes {
            crouched: sim.0.crouched,
            in_water: sim.0.in_water,
            physics: sim.0.physics,
            landed_velocity_z: sim.0.landed.then_some(sim.0.land_velocity_z),
            floor_normal: sim.0.floor_normal,
        };
        sess.step(dt, sim.0.location, sim.0.yaw, sim.0.velocity, &modes);
        if let Some((location, yaw, velocity)) = sess.script_pawn_pose() {
            sim.0.location = location;
            sim.0.yaw = yaw;
            sim.0.velocity = velocity;
        }
        if sess.save_total > state.saved_total {
            state.saved_total = sess.save_total;
            if let Some((_, event)) = sess.saves.back() {
                let map = cfg.options.map.as_deref().unwrap_or("Plage00");
                let rot = sess.player_rotation().unwrap_or([0; 3]);
                let save_dir = cfg.options.save_dir.clone();
                let result = sess
                    .checkpoint_snapshot(map, event, sim.0.location, rot)
                    .and_then(|data| {
                        save_dir
                            .map(Ok)
                            .unwrap_or_else(crate::save::default_save_dir)
                            .and_then(|dir| {
                                let slot = (0..10)
                                    .find(|&n| !crate::save::exists(&dir, n))
                                    .unwrap_or(0);
                                crate::save::write(&dir, slot, &data).map(|()| (dir, slot))
                            })
                    });
                match result {
                    Ok((dir, slot)) => println!(
                        "[save] wrote slot {slot} ({}) to {}",
                        event.description,
                        dir.display()
                    ),
                    Err(e) => eprintln!("[save] checkpoint write failed: {e}"),
                }
            }
        }
        perf.span("vm_step", t0);
        // The VM owns the mover poses; write them into the dynamic collision set so the next
        // player step collides with the moved brush.
        let wr = &mut *world;
        let t0 = Instant::now();
        let mover_states = sess.mover_states();
        if sess.vm().native_profile().enabled {
            let micros = t0.elapsed().as_micros() as u64;
            sess.vm_mut().native_profile_mut().mover_states_micros += micros;
        }
        let t0 = Instant::now();
        wr.movers.update(&mut wr.world, &mover_states);
        perf.span("mover_collision", t0);
        for path in &weapons {
            match sess.grant_weapon(path) {
                Ok(msg) => println!("[play] weapon {msg}"),
                Err(e) => println!("[play] weapon grant failed {path}: {e}"),
            }
        }
        for n in &goals {
            if let Err(e) = sess.set_goal(*n) {
                println!("[play] set_goal {n} failed: {e}");
            }
        }
        if equip {
            match sess.equip_inventory_weapon() {
                Ok(msg) => println!("[play] equip {msg}"),
                Err(e) => println!("[play] equip failed: {e}"),
            }
        }
        if control {
            match sess.take_control() {
                Ok(state) => println!("[play] take_control diagnostic: controller -> {state}"),
                Err(e) => println!("[play] take_control failed: {e}"),
            }
        }
        if use_action {
            perform_use(sess, &wr.world, &wr.sources, &sim.0, &params.0);
        }
        for target in &use_named {
            let outcome = sess.use_target(target);
            println!("[play] use {target}: {outcome:?}");
        }
        for target in &search {
            let outcome = sess.search_corpse(target);
            println!("[play] search {target}: {outcome:?}");
        }
        if fire {
            match sess.fire(sim.0.yaw, sim.0.pitch) {
                // The weapon's own `IncrementFlashCount` -> `ThirdPersonEffects` chain spawns the
                // muzzle flash and calls `MuzzleLight.Flash`; the VM light sync renders it.
                session::FireOutcome::Fired => {
                    println!(
                        "[play] fire [{}] bone {} | {}",
                        state.tick as f32 * DT,
                        sess.vm().last_trace_bone(),
                        combat_snapshot(sess)
                    );
                }
                other => println!("[play] fire: {other:?}"),
            }
        }
        let t0 = Instant::now();
        crate::audio::pump(sess.events.iter());
        perf.span("audio_pump", t0);
        let t0 = Instant::now();
        for (name, delta) in &sess.moved {
            let Some(entities) = sync.entities.get(name) else {
                continue;
            };
            let d = Vec3::from_array(to_bevy_position(*delta));
            for &entity in entities {
                if let Ok(mut t) = transforms.get_mut(entity) {
                    t.translation += d;
                }
            }
        }
        perf.span("render_sync", t0);
    }
    state.tick += 1;
    perf.step();
    if state.tick.is_multiple_of(TRACE_EVERY) {
        println!("[play] {}", format_trace(state.tick, elapsed, &sim.0));
        if let Ok(sess) = session.as_ref() {
            println!("[play] {}", format_vm_trace(sess));
        }
    }
}

/// Mirrors the VM-owned `ParticleEmitter.Disabled` field into the renderer's simulator gate.
/// Map-exported non-Actor subobjects are instantiated by `Vm::load_level`, so authored
/// `TriggerEmit`/`TriggerToggle`/`TriggerControl` script writes are already the source of truth.
fn sync_vm_particle_emitters(
    mut session: NonSendMut<Result<session::Session, String>>,
    data: Res<viewer::particles::ParticleRenderData>,
    mut emitters: Query<&mut viewer::particles::ParticleEmitterRender>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = Instant::now();
    let Ok(sess) = session.as_mut() else {
        return;
    };
    let requests = sess.vm_mut().drain_particle_spawns();
    let mut requested_by_emitter = std::collections::HashMap::with_capacity(requests.len());
    for (id, amount) in requests {
        *requested_by_emitter.entry(id).or_insert(0usize) += amount;
    }
    let vm = sess.vm();
    for mut e in &mut emitters {
        let Some(system) = data.systems.get(e.system) else {
            continue;
        };
        let Some(desc) = system.emitters.get(e.emitter) else {
            continue;
        };
        if !e.vm_lookup_attempted {
            e.vm_id = vm.find_export_instance(&desc.name);
            e.vm_lookup_attempted = true;
        }
        let Some(id) = e.vm_id else {
            continue;
        };
        if let Some(xiii_script::Value::Bool(disabled)) = vm.get_property(id, "Disabled") {
            e.sim.set_enabled(!disabled);
        }
        let requested = requested_by_emitter.remove(&id).unwrap_or_default();
        e.sim.spawn_requested(desc, requested);
    }
    perf.span("particle_vm_sync", t0);
}

/// Mirrors the VM's live light actors to Bevy `PointLight` entities. Map-placed dynamic lights
/// (`TriggerLight`, `ScriptedLight`, `MovableLight`) and runtime lights (`XIII.MuzzleLight`) are
/// all found by class, so a muzzle flash and a scripted flicker use the same path. A light the
/// script turns off (`LightType == LT_None`) or that is despawned loses its entity, so it cannot
/// keep lighting the scene.
fn sync_vm_lights(
    mut commands: Commands,
    session: NonSend<Result<session::Session, String>>,
    mut state: ResMut<RuntimeLights>,
    mut lights: Query<(&mut PointLight, &mut Transform)>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = Instant::now();
    let Ok(sess) = session.as_ref() else {
        return;
    };
    if viewer::lights::lights_disabled() {
        for (_, entity) in state.entities.drain() {
            commands.entity(entity).despawn();
        }
        state.active = 0;
        return;
    }
    let vm = sess.vm();
    let time = sess.vm_time() as f32;
    let mut seen: std::collections::HashSet<xiii_script::ObjectId> =
        std::collections::HashSet::new();
    let mut muzzle_actors = 0usize;
    let mut attach_actors = 0usize;
    for i in 0..vm.objects.len() {
        let id = i as xiii_script::ObjectId;
        if !vm.objects[i].deleted {
            if vm.is_a(id, "MuzzleLight") {
                muzzle_actors += 1;
            }
            if vm.is_a(id, "MuzzleFlashAttachment") {
                attach_actors += 1;
            }
        }
        let Some(light) = viewer::lights::scene_light_from_vm(vm, id) else {
            continue;
        };
        if !light.render_dynamic() {
            continue;
        }
        seen.insert(id);
        let point = viewer::lights::point_light_for(&light, time);
        let position = Vec3::from_array(light.transform.translation);
        match state.entities.get(&id).copied() {
            Some(entity) => {
                if let Ok((mut point_light, mut transform)) = lights.get_mut(entity) {
                    *point_light = point;
                    transform.translation = position;
                }
            }
            None => {
                let entity = commands
                    .spawn((
                        point,
                        Transform::from_translation(position),
                        RenderLayers::layer(viewer::MAIN_LAYER),
                        Name::new(format!("vmlight {}", light.path)),
                    ))
                    .id();
                state.entities.insert(id, entity);
                state.spawned += 1;
            }
        }
    }
    state.entities.retain(|id, entity| {
        if seen.contains(id) {
            true
        } else {
            commands.entity(*entity).despawn();
            false
        }
    });
    state.active = state.entities.len();
    state.muzzle_actors = muzzle_actors;
    state.attach_actors = attach_actors;
    perf.span("vm_lights", t0);
}

/// One VM status line: time, active/suspended counts, dispatcher state, player VM position,
/// event count and last player touch.
fn format_vm_trace(sess: &session::Session) -> String {
    format!(
        "vm t={:.3}s active={} suspended={} suspended_dropped={} dispatcher={} ctrl={} player={} health={} physics={} events={} last_touch={} | {}",
        sess.vm_time(),
        sess.active_actors(),
        sess.suspended.len(),
        sess.vm().suspended_deferred_calls(),
        sess.dispatcher_state().unwrap_or_else(|| "-".to_owned()),
        sess.player_controller_state(),
        sess.player_location()
            .map(|l| format!("({:.1},{:.1},{:.1})", l[0], l[1], l[2]))
            .unwrap_or_else(|| "-".to_owned()),
        sess.player_health()
            .map(|h| h.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        sess.player_physics()
            .map(|p| p.to_string())
            .unwrap_or_else(|| "-".to_owned()),
        sess.total_events(),
        sess.last_touch().unwrap_or_else(|| "-".to_owned()),
        sess.objective_summary(),
    )
}

fn controls(keys: Res<ButtonInput<KeyCode>>, mut exit: MessageWriter<AppExit>) {
    if keys.just_pressed(KeyCode::Escape) {
        exit.write(AppExit::Success);
    }
}

fn grab_cursor(mut done: Local<bool>, mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>) {
    if *done {
        return;
    }
    cursor.grab_mode = CursorGrabMode::Locked;
    cursor.visible = false;
    *done = true;
}

fn mouse_look(
    buttons: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>,
    mut sim: ResMut<SimRes>,
) {
    if buttons.just_pressed(MouseButton::Left) && cursor.grab_mode != CursorGrabMode::Locked {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
    if cursor.grab_mode != CursorGrabMode::Locked || motion.delta == Vec2::ZERO {
        return;
    }
    let sens = 0.003;
    sim.0.yaw += motion.delta.x * sens;
    sim.0.pitch = (sim.0.pitch - motion.delta.y * sens).clamp(-1.54, 1.54);
}

fn sync_camera(
    sim: Res<SimRes>,
    params: Res<ParamsRes>,
    cine: Res<cinematics::CinematicState>,
    mut cams: Query<&mut Transform, With<PlayCam>>,
) {
    for mut t in &mut cams {
        if let Some(v) = &cine.view {
            // A script selected a cutscene camera (`CamView`/`ViewTarget`); render from it.
            let (loc, rot) = cinematics::camera_transform(v.location, v.rotation);
            t.translation = loc;
            t.rotation = rot;
        } else {
            let eye = to_bevy_position(sim.0.eye_location(&params.0));
            t.translation = Vec3::from_array(eye);
            t.rotation = Quat::from_euler(EulerRot::YXZ, -sim.0.yaw, sim.0.pitch, 0.0);
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn overlay(
    cfg: Res<PlayConfig>,
    sim: Res<SimRes>,
    session: NonSend<Result<session::Session, String>>,
    pawns: Option<Res<pawns::PawnScene>>,
    hud: Option<Res<hud::HudRuntime>>,
    projector_decals: Option<Res<viewer::decals::RuntimeProjectorDecals>>,
    fog_ctx: Option<Res<viewer::fog::FogContext>>,
    weapon_view: Option<Res<weapons::WeaponView>>,
    runtime_lights: Option<Res<RuntimeLights>>,
    mut perf: ResMut<crate::perf::Perf>,
    mut text: Query<&mut Text, With<PlayOverlay>>,
) {
    let t0 = Instant::now();
    let Ok(mut text) = text.single_mut() else {
        return;
    };
    let s = &sim.0;
    let vm = match &*session {
        Ok(sess) => format!(
            "VM t={:.2}s | actors active {} / live {} | suspended {} | dispatcher {}\n\
             last {} events: {}\nlast player touch: {}\nfirst error: {}",
            sess.vm_time(),
            sess.active_actors(),
            sess.live_actors(),
            sess.suspended.len(),
            sess.dispatcher_state().unwrap_or_else(|| "-".to_owned()),
            5,
            if sess.recent_events(5).is_empty() {
                "-".to_owned()
            } else {
                sess.recent_events(5).join(" | ")
            },
            sess.last_touch().unwrap_or_else(|| "-".to_owned()),
            sess.first_error()
                .map(|e| e.lines().take(3).collect::<Vec<_>>().join(" / "))
                .unwrap_or_else(|| "-".to_owned()),
        ),
        Err(e) => format!("VM unavailable: {e}"),
    };
    let pawns_line = match pawns.as_deref() {
        Some(p) if !p.instances.is_empty() => {
            let shown: Vec<String> = p
                .instances
                .iter()
                .take(8)
                .map(|i| {
                    let seq = i
                        .current
                        .iter()
                        .min_by_key(|(c, _, _)| *c)
                        .map(|(_, s, _)| s.as_str())
                        .unwrap_or("<bind>");
                    format!("{}={}", i.name, seq)
                })
                .collect();
            format!(
                "pawns rendered {} ({} meshes) | {}",
                p.instances.len(),
                p.models,
                shown.join(", ")
            )
        }
        Some(p) => format!("pawns rendered 0 ({} meshes)", p.models),
        None => "pawns unavailable".to_owned(),
    };
    let hud_line = match hud.as_deref() {
        Some(h) => {
            let name = match &*session {
                Ok(s) => h.hud_name(s.vm()),
                Err(_) => None,
            }
            .unwrap_or_else(|| "-".to_owned());
            format!(
                "HUD {} ({}) | PostRender frames {} commands {} glyphs {} | missing tile materials {} | {}",
                name,
                if h.hud.is_some() { "found" } else { "absent" },
                h.frames,
                h.total_commands,
                h.glyphs_drawn,
                h.missing_materials.len(),
                h.error.as_deref().unwrap_or("ok")
            )
        }
        None => "HUD unavailable".to_owned(),
    };
    let projectors_line = {
        let (runtime, grounded) = projector_decals
            .as_deref()
            .map_or((0, 0), |d| (d.active.len(), d.grounded));
        let counts = fog_ctx.as_deref().map_or_else(
            || "fog unavailable".to_owned(),
            |c| {
                format!(
                    "fog zones {} (fogged {})",
                    c.fog.params.len(),
                    c.fog.params.iter().filter(|p| p.is_fogged()).count()
                )
            },
        );
        format!("{counts} | runtime projector decals {runtime} (grounded {grounded})")
    };
    let combat_line = match &*session {
        Ok(s) => {
            let health = s
                .player_health()
                .map(|h| format!("{h:.0}"))
                .unwrap_or_else(|| "-".to_owned());
            let weapon = s
                .player_weapon()
                .map(|w| s.vm().objects[w as usize].name.clone())
                .unwrap_or_else(|| "none".to_owned());
            let view = weapon_view
                .as_deref()
                .map(weapons::overlay_line)
                .unwrap_or_else(|| "weapon view unavailable".to_owned());
            format!("player health {health} | weapon {weapon} | {view}")
        }
        Err(_) => "combat unavailable".to_owned(),
    };
    let lights_line = match runtime_lights.as_deref() {
        Some(l) => format!(
            "dynamic lights {} ({} spawned, {} MuzzleLight, {} Attach actors)",
            l.active, l.spawned, l.muzzle_actors, l.attach_actors
        ),
        None => "dynamic lights unavailable".to_owned(),
    };
    text.0 = format!(
        "XIII play prototype (NOT a playable mission; no weapons, no full AI)\n\
         map {} | pos ({:.1}, {:.1}, {:.1}) UU | vel ({:.1}, {:.1}, {:.1}) UU/s | state {}\n\
         floor normal ({:.2}, {:.2}, {:.2}) | last contact: {}\n\
         {combat_line}\n\
         {}\n\
         {pawns_line}\n\
         {lights_line}\n\
         {hud_line}\n\
         {projectors_line}\n\
         WASD move | mouse look | Space jump | Shift walk | C crouch | Left mouse fire | E use | Esc quit",
        cfg.options.map.as_deref().unwrap_or("?"),
        s.location[0],
        s.location[1],
        s.location[2],
        s.velocity[0],
        s.velocity[1],
        s.velocity[2],
        s.state(),
        s.floor_normal[0],
        s.floor_normal[1],
        s.floor_normal[2],
        s.last_source.as_deref().unwrap_or("-"),
        vm,
    );
    perf.span("overlay", t0);
}

#[allow(clippy::too_many_arguments)]
fn unattended(
    mut commands: Commands,
    cfg: Res<PlayConfig>,
    mut state: ResMut<TraceState>,
    flag: Res<ShotFlag>,
    sim: Res<SimRes>,
    host: NonSend<cutscene::CutsceneHost>,
    mut perf: ResMut<crate::perf::Perf>,
    mut exit: MessageWriter<AppExit>,
) {
    // item21 hook: an unattended run with a screenshot requested screenshots the in-game
    // cutscene video [`cutscene::VIDEO_SHOT_AFTER_SECS`] into playback (the video may start
    // long after the wall-clock budget began) and exits once the shot is confirmed.
    if cfg.options.screenshot.is_some()
        && state.video_exit_at.is_none()
        && cutscene::playing_for(&host, cutscene::VIDEO_SHOT_AFTER_SECS)
    {
        if let Some(path) = cfg.options.screenshot.clone() {
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(dir);
            }
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path))
                .observe(|_: On<ScreenshotCaptured>, mut f: ResMut<ShotFlag>| f.0 = true);
        }
        state.video_exit_at = Some(Instant::now());
        println!(
            "[play] cutscene video playing ({:.1}s into playback); screenshot requested",
            host.0.as_ref().map_or(0.0, |h| h.time_estimate())
        );
    }
    if let Some(at) = state.video_exit_at {
        // Wait for the screenshot to land (8 s backstop), then exit.
        if flag.0 || at.elapsed() >= Duration::from_secs(8) {
            println!(
                "[play] exit {:.1}s into cutscene playback, {} frames, screenshot {}",
                host.0.as_ref().map_or(0.0, |h| h.time_estimate()),
                state.tick,
                match (&cfg.options.screenshot, flag.0) {
                    (Some(p), true) => format!("saved {}", p.display()),
                    (Some(p), false) => format!("NOT confirmed {}", p.display()),
                    (None, _) => "none".into(),
                }
            );
            perf.request_final();
            exit.write(AppExit::Success);
        }
        return;
    }
    // After a level transition the new map owns a short tail: screenshot it once the scene has
    // settled, then exit, regardless of the original exit budget.
    if let Some(at) = state.post_travel_at {
        let since = at.elapsed().as_secs_f32();
        if flag.0 {
            state.shot_done = true;
        }
        if let Some(path) = &cfg.options.screenshot
            && state.shot == 0
            && since >= 2.0
        {
            let path: std::path::PathBuf = path.clone();
            if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
                let _ = std::fs::create_dir_all(dir);
            }
            commands
                .spawn(Screenshot::primary_window())
                .observe(save_to_disk(path))
                .observe(|_: On<ScreenshotCaptured>, mut f: ResMut<ShotFlag>| f.0 = true);
            state.shot = 1;
        }
        if since >= 3.0 {
            println!("[play] {}", format_trace(state.tick, since, &sim.0));
            println!(
                "[play] exit {:.1}s after travel, {} frames, screenshot {}",
                since,
                state.tick,
                match (&cfg.options.screenshot, state.shot_done) {
                    (Some(p), true) => format!("saved {}", p.display()),
                    (Some(p), false) => format!("NOT confirmed {}", p.display()),
                    (None, _) => "none".into(),
                }
            );
            perf.request_final();
            exit.write(AppExit::Success);
        }
        return;
    }
    let Some(secs) = state.exit_secs else {
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
        let path: std::path::PathBuf = path.clone();
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
    println!("[play] {}", format_trace(state.tick, elapsed, &sim.0));
    println!(
        "[play] exit after {:.1}s, {} frames, screenshot {}",
        elapsed,
        state.tick,
        match (&cfg.options.screenshot, state.shot_done) {
            (Some(p), true) => format!("saved {}", p.display()),
            (Some(p), false) => format!("NOT confirmed {}", p.display()),
            (None, _) => "none".into(),
        }
    );
    perf.request_final();
    exit.write(AppExit::Success);
}

/// Filter for the entities the travel reload despawns: rendered/UI content only, excluding Bevy
/// resource entities (stored as entities) and the window.
type MapSceneFilter = (
    Without<Window>,
    Or<(
        With<Transform>,
        With<Node>,
        With<Text>,
        With<Text2d>,
        With<Camera3d>,
    )>,
);

/// Level-transition host system (item15). When the game's own code requests travel, this tears
/// down the current map (every entity except the window), opens a fresh script session for the
/// requested map and rebuilds the scene through [`setup_inner`]. The input-script cursor and the
/// travel timing survive the reload.
#[allow(clippy::too_many_arguments)]
fn travel(
    mut commands: Commands,
    mut cfg: ResMut<PlayConfig>,
    mut session: NonSendMut<Result<session::Session, String>>,
    mut cutscene_host: NonSendMut<cutscene::CutsceneHost>,
    mut sync: ResMut<RenderSync>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut receiver_materials: ResMut<Assets<viewer::lights::ReceiverMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut bindposes: ResMut<Assets<SkinnedMeshInverseBindposes>>,
    mut decal_materials: ResMut<Assets<ForwardDecalMaterial<StandardMaterial>>>,
    // Every rendered/UI entity of the current map. The `Or` filter is what distinguishes our
    // content from Bevy resource entities (which are stored as entities and must not be
    // despawned) and from the window entity.
    entities: Query<Entity, MapSceneFilter>,
    mut exit: MessageWriter<AppExit>,
) {
    let Ok(sess) = session.as_mut() else {
        return;
    };
    let Some(req) = sess.take_travel_request() else {
        return;
    };
    let plan = match travel::TravelPlan::from_request(&req) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[play] travel request invalid: {e}");
            exit.write(AppExit::error());
            return;
        }
    };
    println!(
        "[play] travel requested at t={:.3}s by {}: url={:?} mode={} items={} source={:?} -> map {} options {:?}",
        req.time, req.actor, req.url, req.mode, req.items, req.source, plan.map, plan.options
    );
    let Some(game_dir) = cfg.options.game_dir.clone() else {
        eprintln!("[play] travel without --game-dir");
        exit.write(AppExit::error());
        return;
    };
    let t0 = Instant::now();
    // Tear down the current scene: every entity except the window(s). Cameras, lights, meshes,
    // skinned pawns, HUD nodes and particle emitters are all respawned by `setup_inner`.
    let mut removed = 0usize;
    for e in &entities {
        commands.entity(e).try_despawn();
        removed += 1;
    }
    let new_session = match session::Session::open(&game_dir, &plan.map) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[play] travel load failed for {}: {e}", plan.map);
            exit.write(AppExit::error());
            return;
        }
    };
    *session = Ok(new_session);
    cfg.options.map = Some(plan.map.clone());
    sync.entities.clear();
    let s = match session.as_mut() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("[play] travel session unavailable: {e}");
            exit.write(AppExit::error());
            return;
        }
    };
    // item21: reinstall the cutscene host on the new map's VM. Dropping the old session stops
    // its clip; the overlay sync tears the picture/audio down once the new host has none open.
    cutscene_host.0 = Some(cutscene::install(
        s.vm_mut(),
        Path::new(&game_dir),
        cfg.options.audio == crate::cli::Audio::On,
    ));
    match setup_inner(
        &mut commands,
        &cfg.options,
        s,
        &mut sync,
        &mut meshes,
        &mut materials,
        &mut receiver_materials,
        &mut images,
        &mut bindposes,
        &mut decal_materials,
        true,
    ) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("[play] travel setup failed for {}: {e}", plan.map);
            exit.write(AppExit::error());
            return;
        }
    }
    println!(
        "[play] travel complete: -> {} ({} entities replaced) in {:.2}s",
        plan.map,
        removed,
        t0.elapsed().as_secs_f32()
    );
}

/// One trace line: `t` seconds, tick, Unreal position/velocity, state, floor normal, source.
fn format_trace(tick: u64, t: f32, sim: &PlayerSim) -> String {
    format!(
        "t={t:6.3}s tick={tick:5} pos=({:8.1},{:8.1},{:8.1}) UU vel=({:7.1},{:7.1},{:7.1}) UU/s {} floor_n=({:.2},{:.2},{:.2}) source={}",
        sim.location[0],
        sim.location[1],
        sim.location[2],
        sim.velocity[0],
        sim.velocity[1],
        sim.velocity[2],
        sim.state(),
        sim.floor_normal[0],
        sim.floor_normal[1],
        sim.floor_normal[2],
        sim.last_source.as_deref().unwrap_or("-")
    )
}

/// Headless deterministic scripted run (`--play-script` with no `--screenshot`): imports the
/// map, spawns the player, replays the script through the same [`PlayerSim`], prints a trace
/// every [`TRACE_EVERY`] ticks and exits. No window is opened.
pub fn run_headless(opts: &Options) -> AppExit {
    match run_headless_inner(opts) {
        Ok(()) => AppExit::Success,
        Err(e) => {
            eprintln!("error: {e}");
            AppExit::error()
        }
    }
}

/// Outcome of a headless scripted run: the VM session (after the run), the movement trace and
/// timing. Shared by `--play-script` and the opt-in walking-trigger corpus test.
pub(crate) struct ScriptOutcome {
    /// VM session after the run (the last map when the run travelled).
    pub session: session::Session,
    /// Fixed ticks run.
    pub ticks: u64,
    /// Wall-clock seconds spent in the loop.
    pub wall_secs: f32,
    /// Trace samples: `(tick, seconds, position UU, velocity UU/s)`.
    pub trace: Vec<(u64, f32, [f32; 3], [f32; 3])>,
    /// Map stem active at the end of the run.
    pub final_map: String,
    /// One entry per level transition, in order (empty when the run did not travel).
    pub travel: Vec<TravelHop>,
    /// item18: each map the run passed through and its `MapInfo.Objectif[]` states as the run left
    /// it (the final map's states are the last entry). Requirement 4's "objective states over
    /// time" across a multi-map run.
    pub map_objectives: Vec<(String, Vec<session::ObjectiveState>)>,
    /// Host-synthesised footsteps in order:
    /// `(seconds, XIIIFootStepSound wrapper path, floor material path)`.
    pub footsteps: Vec<(f32, String, Option<String>)>,
}

/// One level transition observed in a headless run.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct TravelHop {
    /// Map stem travelled from.
    pub from: String,
    /// Map stem travelled to.
    pub to: String,
    /// Requested URL exactly as the script built it.
    pub url: String,
    /// UE2 `ETravelType` byte.
    pub mode: u8,
    /// `bItems`.
    pub items: bool,
    /// VM time (seconds) of the source map when the request was taken.
    pub vm_time: f64,
    /// Global tick at which the request was taken.
    pub tick: u64,
}

/// Per-map headless runtime: session, collision world, movement volumes and the player sim.
struct MapRuntime {
    name: String,
    session: session::Session,
    world: CollisionWorld,
    mover_collision: movers::MoverCollision,
    volumes: movement_modes::VolumeMotion,
    sim: PlayerSim,
    sources: Vec<String>,
    /// Shared counter of voice names this map's `VoiceDuration` provider could not resolve (the
    /// provider is re-installed on every map open, including travel reloads).
    voice_unresolved: std::sync::Arc<std::sync::atomic::AtomicU64>,
    /// item21: the host `VideoPlayer` provider installed on this map's VM. Driven by
    /// `advance_virtual_all` each fixed step (no audio device headless: completion is the
    /// decoded frame count).
    video_host: Option<crate::video::VideoHostHandle>,
}

/// Opens a script session and builds the movement world for `map` from `scene` (the headless
/// path). Prints the same mover/spawn/volume lines as the interactive setup.
fn open_map_runtime(
    game_dir: &Path,
    map: &str,
    scene: &xiii_world::WorldScene,
    params: &PlayerParams,
) -> Result<MapRuntime, String> {
    let mut session = session::Session::open(game_dir, map)?;
    // item21: install the host `VideoPlayer` provider. Headless runs have no output device, so
    // audio is off and playback is caller-paced (`advance_virtual_all`): `GetStatus` still
    // reports completion from the decoded frame count, which is what the level-end
    // `PlayingVideo` state waits for.
    let video_host = {
        let host = crate::video::VideoHostHandle::new(game_dir.to_path_buf(), false);
        session.vm_mut().set_video_host(Box::new(host.clone()));
        Some(host)
    };
    // The headless path has no Bevy audio resource; scan the same decoded HX library so
    // `Actor.PlayStrVoice` takes the engine's voice-completion path (real wave length) instead of
    // the script's `NoSound` fallback. Names the library cannot resolve keep returning `false`.
    let voice_library = std::sync::Arc::new(std::sync::Mutex::new(xiii_audio::SoundLibrary::scan(
        game_dir,
    )));
    let (voice_provider, voice_unresolved) = voice::LibraryVoiceDuration::new(voice_library);
    session
        .vm_mut()
        .set_voice_duration(Box::new(voice_provider));
    session.register_movers(scene);
    let mover_states = session.mover_states();
    let (mut world, mover_collision) = movers::MoverCollision::build(scene, &mover_states);
    mover_collision.update(&mut world, &mover_states);
    println!(
        "[play] movers: {} collision objects from {} live mover actors ({} static triangles): {}",
        mover_collision.count(),
        mover_states.len(),
        world.triangle_count(),
        mover_collision.names().join(", ")
    );
    let sources = scene.collision_sources.clone();
    let (ps_bevy, rot) = scene.player_start.ok_or("map has no PlayerStart")?;
    let spawn = collision::place_spawn(&world, ps_bevy, params.half_extents_bevy())?;
    println!(
        "[play] spawn: PlayerStart {:?} UU -> box centre {:?} UU, raise {:.2} UU",
        bevy_to_unreal_position(ps_bevy),
        bevy_to_unreal_position(spawn.position),
        spawn.raise * UNREAL_UNITS_PER_METER
    );
    // The script login chain (item3h) created the pawn at the PlayerStart; the host owns its
    // movement fields (item8a rule), so the position comes from the host's FindSpot placement
    // and the facing from the pawn's script-set Rotation when the login path ran.
    let start_yaw = match (session.login_script, session.player_rotation()) {
        (1, Some(r)) => r[1] as f32 * std::f32::consts::TAU / 65536.0,
        _ => rot[1] as f32 * std::f32::consts::TAU / 65536.0,
    };
    let mut sim = PlayerSim::new(bevy_to_unreal_position(spawn.position), start_yaw);
    sim.grounded = true;
    let volumes = match xiii_world::movement_volumes::MovementVolumes::import(game_dir, map) {
        Ok(v) => {
            println!("[play] movement volumes: {}", v.summary());
            for d in &v.diagnostics {
                println!("[play]   volume diagnostic: {d}");
            }
            movement_modes::VolumeMotion::new(v)
        }
        Err(e) => {
            println!("[play] movement volumes unavailable ({e}); ladder/water modes disabled");
            movement_modes::VolumeMotion::default()
        }
    };
    Ok(MapRuntime {
        name: map.to_owned(),
        session,
        world,
        mover_collision,
        volumes,
        sim,
        sources,
        voice_unresolved,
        video_host,
    })
}

/// Opens a VM session and drives it with the movement simulation and an input script. No window
/// is opened; the fixed-step order is the one `fixed_step` uses.
///
/// The collision world (static soup + the VM's mover actors as dynamic objects) is built here so
/// the script can never diverge from the interactive path. When the game's own code requests
/// level travel, the next map is imported and a fresh session is opened (the host owns the
/// transition); the run continues on the new map until the duration is spent.
pub(crate) fn run_script(
    game_dir: &Path,
    map: &str,
    script: &script::Script,
    params: &PlayerParams,
    scene: &xiii_world::WorldScene,
    duration: f32,
) -> Result<ScriptOutcome, String> {
    let started = Instant::now();
    let ticks = (duration / DT).ceil() as u64;
    let mut drive = script::Drive::new(script);
    let mut trace = Vec::new();
    let mut travel = Vec::new();
    let mut map_objectives = Vec::new();
    let mut runtime = open_map_runtime(game_dir, map, scene, params)?;
    // Accumulated unresolved voice names across maps (the provider is re-installed per map).
    let mut voice_unresolved_total = 0u64;
    // Player footsteps (item6e): the same notify-free synthesis `fixed_step` uses, so the
    // headless path reports and can play them. Rebuilt for each map after a level transition.
    let mut surfaces = footsteps::SurfaceSounds::from_scene(scene);
    let mut step_driver = footsteps::FootstepDriver::new();
    let mut footstep_log: Vec<(f32, String, Option<String>)> = Vec::new();
    let mut tick = 0u64;
    while tick < ticks {
        let elapsed = tick as f32 * DT;
        if let Some(name) = drive.tracking_actor().map(str::to_owned) {
            let location = runtime
                .session
                .vm()
                .find_live_object(&name)
                .and_then(|id| runtime.session.vm().vector_prop(id, "Location"));
            drive.set_track_location(Some(&name), location);
        } else {
            drive.set_track_location(None, None);
        }
        let input = drive.advance(elapsed, &mut runtime.sim);
        let weapons = drive.take_weapons();
        let goals = drive.take_goals();
        let equip = drive.take_equip();
        let use_named = drive.take_use_named();
        let search = drive.take_search();
        let control = drive.take_control();
        let fired = input.fire;
        if runtime.volumes.is_empty() {
            runtime
                .sim
                .step(DT, &runtime.world, params, input, &runtime.sources);
        } else {
            runtime.sim.step_with_modes(
                DT,
                &runtime.world,
                params,
                input,
                &runtime.sources,
                &runtime.volumes,
            );
        }
        // The headless driver only records footsteps; playback belongs to the windowed
        // `fixed_step` path (this function opens no audio device).
        if let Some(step) = step_driver.advance(
            DT,
            &runtime.sim,
            params,
            &runtime.world,
            &surfaces,
            input.walk,
        ) {
            footstep_log.push((elapsed, step.sound, step.material));
        }
        let modes = session::PlayerVMModes {
            crouched: runtime.sim.crouched,
            in_water: runtime.sim.in_water,
            physics: runtime.sim.physics,
            landed_velocity_z: runtime.sim.landed.then_some(runtime.sim.land_velocity_z),
            floor_normal: runtime.sim.floor_normal,
        };
        runtime.session.drive_render_phase();
        runtime.session.step(
            DT,
            runtime.sim.location,
            runtime.sim.yaw,
            runtime.sim.velocity,
            &modes,
        );
        if let Some((location, yaw, velocity)) = runtime.session.script_pawn_pose() {
            runtime.sim.location = location;
            runtime.sim.yaw = yaw;
            runtime.sim.velocity = velocity;
        }
        // item21: advance the host cutscene player with the VM's own time. `PlayingVideo`'s
        // `GetStatus` (next tick) then ends when the decoded frame count elapsed.
        if let Some(h) = runtime.video_host.as_ref() {
            h.advance_virtual_all(f64::from(DT));
        }
        let states = runtime.session.mover_states();
        runtime.mover_collision.update(&mut runtime.world, &states);
        for path in &weapons {
            match runtime.session.grant_weapon(path) {
                Ok(msg) => println!("[play] weapon {msg}"),
                Err(e) => println!("[play] weapon grant failed {path}: {e}"),
            }
        }
        for n in &goals {
            if let Err(e) = runtime.session.set_goal(*n) {
                println!("[play] set_goal {n} failed: {e}");
            }
        }
        if equip {
            match runtime.session.equip_inventory_weapon() {
                Ok(msg) => println!("[play] equip {msg}"),
                Err(e) => println!("[play] equip failed: {e}"),
            }
        }
        if control {
            match runtime.session.take_control() {
                Ok(state) => println!("[play] take_control diagnostic: controller -> {state}"),
                Err(e) => println!("[play] take_control failed: {e}"),
            }
        }
        if input.use_action {
            perform_use(
                &mut runtime.session,
                &runtime.world,
                &runtime.sources,
                &runtime.sim,
                params,
            );
        }
        for target in &use_named {
            let outcome = runtime.session.use_target(target);
            println!("[play] use {target}: {outcome:?}");
        }
        for target in &search {
            let outcome = runtime.session.search_corpse(target);
            println!("[play] search {target}: {outcome:?}");
        }
        if fired {
            match runtime.session.fire(runtime.sim.yaw, runtime.sim.pitch) {
                session::FireOutcome::Fired => {
                    println!(
                        "[play] fire [{elapsed:.3}s] player {} bone {} | {}",
                        runtime
                            .session
                            .player_health()
                            .map(|h| format!("{h:.0} hp"))
                            .unwrap_or_else(|| "? hp".to_owned()),
                        runtime.session.vm().last_trace_bone(),
                        combat_snapshot(&runtime.session)
                    );
                }
                other => println!("[play] fire [{elapsed:.3}s]: {other:?}"),
            }
        }
        if tick.is_multiple_of(TRACE_EVERY) || tick + 1 == ticks {
            trace.push((tick, elapsed, runtime.sim.location, runtime.sim.velocity));
            println!(
                "[play] {} | {} | {}",
                format_trace(tick, elapsed, &runtime.sim),
                format_vm_trace(&runtime.session),
                format_mover_trace(&runtime.session)
            );
        }

        // Level transition: the game's own goal/travel code requested it. The VM reported the
        // URL; the host imports the next map and opens a fresh session.
        if let Some(req) = runtime.session.take_travel_request() {
            let plan = travel::TravelPlan::from_request(&req)?;
            println!(
                "[play] travel requested at t={:.3}s tick={} by {}: url={:?} mode={} items={} source={:?} -> map {} options {:?}",
                req.time,
                tick,
                req.actor,
                req.url,
                req.mode,
                req.items,
                req.source,
                plan.map,
                plan.options
            );
            travel.push(TravelHop {
                from: runtime.name.clone(),
                to: plan.map.clone(),
                url: req.url.clone(),
                mode: req.mode,
                items: req.items,
                vm_time: req.time,
                tick,
            });
            map_objectives.push((runtime.name.clone(), runtime.session.objective_states()));
            // The script block (if any) is released; the next map starts a fresh run.
            drive.notify_travel();
            let opts = crate::cli::Options {
                game_dir: Some(game_dir.to_path_buf()),
                map: Some(plan.map.clone()),
                ..Default::default()
            };
            let next_scene = viewer::load_scene(&opts)?;
            let t0 = Instant::now();
            voice_unresolved_total += runtime
                .voice_unresolved
                .load(std::sync::atomic::Ordering::Relaxed);
            runtime = open_map_runtime(game_dir, &plan.map, &next_scene, params)?;
            // Rebuild the footstep surface map and driver for the next map.
            surfaces = footsteps::SurfaceSounds::from_scene(&next_scene);
            step_driver = footsteps::FootstepDriver::new();
            println!(
                "[play] travel complete: {} -> {} in {:.2}s ({} objects)",
                travel.last().map(|h| h.from.as_str()).unwrap_or("-"),
                plan.map,
                t0.elapsed().as_secs_f32(),
                next_scene.objects.len()
            );
        }
        tick += 1;
    }
    if drive.waiting_travel() && travel.is_empty() {
        println!(
            "[play] script is still waiting for travel after {:.1}s; no travel was requested",
            duration
        );
    }
    map_objectives.push((runtime.name.clone(), runtime.session.objective_states()));
    if let Some(h) = runtime.video_host.as_ref() {
        println!("[play] {}", h.report_line());
        for d in h.diagnostics() {
            println!("[play]   video host: {d}");
        }
    }
    let unresolved = voice_unresolved_total
        + runtime
            .voice_unresolved
            .load(std::sync::atomic::Ordering::Relaxed);
    if unresolved > 0 {
        println!(
            "[play] voice durations: {unresolved} name(s) unresolved (those lines use the script's NoSound fallback)"
        );
    } else {
        println!("[play] voice durations: every requested name resolved from the HX library");
    }
    Ok(ScriptOutcome {
        session: runtime.session,
        ticks,
        wall_secs: started.elapsed().as_secs_f32(),
        trace,
        final_map: runtime.name,
        travel,
        map_objectives,
        footsteps: footstep_log,
    })
}

/// One compact line per live soldier with its `Health`/`bIsDead`, plus the player.
fn combat_snapshot(sess: &session::Session) -> String {
    let vm = sess.vm();
    let mut parts = Vec::new();
    for (i, o) in vm.objects.iter().enumerate() {
        if !o.is_actor || o.deleted || o.name.starts_with("Default__") {
            continue;
        }
        let id = i as xiii_script::ObjectId;
        if !vm.is_a(id, "basesoldier") {
            continue;
        }
        let hp = sess
            .actor_health(id)
            .map(|h| format!("{h:.0}"))
            .unwrap_or_else(|| "?".to_owned());
        let bone = match vm.get_property(id, "LastBoneHit") {
            Some(xiii_script::Value::Name(n)) if !n.eq_ignore_ascii_case("None") => {
                format!(" bone={n}")
            }
            _ => String::new(),
        };
        parts.push(format!(
            "{} {hp} hp{}{bone}",
            o.name,
            if sess.actor_is_dead(id) { " DEAD" } else { "" }
        ));
    }
    if parts.is_empty() {
        "no soldiers".to_owned()
    } else {
        parts.join(", ")
    }
}

/// One compact entry per mover that is currently interpolating:
/// `name key=K alpha=A loc=(x,y,z) rot=(p,y,r)`. Movers at rest are omitted (there are ~90 on
/// Plage01), but the count is always reported so a missing mover cannot hide.
fn format_mover_trace(sess: &session::Session) -> String {
    let movers = sess.mover_states();
    let moving: Vec<_> = movers.iter().filter(|m| m.interpolating).collect();
    let parts = moving
        .iter()
        .map(|m| {
            format!(
                "{} key={} alpha={:.3} loc=({:.1},{:.1},{:.1}) rot=({},{},{})",
                m.name,
                m.key_num,
                m.phys_alpha,
                m.location[0],
                m.location[1],
                m.location[2],
                m.rotation[0],
                m.rotation[1],
                m.rotation[2]
            )
        })
        .collect::<Vec<_>>()
        .join(" | ");
    if parts.is_empty() {
        format!("movers 0/{} moving (all at rest)", movers.len())
    } else {
        format!("movers {}/{} moving: {parts}", moving.len(), movers.len())
    }
}

fn run_headless_inner(opts: &Options) -> Result<(), String> {
    let game_dir = opts
        .game_dir
        .clone()
        .ok_or("--game-dir is required for --play-script")?;
    let map = opts.map.clone().ok_or("--play-script needs --map")?;
    let script_path = opts
        .play_script
        .clone()
        .ok_or("--play-script needs a script file")?;
    let script = script::Script::load(&script_path)?;
    let scene = viewer::load_scene(opts)?;
    let resolved = resolve_params(&game_dir)?;
    let params = resolved.params;
    println!(
        "[play] headless scripted VM run (no window): map {map}, {} events from {}",
        script.events.len(),
        script_path.display()
    );
    for l in &resolved.lines {
        println!("[play] {l}");
    }
    let duration = opts.exit_after_secs.unwrap_or_else(|| {
        if script.has_wait_travel() {
            // `wait_travel` scripts run until the game requests travel; give them a budget.
            script.last_time() + 60.0
        } else {
            script.last_time() + 2.0
        }
    });
    let outcome = run_script(&game_dir, &map, &script, &params, &scene, duration)?;
    let session = &outcome.session;
    println!(
        "[play] travel: {} transition(s), final map {}",
        outcome.travel.len(),
        outcome.final_map
    );
    for hop in &outcome.travel {
        println!(
            "[play]   travel {} -> {} url={:?} mode={} items={} at t={:.3}s tick={}",
            hop.from, hop.to, hop.url, hop.mode, hop.items, hop.vm_time, hop.tick
        );
    }
    for (map, states) in &outcome.map_objectives {
        println!(
            "[play]   objectives as the run left {map}: {}",
            states
                .iter()
                .map(|o| format!(
                    "[{}{}{}] {}",
                    o.index,
                    if o.primary { " P" } else { " -" },
                    if o.completed {
                        " C"
                    } else if o.anti_goal {
                        " A"
                    } else {
                        " ."
                    },
                    if o.text.is_empty() {
                        "<empty>"
                    } else {
                        o.text.as_str()
                    }
                ))
                .collect::<Vec<_>>()
                .join(" | ")
        );
    }
    println!("[play] {}", session.bootstrap_note);
    match pawns::headless_report(session, &game_dir) {
        Ok(line) => println!("[play] {line}"),
        Err(e) => println!("[play] pawns headless report failed: {e}"),
    }
    println!("[play] player login path: script={}", session.login_script);
    println!("[play] hit boxes: {}", session.hitbox_summary());
    for e in &session.hitbox_errors {
        println!("[play]   hit-box mesh failed: {e}");
    }
    println!(
        "[play] localisation: language={} localized class-default overrides={}",
        session.localization_language, session.localized_overrides
    );
    println!(
        "[play] video clips: {} Bink header(s) read, {} timed (VideoPlayer.GetStatus uses the real duration)",
        session.video_clips, session.video_timed
    );
    println!("[play] objectives: {}", session.objective_summary());
    let pawns_now = session.player_pawn_actors();
    println!(
        "[play] player pawns: {} live XIIIPlayerPawn actor(s): {}",
        pawns_now.len(),
        pawns_now
            .iter()
            .map(|(_, n)| n.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "[play] player {} | controller {} | GameInfo {}",
        session.player_name,
        session.controller.is_some(),
        session.game_info.is_some()
    );
    println!(
        "[play] combat: player {} hp{} | weapon {} | {}",
        session
            .player_health()
            .map(|h| format!("{h:.0}"))
            .unwrap_or_else(|| "?".to_owned()),
        if session.actor_is_dead(session.player) {
            " DEAD"
        } else {
            ""
        },
        session
            .player_weapon()
            .map(|w| session.vm().objects[w as usize].name.clone())
            .unwrap_or_else(|| "none".to_owned()),
        combat_snapshot(session)
    );
    println!(
        "[play] player inventory: {}",
        session
            .inventory_items()
            .iter()
            .map(|(n, c)| format!("{n} [{c}]"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    for b in &session.blocked {
        println!("[play]   script path blocked: {b}");
    }
    println!(
        "[play] VM final: t={:.3}s active={} live={} suspended={} dispatcher={} (active {})",
        session.vm_time(),
        session.active_actors(),
        session.live_actors(),
        session.suspended.len(),
        session.dispatcher_state().unwrap_or_else(|| "-".to_owned()),
        session.dispatcher_active()
    );
    for (name, state) in session.soldier_states() {
        println!(
            "[play]   soldier {name}: {}",
            state.as_deref().unwrap_or("<none>")
        );
    }
    for (name, state, active) in session.controller_states() {
        println!(
            "[play]   controller {name}: {} [{}]",
            state.as_deref().unwrap_or("<none>"),
            if active { "active" } else { "suspended" }
        );
    }
    for (t, actor) in session.touches() {
        println!("[play] touch [{t:.3}s] player <- {actor}");
    }
    println!(
        "[play] suspended actors ({}): {}",
        session.suspended.len(),
        session.suspended.join(", ")
    );
    for e in session.recent_events(5) {
        println!("[play] event {e}");
    }
    if let Some(first) = session.first_error() {
        println!("[play] first script error: {first}");
    }
    // Footstep summary and the full ordered list (requirement 3: count and names on Plage01).
    let mut by_sound: std::collections::BTreeMap<String, (usize, Option<String>)> =
        std::collections::BTreeMap::new();
    for (_, s, mat) in &outcome.footsteps {
        let e = by_sound.entry(s.clone()).or_insert((0, mat.clone()));
        e.0 += 1;
        if e.1.is_none() {
            e.1.clone_from(mat);
        }
    }
    println!(
        "[play] footsteps: {} emitted: {}",
        outcome.footsteps.len(),
        by_sound
            .iter()
            .map(|(s, (n, mat))| match mat {
                Some(m) => format!("{n}x {s} [{m}]"),
                None => format!("{n}x {s} [material path unknown]"),
            })
            .collect::<Vec<_>>()
            .join(", ")
    );
    for (t, s, mat) in &outcome.footsteps {
        match mat {
            Some(m) => println!("[play]   footstep [{t:.3}s] {s} [{m}]"),
            None => println!("[play]   footstep [{t:.3}s] {s}"),
        }
    }
    println!(
        "[play] headless scripted VM run finished in {:.2}s wall time, {} ticks, {} trace samples",
        outcome.wall_secs,
        outcome.ticks,
        outcome.trace.len()
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `XIII_GOG_DIR` resolved against the workspace root, or `None` in CI.
    fn opt_in_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in corpus test: Plage00, 2 s of forward input from the PlayerStart must stay on the
    /// floor (no falling through) and move horizontally.
    #[test]
    fn opt_in_plage00_forward_stays_on_floor() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage00".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage00");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let world = CollisionWorld::new(scene.box_collision());
        let (ps, rot) = scene.player_start.expect("Plage00 has a PlayerStart");
        let spawn = collision::place_spawn(&world, ps, resolved.params.half_extents_bevy())
            .expect("spawn placement");
        let mut sim = PlayerSim::new(
            bevy_to_unreal_position(spawn.position),
            rot[1] as f32 * std::f32::consts::TAU / 65536.0,
        );
        sim.grounded = true;
        let start = sim.location;
        for _ in 0..120 {
            sim.step(
                DT,
                &world,
                &resolved.params,
                Input {
                    forward: 1.0,
                    ..Default::default()
                },
                &scene.collision_sources,
            );
        }
        let dist =
            ((sim.location[0] - start[0]).powi(2) + (sim.location[1] - start[1]).powi(2)).sqrt();
        println!(
            "[play test] Plage00 2 s forward: start {:?} -> {:?} UU, {} UU horizontal, grounded {}",
            start, sim.location, dist, sim.grounded
        );
        assert!(sim.grounded, "player did not stay on the floor: {sim:?}");
        assert!(
            (sim.location[2] - start[2]).abs() < 200.0,
            "player fell through the floor: z {} -> {}",
            start[2],
            sim.location[2]
        );
        assert!(dist > 1.0, "player did not move: {dist} UU");
    }

    /// Synthetic ownership/sync test: the render translation follows a VM-moved actor's
    /// Location delta and does not move a static actor.
    #[test]
    fn render_sync_delta_follows_vm_location_change() {
        // An actor the VM moved by +90 UU in Unreal Y: the stored delta is in Unreal units and
        // converts to exactly one Bevy metre at the calibrated 90 UU/m.
        let moved = session::render_delta([100.0, 200.0, 300.0], [100.0, 290.0, 300.0]);
        assert_eq!(moved, [0.0, 90.0, 0.0]);
        let bevy = Vec3::from_array(to_bevy_position(moved));
        assert!((bevy.length() - 1.0).abs() < 1e-4, "delta {bevy:?} != 1 m");
        // A static actor's Locations are equal: no delta.
        let still = session::render_delta([1.0, 2.0, 3.0], [1.0, 2.0, 3.0]);
        assert_eq!(still, [0.0; 3]);
    }

    /// The input script can place the pawn (needed because the Plage00 trigger is ~47,000 UU
    /// from the PlayerStart) and then walk it.
    #[test]
    fn script_teleport_places_the_player() {
        let s = script::Script::parse("t=0.0 teleport 1 2 3\nt=0.0 place 4 5 6\n").unwrap();
        assert_eq!(s.events.len(), 2);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut d = script::Drive::new(&s);
        d.advance(0.0, &mut sim);
        assert_eq!(sim.location, [4.0, 5.0, 6.0]);
        // Bad arity is rejected.
        assert!(script::Script::parse("t=0.0 teleport 1 2\n").is_err());
    }

    /// Opt-in corpus test (item6e requirement 3): a scripted shuttle walk on Plage01 emits
    /// footsteps whose names come from the floor's `XIIIFootStepSound` material properties. The
    /// nine-second walk starts after the game's wake-up handoff returns control at 45 s. The hut
    /// interior texture carries `XIIIPlage.PLmeub05` -> `XIIIsound.Footsteps__XIIIFSBoi.…`.
    #[test]
    fn opt_in_plage01_scripted_walk_plays_surface_footsteps() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let script = script::Script::parse(
            "t=45.0 forward 1\nt=46.5 forward -1\nt=48.0 forward 1\nt=49.5 forward -1\nt=51.0 forward 1\nt=52.5 forward -1\nt=54.0 forward 0\n",
        )
        .unwrap();
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            55.0,
        )
        .expect("run Plage01 shuttle walk");
        assert!(
            !outcome.footsteps.is_empty(),
            "a 9 s scripted walk on Plage01 must emit footsteps"
        );
        // Every footstep names a real `XIIIFootStepSound` wrapper and a floor material path.
        for (t, sound, material) in &outcome.footsteps {
            assert!(
                sound.to_ascii_lowercase().contains("footsteps__xiiifs"),
                "footstep [{t:.3}s] sound {sound} is not a player footstep wrapper"
            );
            assert!(
                material.is_some(),
                "footstep [{t:.3}s] {sound} has no floor material path"
            );
        }
        let mut names: Vec<&str> = outcome
            .footsteps
            .iter()
            .map(|(_, s, _)| s.as_str())
            .collect();
        names.sort_unstable();
        names.dedup();
        println!(
            "[steps test] Plage01 {} footsteps, {} distinct: {:?}; materials {:?}",
            outcome.footsteps.len(),
            names.len(),
            names,
            outcome
                .footsteps
                .iter()
                .filter_map(|(_, _, m)| m.clone())
                .collect::<std::collections::BTreeSet<_>>()
        );
    }

    /// item32 opt-in regression: the authored Plage01 trigger wakes BaseSoldier6 from faction.
    /// `IAController.Init` reads `MapInfo.XIIIPawn` after two latent sleeps (about 0.2-0.3 s);
    /// the game's own `MapInfo.FirstFrame` (run by `MapInfo.Timer` after `checkTime` = 0.1 s)
    /// sets it first, so `Trigger` engages the real player.
    #[test]
    fn opt_in_plage01_faction_trigger_cue_enters_scripted_attack() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = session::Session::open(&game_dir, "Plage01").expect("open Plage01");
        let soldier = session
            .vm()
            .find_object("BaseSoldier6")
            .expect("BaseSoldier6");
        let controller = match session.vm().get_property(soldier, "Controller") {
            Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(id)))) => *id,
            _ => panic!("BaseSoldier6 has no controller"),
        };
        let player = session.player;
        let player_loc = session
            .vm()
            .vector_prop(player, "Location")
            .expect("player location");

        for _ in 0..120 {
            session.step(
                1.0 / 60.0,
                player_loc,
                0.0,
                [0.0; 3],
                &session::PlayerVMModes::default(),
            );
        }
        let map_info = session.map_info().expect("Plage01 MapInfo");
        assert_eq!(
            session.vm().get_property(map_info, "XIIIPawn"),
            Some(&xiii_script::Value::Object(Some(
                xiii_script::ObjRef::Instance(player)
            ))),
            "MapInfo.FirstFrame (MapInfo.Timer after checkTime 0.1 s) must have set XIIIPawn to the logged-in pawn"
        );
        assert_eq!(
            session.vm().state_name(controller).as_deref(),
            Some("faction"),
            "BaseSoldier6 should begin in the map-authored faction order"
        );

        // Deliver the real trigger event with the logged-in pawn. The map dispatcher and the
        // soldier's own Pawn.Trigger -> IAController.Trigger chain perform the state transition.
        let touch = session
            .vm()
            .find_object("TouchTrigger8")
            .expect("TouchTrigger8");
        session
            .vm_mut()
            .send_event(
                touch,
                "Touch",
                vec![xiii_script::Value::Object(Some(
                    xiii_script::ObjRef::Instance(player),
                ))],
            )
            .expect("TouchTrigger8.Touch");

        let mut engagement = None;
        for tick in 0..600u32 {
            session.step(
                1.0 / 60.0,
                player_loc,
                0.0,
                [0.0; 3],
                &session::PlayerVMModes::default(),
            );
            let state = session.vm().state_name(controller);
            if matches!(
                state.as_deref(),
                Some("Acquisition" | "Attaque" | "AttaqueScriptee")
            ) {
                engagement = Some((tick as f32 / 60.0, state));
                break;
            }
        }
        println!(
            "[item32] cue TouchTrigger8 -> XIIIDispatcher3.OutEvents('tueur_conducteur'); \
             BaseSoldier6 engagement {engagement:?}"
        );
        assert!(
            engagement.is_some(),
            "the authored tueur_conducteur cue did not move BaseSoldier6 from faction to \
             Acquisition/Attaque/AttaqueScriptee; current state {:?}, first error {:?}",
            session.vm().state_name(controller),
            session.first_error()
        );
    }

    /// Opt-in corpus test (requirement 6): the Plage01 script run opens the locked hut door
    /// (`Porte6`) with the host use action and ends with the player at least 2 m (180 UU) outside
    /// the door plane, along the door's own forward axis. No teleport.
    #[test]
    fn opt_in_plage01_door_opens_and_player_walks_out() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        // Item3i: the door key is no longer host-granted. The pawn walks onto the hut key
        // through the game's own pickup chain (autopilot `goto` + jumps; approached from the
        // key's open -Y side), then walks to `Porte6` and uses the carried key.
        let script = script::Script::parse(
            "t=45.00 teleport -491.8 -414.1 1265.0\n\
             t=45.10 goto -491.84 -314.14\nt=45.30 jump\nt=45.80 jump\nt=46.30 jump\nt=46.80 jump\n\
             t=47.30 jump\nt=47.80 forward 0\n\
             t=48.20 teleport -742.1444 -808.429 1311.0449\n\
             t=48.20 yaw 312.891\nt=48.20 turn 2\nt=48.20 forward 1\n\
             t=49.80 turn -45\nt=50.50 forward 0\nt=50.80 use\nt=51.80 use\nt=52.00 forward 1\n\
             t=53.00 forward 0\n",
        )
        .unwrap();
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            54.0,
        )
        .expect("run Plage01 door walk");
        let s = &outcome.session;
        assert!(
            s.inventory_items()
                .iter()
                .any(|(_, c)| c.eq_ignore_ascii_case("xidmaps.Plage01CahuteKey")),
            "the carried key must come from the pickup chain, not a host grant: {:?}",
            s.inventory_items()
        );
        let door = s
            .mover_states()
            .into_iter()
            .find(|m| m.name.eq_ignore_ascii_case("Porte6"))
            .expect("Plage01 has a live Porte6");
        assert_eq!(
            door.key_num, 1,
            "Porte6 did not reach its open key: {door:?}"
        );
        assert_ne!(
            door.rotation, door.base_rot,
            "Porte6 rotation did not change (door did not swing)"
        );
        let (_, _, pos, _) = outcome.trace.last().expect("trace sample");
        let pos = *pos;
        let base = door.base_pos;
        let yaw = door.base_rot[1] as f32 * std::f32::consts::TAU / 65536.0;
        let fwd = [yaw.cos(), yaw.sin()];
        let dist = (pos[0] - base[0]) * fwd[0] + (pos[1] - base[1]) * fwd[1];
        println!(
            "[play test] Plage01 door walk: Porte6 key={} rot={:?} (base {:?}), player {:?} UU, \
             {dist:.1} UU outside the door plane",
            door.key_num, door.rotation, door.base_rot, pos
        );
        assert!(
            dist >= 2.0 * UNREAL_UNITS_PER_METER,
            "player only {dist:.1} UU outside the door plane (need >= {:.0})",
            2.0 * UNREAL_UNITS_PER_METER
        );
    }

    /// item19 opt-in corpus test: follow Plage01 without `take_control`/`set_goal`. The intro and
    /// objective promotions must come from the map's Cine2/ScriptedImpacts/TouchTrigger chains;
    /// teleports in the route file are explicitly diagnostic movement shortcuts only. Pickup,
    /// doors, damage/death, corpse search, goal completion and travel use the game's own code.
    #[test]
    fn opt_in_plage01_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        // Tracked fixture (our own route commands; no game data).
        let route_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/plage01_route.script");
        let script = script::Script::load(&route_path).expect("load item19 Plage01 route");
        assert!(
            script
                .events
                .iter()
                .all(|event| !matches!(&event.command, script::Command::TakeControl)),
            "the normal Plage01 route must release control through the authored intro"
        );
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            140.0,
        )
        .expect("run Plage01 route");
        println!(
            "[route] player inventory after route: {:?}",
            outcome.session.inventory_items()
        );
        for (i, object) in outcome.session.vm().objects.iter().enumerate() {
            let id = i as xiii_script::ObjectId;
            if !object.deleted && outcome.session.vm().is_a(id, "keys") {
                println!(
                    "[route] key {} active={} Instigator={:?} Owner={:?} Inventory={:?} KeyCodeName={:?} ItemName={:?}",
                    object.name,
                    object.active,
                    outcome.session.vm().get_property(id, "Instigator"),
                    outcome.session.vm().get_property(id, "Owner"),
                    outcome.session.vm().get_property(id, "Inventory"),
                    outcome.session.vm().get_property(id, "KeyCodeName"),
                    outcome.session.vm().get_property(id, "ItemName"),
                );
            }
        }
        println!("[route] map objectives: {:?}", outcome.map_objectives);
        println!(
            "[route] end state: game_ended={:?}, controller={:?}, controller_state={:?}, controller_active={:?}",
            outcome.session.game_info.and_then(|gi| outcome
                .session
                .vm()
                .get_property(gi, "bGameEnded")
                .cloned()),
            outcome
                .session
                .controller
                .map(|c| outcome.session.vm().objects[c as usize].name.clone()),
            outcome
                .session
                .controller
                .and_then(|c| outcome.session.vm().state_name(c)),
            outcome
                .session
                .controller
                .map(|c| outcome.session.vm().objects[c as usize].active),
        );
        let level = outcome.session.vm().find_level_info();
        let mut listed = Vec::new();
        let mut current =
            level.and_then(
                |id| match outcome.session.vm().get_property(id, "ControllerList") {
                    Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(next)))) => {
                        Some(*next)
                    }
                    _ => None,
                },
            );
        for _ in 0..64 {
            let Some(id) = current else { break };
            listed.push(outcome.session.vm().objects[id as usize].name.clone());
            current = match outcome.session.vm().get_property(id, "NextController") {
                Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(next)))) => {
                    Some(*next)
                }
                _ => None,
            };
        }
        println!("[route] LevelInfo={level:?} ControllerList={listed:?}");
        for ev in &outcome.session.vm().trace {
            let rendered = format!("{:?}", ev.kind);
            if rendered.contains("ClientGameEnded")
                || rendered.contains("GameEndedSuccess")
                || rendered.contains("ServerTravel")
                || rendered.contains("AddController")
                || rendered.contains("RemoveController")
            {
                println!("[route] ending trace t={:.3}: {rendered}", ev.time);
            }
        }
        println!(
            "[route] travel: {:?}, final map {}",
            outcome.travel, outcome.final_map
        );
        let (plage, states) = outcome
            .map_objectives
            .first()
            .expect("Plage01 objective states");
        assert_eq!(plage, "Plage01");
        assert!(
            states.len() >= 2,
            "Plage01 MapInfo must expose its objectives: {states:?}"
        );
        assert!(
            states[0].primary && states[0].completed,
            "objective 0 (escape) must be promoted and completed: {:?}",
            states[0]
        );
        assert!(
            states[1].primary && states[1].completed,
            "objective 1 (truck) must be promoted and completed: {:?}",
            states[1]
        );
        // PlayerTick dispatch is what turns `PlayingVideo` into `ServerTravel`; the route reaches
        // it because `PlayerTick` now runs and `VideoPlayer.GetStatus` times `Cine01`.
        assert!(
            !outcome.travel.is_empty(),
            "the level must travel; blocked actors: {:?}",
            outcome.session.suspended
        );
        assert_eq!(outcome.final_map, "banque01");
        assert_eq!(outcome.travel[0].to, "banque01");
        assert!(
            !outcome
                .session
                .failures
                .iter()
                .any(|(_, error)| error.to_ascii_lowercase().contains("bcompleted")),
            "Plage01 checkpoint save hit the old bcompleted struct error: {:?}",
            outcome.session.failures
        );
        assert!(
            outcome.session.save_total > 0,
            "the game's checkpoint trigger must emit a SaveAtCheckpoint event"
        );
    }

    /// item33 acceptance: execute the tracked Plage01 route through the M60 fire burst, deliver
    /// Touch to the map's real checkpoint trigger (which runs GoSaving.DoSave), persist that
    /// checkpoint and reload it through AcceptInventory. The trigger Touch is harness-delivered;
    /// the save/restore and weapon/ammo/objective work is the authored game path.
    #[test]
    fn opt_in_plage01_checkpoint_restores_route_inventory_ammo_weapon_and_music() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage01".into()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let route_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/plage01_route.script");
        let mut script = script::Script::load(&route_path).expect("load tracked Plage01 route");
        script.events.retain(|event| event.t <= 64.0);
        let mut outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            65.0,
        )
        .expect("run Plage01 through its weapon fire and corpse search");
        assert_eq!(
            outcome.final_map, "Plage01",
            "route must stop before its travel command"
        );
        let requested_shots = script
            .events
            .iter()
            .filter(|event| matches!(event.command, script::Command::Fire))
            .count();
        assert!(
            requested_shots >= 10,
            "tracked route must fire a burst, got {requested_shots} fire commands"
        );
        let target = outcome
            .session
            .vm()
            .find_object("BaseSoldier6")
            .expect("route killer");
        assert!(
            outcome.session.actor_is_dead(target),
            "tracked route's fired M60 did not kill BaseSoldier6"
        );

        let weapon = outcome
            .session
            .player_weapon()
            .expect("route leaves a selected weapon");
        let weapon_class = outcome
            .session
            .vm()
            .set()
            .path(outcome.session.vm().objects[weapon as usize].class);
        assert!(
            weapon_class.to_ascii_lowercase().contains("m60"),
            "selected weapon after route: {weapon_class}"
        );
        let before_inventory = outcome.session.inventory_items();
        assert!(
            before_inventory
                .iter()
                .any(|(_, class)| class.eq_ignore_ascii_case(&weapon_class)),
            "selected weapon is absent from inventory chain: {before_inventory:?}"
        );

        let checkpoint = outcome.session.vm().objects.iter().enumerate().find_map(|(i, actor)| {
            let id = i as xiii_script::ObjectId;
            (actor.is_actor && outcome.session.vm().is_a(id, "xiiisavegametrigger")
                && matches!(outcome.session.vm().get_property(id, "Tag"), Some(xiii_script::Value::Name(tag)) if tag.eq_ignore_ascii_case("CP1")))
                .then_some(id)
        }).expect("Plage01 CP1 save trigger");
        let player = outcome.session.player;
        let trigger_class = outcome.session.vm().objects[checkpoint as usize].class;
        let trigger_loc = outcome.session.player_location().expect("player location");
        let save_trigger = outcome
            .session
            .vm_mut()
            .spawn_actor(
                player,
                Some(trigger_class),
                Some(player),
                None,
                Some(trigger_loc),
                None,
            )
            .expect("spawn a fresh instance of the map's checkpoint trigger class")
            .expect("checkpoint trigger spawn returned None");
        outcome.session.vm_mut().set_property(
            save_trigger,
            "Tag",
            0,
            xiii_script::Value::Name("CP1".into()),
        );
        outcome.session.vm_mut().set_property(
            save_trigger,
            "TeleporterName",
            0,
            xiii_script::Value::Str("teleporte_apres_lit".into()),
        );
        outcome.session.vm_mut().set_property(
            save_trigger,
            "SaveDescription",
            0,
            xiii_script::Value::Str("item33 Plage01 route".into()),
        );
        if let Some(sound) = outcome
            .session
            .vm()
            .get_property(checkpoint, "SoundToLaunch")
            .cloned()
        {
            assert!(
                outcome
                    .session
                    .vm_mut()
                    .set_property(save_trigger, "SoundToLaunch", 0, sound),
                "fresh checkpoint trigger could not receive SoundToLaunch"
            );
        }
        outcome
            .session
            .vm_mut()
            .send_event(
                save_trigger,
                "Touch",
                vec![xiii_script::Value::Object(Some(
                    xiii_script::ObjRef::Instance(player),
                ))],
            )
            .expect("deliver player Touch to checkpoint trigger");
        let loc = outcome.session.player_location().expect("player location");
        for _ in 0..120 {
            outcome.session.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &session::PlayerVMModes::default(),
            );
            if outcome.session.save_total > 0 {
                break;
            }
        }
        assert!(
            outcome.session.save_total > 0,
            "CP1 Touch did not run GoSaving.DoSave: {:?}",
            outcome.session.failures
        );
        let event = outcome
            .session
            .saves
            .back()
            .expect("game-emitted SaveCheckpoint event")
            .1
            .clone();
        assert_eq!(event.teleporter_name, "teleporte_apres_lit");
        let saved = outcome
            .session
            .checkpoint_snapshot(
                "Plage01",
                &event,
                loc,
                outcome.session.player_rotation().unwrap_or([0; 3]),
            )
            .expect("snapshot route checkpoint");
        assert_eq!(
            saved.save_trigger_tag, "CP1",
            "snapshot must retain XIIISaveGameTrigger.Tag"
        );
        assert!(
            saved.sound_to_launch.is_some(),
            "CP1 SoundToLaunch must be captured: event={event:?}, saved={saved:?}, original={:?}",
            outcome
                .session
                .vm()
                .get_property(checkpoint, "SoundToLaunch")
        );
        let ammo = saved
            .inventory
            .iter()
            .find_map(|item| {
                item.ammo_amount
                    .map(|amount| (item.class_path.clone(), amount))
            })
            .expect("saved travel inventory includes ammunition");
        assert!(ammo.1 >= 0, "invalid saved ammo count: {ammo:?}");
        println!(
            "[item33 route] checkpoint={} weapon={} ammo={ammo:?} health={} objectives={:?} sound={:?}",
            saved.checkpoint_number,
            saved.selected_weapon.as_deref().unwrap_or("<none>"),
            saved.health,
            saved.objectives,
            saved.sound_to_launch
        );

        let temp_dir = std::env::temp_dir();
        if let Ok(entries) = std::fs::read_dir(&temp_dir) {
            for entry in entries.flatten() {
                if entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("xiii-item33-route-")
                {
                    let _ = std::fs::remove_dir_all(entry.path());
                }
            }
        }
        let save_dir = temp_dir.join(format!("xiii-item33-route-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&save_dir);
        crate::save::write(&save_dir, 0, &saved).expect("write route checkpoint");
        let loaded = crate::save::read(&save_dir, 0).expect("read route checkpoint");
        let mut resumed =
            session::Session::open_checkpoint(&game_dir, "Plage01").expect("open checkpoint map");
        resumed
            .restore_checkpoint(&loaded)
            .expect("run AcceptInventory restore path");
        assert_eq!(resumed.player_health(), Some(saved.health));
        assert_eq!(
            resumed.player_weapon().map(|id| resumed
                .vm()
                .set()
                .path(resumed.vm().objects[id as usize].class)),
            saved.selected_weapon
        );
        let restored_inventory = resumed
            .inventory_snapshot()
            .expect("snapshot restored inventory for assertions");
        let restored_objectives = resumed
            .objective_states()
            .into_iter()
            .map(|objective| crate::save::Objective {
                completed: objective.completed,
                primary: objective.primary,
                anti_goal: objective.anti_goal,
            })
            .collect::<Vec<_>>();
        let restored_music_vars = resumed
            .music_variable_snapshot()
            .expect("snapshot restored LevelInfo music variables");
        let restored_weapon = resumed.player_weapon().expect("restored selected weapon");
        let restored_ammo_id = match resumed.vm().get_property(restored_weapon, "AmmoType") {
            Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(id)))) => *id,
            other => panic!("restored M60 has no instance AmmoType: {other:?}"),
        };
        let restored_ammo_count = match resumed.vm().get_property(restored_ammo_id, "AmmoAmount") {
            Some(xiii_script::Value::Int(n)) => *n,
            other => panic!("restored M60 AmmoType has no int AmmoAmount: {other:?}"),
        };
        let restored_ammo = (
            resumed
                .vm()
                .set()
                .path(resumed.vm().objects[restored_ammo_id as usize].class),
            restored_ammo_count,
        );
        assert_eq!(restored_ammo, ammo, "ammo must match the saved count");
        assert_eq!(restored_objectives, saved.objectives);
        assert_eq!(
            restored_music_vars, saved.music_vars,
            "LevelInfo music-variable state must restore"
        );
        let mut saved_classes = saved
            .inventory
            .iter()
            .map(|i| i.class_path.to_ascii_lowercase())
            .collect::<Vec<_>>();
        let mut restored_classes = restored_inventory
            .iter()
            .map(|i| i.class_path.to_ascii_lowercase())
            .collect::<Vec<_>>();
        saved_classes.sort();
        restored_classes.sort();
        let mut remaining = restored_classes.clone();
        for class in &saved_classes {
            let at = remaining
                .iter()
                .position(|restored| restored == class)
                .unwrap_or_else(|| {
                    panic!("saved inventory class {class} missing after load: {restored_classes:?}")
                });
            remaining.remove(at);
        }
        remaining.sort();
        assert_eq!(
            remaining,
            ["xiii.fistsammo", "xiii.xiiilefthand"],
            "only AcceptInventory's authored default ammo/left-hand entries should be added"
        );
        let mut saved_ammo_state = saved
            .inventory
            .iter()
            .filter_map(|item| {
                item.ammo_amount
                    .map(|amount| (item.class_path.to_ascii_lowercase(), amount))
            })
            .collect::<Vec<_>>();
        let mut restored_ammo_state = restored_inventory
            .iter()
            .filter(|item| {
                saved
                    .inventory
                    .iter()
                    .any(|original| original.class_path.eq_ignore_ascii_case(&item.class_path))
            })
            .filter_map(|item| {
                item.ammo_amount
                    .map(|amount| (item.class_path.to_ascii_lowercase(), amount))
            })
            .collect::<Vec<_>>();
        saved_ammo_state.sort();
        restored_ammo_state.sort();
        assert_eq!(
            restored_ammo_state, saved_ammo_state,
            "each travel Ammunition.AmmoAmount must survive the restore"
        );
        let saved_m60 = saved
            .inventory
            .iter()
            .find(|item| item.class_path.eq_ignore_ascii_case("XIII.m60"))
            .expect("saved M60 actor");
        let m60_ammo = saved
            .inventory
            .iter()
            .find(|item| item.class_path.eq_ignore_ascii_case("XIII.M60Ammo"))
            .and_then(|item| item.ammo_amount)
            .expect("saved M60 ammunition actor");
        let saved_clip = saved_m60.reload_count.expect("saved M60 ReloadCount");
        let expected_clip = saved_clip.min(m60_ammo);
        let loaded_clip = match resumed.vm().get_property(restored_weapon, "ReloadCount") {
            Some(xiii_script::Value::Int(v)) => *v,
            Some(xiii_script::Value::Byte(v)) => i32::from(*v),
            other => panic!("restored M60 ReloadCount missing/unsupported: {other:?}"),
        };
        assert_eq!(
            loaded_clip, expected_clip,
            "XIIIWeapon.Active.BeginState clamps ReloadCount to the remaining AmmoAmount on BringUp"
        );
        assert!(
            resumed.events.iter().any(|(_, event)| matches!(event,
            xiii_script::PresentationEvent::PlayMusic(music)
                if music.sound.as_deref() == saved.sound_to_launch.as_deref())),
            "AcceptInventory did not play saved SoundToLaunch {:?}",
            saved.sound_to_launch
        );
        let _ = std::fs::remove_dir_all(save_dir);
    }

    /// item24 opt-in corpus test: follow Banque01 to its end through the game's own chain.
    ///
    /// `MapInfo.NextMapLevelWithUnr` is `"Amos01.unr"` and `EndMapVideo` is `"cine02"` (848
    /// frames / 25 fps = 33.92 s), so the level-end path is the same `SetGoalComplete ->
    /// TestGoalComplete -> DoTravel -> EndGame -> GameEndedSuccess -> PlayingVideo ->
    /// PlayingVideo.PlayerTick -> ServerTravel` chain as Plage01. The route fixture
    /// (`tests/data/banque01_route.script`) walks onto the map's own trigger/volume actors and
    /// uses two labelled `set_goal` bridges for the objectives that three scripted bank scenes
    /// would otherwise promote (blockers B1-B3 in the item24 report). This test asserts the
    /// measured Banque01 objective states and the travel hop; it does not claim the cutscenes
    /// play.
    #[test]
    fn opt_in_banque01_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("banque01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import banque01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let route_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/banque01_route.script");
        let script = script::Script::load(&route_path).expect("load item24 banque01 route");
        let outcome = run_script(
            &game_dir,
            "banque01",
            &script,
            &resolved.params,
            &scene,
            90.0,
        )
        .expect("run banque01 route");
        let banque = outcome
            .map_objectives
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("banque01"))
            .expect("banque01 objective states");
        println!("[route] banque01 objectives: {:?}", banque.1);
        for obj in &banque.1 {
            println!(
                "[route]   [{}] primary={} completed={} anti_goal={} {:?}",
                obj.index, obj.primary, obj.completed, obj.anti_goal, obj.text
            );
        }
        println!(
            "[route] travel: {:?}, final map {}",
            outcome.travel, outcome.final_map
        );
        assert!(
            banque.1.len() >= 3,
            "Banque01 MapInfo must expose its objectives: {:?}",
            banque.1
        );
        // Objective 0 "Access the strongroom." must be primary and completed.
        assert!(
            banque.1[0].primary && banque.1[0].completed,
            "objective 0 (strongroom) must be promoted and completed: {:?}",
            banque.1[0]
        );
        // Objective 1 "Escape from the bank." must be primary and completed.
        assert!(
            banque.1[1].primary && banque.1[1].completed,
            "objective 1 (escape) must be promoted and completed: {:?}",
            banque.1[1]
        );
        // Objective 2 "Do not kill the bank staff." is the anti-goal, completed at level start
        // and still completed (the route kills no teller).
        assert!(
            banque.1[2].anti_goal,
            "objective 2 must be the anti-goal: {:?}",
            banque.1[2]
        );
        assert!(
            banque.1[2].completed,
            "the bank-staff anti-goal must remain completed: {:?}",
            banque.1[2]
        );
        // The game's own `TestGoalComplete`/`DoTravel`/`EndGame`/`ServerTravel` chain must run.
        // (`outcome.session` is the map the run travelled *to*, so the Banque01 touch log is only
        // in the run's stdout; the objective states above are captured as the run left Banque01.)
        assert!(
            !outcome.travel.is_empty(),
            "the level must travel; blocked actors: {:?}",
            outcome.session.suspended
        );
        assert_eq!(outcome.final_map, "Amos01");
        assert_eq!(outcome.travel[0].from, "banque01");
        assert_eq!(outcome.travel[0].to, "Amos01");
        assert_eq!(outcome.travel[0].url, "Amos01.unr");
    }

    /// item18 opt-in VM/session test for the `PlayerTick` dispatch itself: putting the real
    /// `XIIIPlayerController` into its `PlayingVideo` state and ticking once must run
    /// `PlayingVideo.PlayerTick`, which calls `Level.ServerTravel(MapInfo.NextMapLevelWithUnr)`
    /// when no `VideoPlayer` is held. Without the dispatch, the controller sits in `PlayingVideo`
    /// forever and no request appears.
    #[test]
    fn opt_in_plage01_player_tick_reaches_server_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = session::Session::open(&game_dir, "Plage01").expect("open Plage01");
        let ctrl = session.controller.expect("player controller");
        session
            .vm_mut()
            .goto_state(ctrl, "PlayingVideo", None)
            .expect("enter PlayingVideo");
        assert_eq!(
            session.vm().state_name(ctrl).as_deref(),
            Some("PlayingVideo")
        );
        // Use the app's tolerant tick: an unrelated suspended save trigger
        // (`XIIISaveGameTrigger.DoSave`'s undecoded save struct) must not abort the dispatch test.
        let _ = session.vm_mut().tick_suspending(0.05);
        let req = session.vm_mut().take_travel_request();
        assert!(
            req.as_ref()
                .is_some_and(|r| r.url.eq_ignore_ascii_case("banque01.unr")),
            "PlayingVideo.PlayerTick must request banque01; got {req:?}"
        );
    }

    /// item21 opt-in corpus test (requirement 4): a `PlayingVideo` with `cine01` reports
    /// completion only after the decoded frame count elapsed. The run drives the game's real
    /// level-end chain — `XIIIGameInfo.EndGame(None, "GoalComplete")` ->
    /// `GameEnded.BeginState` (XIIIEndGameType 4) -> `GameEndedSuccess.Timer` ->
    /// `VP.Open(MapInfo.EndMapVideo)` -> `PlayingVideo.BeginState` -> `VP.Play()` — with the
    /// host cutscene player installed on the session VM (headless form: caller-paced clock, no
    /// audio device). First let Plage01's opening sequence reach its decoded release handoff; the
    /// test does not rely on `take_control` or on suspending cine actors when `bGameEnded` changes.
    /// The game's own `PlayingVideo.PlayerTick` must then request the level
    /// travel when — and only when — the host has decoded every frame of `Cine01.bik`
    /// (1358 frames / 25 fps ≈ 54.32 s).
    #[test]
    fn opt_in_plage01_playingvideo_completes_when_decoded_frames_elapse() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = session::Session::open(&game_dir, "Plage01").expect("open Plage01");
        let host = crate::video::VideoHostHandle::new(game_dir.clone(), false);
        session.vm_mut().set_video_host(Box::new(host.clone()));
        let dt = 1.0f64 / 30.0;
        let intro_steps = (46.0 / dt) as usize;
        let mut location = session.player_location().expect("player location");
        let mut yaw = 0.0;
        let mut velocity = [0.0; 3];
        for _ in 0..intro_steps {
            session.drive_render_phase();
            session.step(
                dt as f32,
                location,
                yaw,
                velocity,
                &session::PlayerVMModes::default(),
            );
            if let Some((script_location, script_yaw, script_velocity)) = session.script_pawn_pose()
            {
                location = script_location;
                yaw = script_yaw;
                velocity = script_velocity;
            }
        }
        let controller = session.controller.expect("player controller");
        assert_ne!(
            session.vm().state_name(controller).as_deref(),
            Some("NoControl"),
            "Plage01's authored intro must release the player through its own script sequence"
        );
        let gi = session.game_info.expect("GameInfo");
        session
            .vm_mut()
            .send_event(
                gi,
                "EndGame",
                vec![
                    xiii_script::Value::Object(None),
                    xiii_script::Value::Str("GoalComplete".to_owned()),
                ],
            )
            .expect("EndGame(GoalComplete)");
        // Step until the chain's `PlayingVideo` state starts the host playback, then until the
        // game's own `PlayerTick` requests the travel. The host advances with the VM's time, so
        // travel can only be requested once every frame is decoded.
        let max_steps = (64.0f64 / dt).ceil() as usize;
        let mut duration_opt: Option<f64> = None;
        let mut started_at = None;
        let mut status_before_end = None;
        let mut travel_at = None;
        let t0 = std::time::Instant::now();
        for _i in 0..max_steps {
            session.step(
                dt as f32,
                [0.0; 3],
                0.0,
                [0.0; 3],
                &session::PlayerVMModes::default(),
            );
            if started_at.is_none() && host.is_playing() {
                started_at = Some(session.vm_time());
                duration_opt = Some(
                    host.frame_count().expect("open clip") as f64 / host.fps().expect("open clip"),
                );
                assert_eq!(
                    session.vm().video_status(),
                    1,
                    "the host playback just started; GetStatus must report playing"
                );
                assert_eq!(
                    session.vm().video_timing(),
                    Some(xiii_script::VideoTiming::Host),
                    "cine01 must be host-decoded, not duration-timed"
                );
            }
            if travel_at.is_none()
                && let Some(req) = session.take_travel_request()
            {
                assert!(
                    req.url.eq_ignore_ascii_case("banque01.unr"),
                    "PlayingVideo.PlayerTick must request banque01; got {:?}",
                    req.url
                );
                travel_at = Some(session.vm_time());
                break;
            }
            host.advance_virtual_all(dt);
            if let (Some(start), Some(dur), false) = (started_at, duration_opt, travel_at.is_some())
            {
                let played = session.vm_time() - start;
                if played >= dur - 1.0 && played < dur && status_before_end.is_none() {
                    status_before_end = Some(session.vm().video_status());
                }
            }
        }
        let wall = t0.elapsed().as_secs_f32();
        let start = started_at.unwrap_or_else(|| {
            panic!("the game's end chain must start the cutscene playback");
        });
        let duration = duration_opt.unwrap_or_else(|| {
            panic!("the clip must still be open when travel ends the state");
        });
        let Some(travel_at) = travel_at else {
            panic!(
                "PlayingVideo must end in travel once the decoded frames elapsed \
                 ({duration:.3}s of playback, started at VM {start:.3}, decoded {:?}/{:?})",
                host.decoded(),
                host.frame_count(),
            );
        };
        let waited = travel_at - start;
        println!(
            "[item21 test] cine01: {:?} frames, travel after {waited:.3}s of VM playback \
             ({duration:.3}s clip); decoded {:?}; wall {wall:.2}s",
            host.frame_count(),
            host.decoded(),
        );
        assert_eq!(
            status_before_end,
            Some(1),
            "GetStatus must still report playing 1 s before the clip ends"
        );
        assert!(
            waited >= duration - 2.0 * dt,
            "travel came too early: {waited:.3}s < duration {duration:.3}s (the clip must not \
             end before every frame is decoded)"
        );
        assert!(
            waited <= duration + 2.0 * dt,
            "travel came too late: {waited:.3}s > duration {duration:.3}s (GetStatus must \
             follow the host playback, not a longer timer)"
        );
        assert_eq!(
            host.decoded(),
            host.frame_count(),
            "the travel tick must have decoded every frame"
        );
    }

    /// Opt-in diagnostic probe (item15 evidence): prints the map's `MapInfo.Objectif` struct
    /// fields so the goal/primary/completed semantics are read from the decoded data, not guessed.
    #[test]
    fn opt_in_plage00_objectif_probe() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        for map in ["Plage00", "Plage01"] {
            probe_objectifs(&game_dir, map);
        }
    }

    fn probe_objectifs(game_dir: &std::path::Path, map: &str) {
        let session = session::Session::open(game_dir, map).expect("open map");
        let gi = session.game_info.expect("GameInfo");
        let mi = match session.vm().get_property(gi, "MapInfo") {
            Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(id)))) => *id,
            other => panic!("MapInfo is {other:?}"),
        };
        println!(
            "[probe] {map} MapInfo {} next={:?} keepInventory={:?}",
            session.vm().objects[mi as usize].name,
            session.vm().get_property(mi, "NextMapLevelWithUnr"),
            session.vm().get_property(mi, "NextMapKeepInventory"),
        );
        let mut n = 0;
        while let Some(v) = session.vm().get_property_elem(mi, "Objectif", n) {
            println!("[probe] {map} Objectif[{n}] = {v:?}");
            n += 1;
        }
        println!("[probe] {map}: {n} objective(s)");
    }

    /// Opt-in corpus test (item15): Plage00's objective completes through the game's own
    /// `XIIIGoalTrigger5` -> `MapInfo.SetGoalComplete` -> `DoTravel` -> `EndGame` ->
    /// `GameEndedSuccess` -> `Level.ServerTravel(NextMapLevelWithUnr, true)` chain, the VM reports
    /// a travel request, and the host loads the next map (Plage01) with a spawned player.
    #[test]
    fn opt_in_plage00_goal_trigger_requests_travel_to_plage01() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage00".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage00");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        // `XIDCine.BeachFinalFall0` (Event `objectif0`, Tag `Fall`) at (5504.6, -1360.3, 867)
        // is the beach end trigger: `Engine.Trigger.Touch` fires `TriggerEvent('objectif0')`,
        // which calls `XIIIGoalTrigger5.Trigger`. Teleport next to it and walk in, then wait for
        // the game's own travel request.
        let script = script::Script::parse(
            "t=0.0 teleport 5420 -1360.3 880\nt=0.0 yaw 0\nt=0.0 forward 1\nt=2.0 forward 0\nt=2.0 wait_travel\n",
        )
        .unwrap();
        let outcome = run_script(
            &game_dir,
            "Plage00",
            &script,
            &resolved.params,
            &scene,
            40.0,
        )
        .expect("run Plage00 goal walk");
        println!(
            "[probe] travel hops: {:?}, final map {}",
            outcome.travel, outcome.final_map
        );
        assert!(
            !outcome.travel.is_empty(),
            "the goal trigger did not request travel; touches: {:?}, suspended: {:?}",
            outcome.session.touches(),
            outcome.session.suspended
        );
        assert_eq!(outcome.final_map, "Plage01");
        assert_eq!(outcome.travel[0].to, "Plage01");
        assert!(
            outcome.session.player_location().is_some(),
            "the next map must have a spawned player"
        );
    }

    /// Opt-in corpus test (item15): the game's own level-complete chain requests travel when the
    /// goal trigger's `Trigger` runs. This calls `XIIIGoalTrigger5.Trigger` directly (the event a
    /// cutscene's `TriggerEvent('objectif0')` delivers), then asserts `MapInfo.SetGoalComplete` ->
    /// `DoTravel` -> `EndGame` -> `GameEndedSuccess` -> `ServerTravel` reaches the VM travel
    /// request. The walk-in test below exercises the same chain from a scripted walk.
    #[test]
    fn opt_in_plage00_goal_completion_chain_requests_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = session::Session::open(&game_dir, "Plage00").expect("open Plage00");
        let gt5 = session
            .vm()
            .find_object("XIIIGoalTrigger5")
            .expect("XIIIGoalTrigger5");
        let beach = session
            .vm()
            .find_object("BeachFinalFall0")
            .expect("BeachFinalFall0");
        let pawn = session.player;
        let args = vec![
            xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(beach))),
            xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(pawn))),
        ];
        session
            .vm_mut()
            .send_event(gt5, "Trigger", args)
            .expect("XIIIGoalTrigger5.Trigger");
        for _ in 0..300 {
            let loc = session.player_location().unwrap_or([0.0; 3]);
            session.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &session::PlayerVMModes::default(),
            );
            if let Some(req) = session.take_travel_request() {
                println!(
                    "[probe] goal chain travel request from {:?}: {} -> {}",
                    req.source, req.url, req.mode
                );
                assert_eq!(req.url, "Plage01.unr");
                return;
            }
        }
        panic!(
            "goal chain did not request travel; suspended {:?}; first error {:?}",
            session.suspended,
            session.first_error()
        );
    }

    /// Opt-in corpus test: walking (from a harness-placed start) into Plage00's `TouchTrigger2`
    /// volume fires `Touch` through the VM cylinder overlap and the `TouchTrigger2 ->
    /// XIIIDispatcher0` chain reaches `Fin`. No harness-delivered `Touch` is used anywhere in the
    /// play session: the only Touch comes from the host writing the pawn Location each tick and
    /// calling `refresh_touching_of`.
    #[test]
    fn opt_in_plage00_walking_into_trigger_fires_dispatcher() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage00".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage00");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        // Walk from just short of TouchTrigger2 (3635.5, -44620.2, 4918) into its 130 UU cylinder.
        let script = script::Script::parse(
            "t=0.0 teleport 3380 -44620.2 4918\nt=0.0 yaw 0\nt=0.0 forward 1\nt=0.5 forward 0\n",
        )
        .unwrap();
        let outcome = run_script(&game_dir, "Plage00", &script, &resolved.params, &scene, 8.0)
            .expect("run Plage00 trigger walk");
        let s = &outcome.session;
        let touch = s.touches().into_iter().find(|(_, a)| a == "TouchTrigger2");
        let (t, actor) = touch.unwrap_or_else(|| {
            panic!(
                "TouchTrigger2 was not touched from walking: {:?}",
                s.touches()
            )
        });
        assert_eq!(actor, "TouchTrigger2");
        assert!(
            (0.0..2.0).contains(&t),
            "touch time {t}s is not from the scripted walk"
        );
        assert_eq!(
            s.dispatcher_state().as_deref(),
            Some("Fin"),
            "dispatcher did not reach Fin (state {:?})",
            s.dispatcher_state()
        );
        println!(
            "[play test] Plage00 trigger walk: touch {actor} at {t:.3}s, dispatcher {:?}, player {} active, suspended {}",
            s.dispatcher_state(),
            s.active_actors(),
            s.suspended.len()
        );
    }

    /// Opt-in corpus test (Part B): the level-start message/objective path creates real HUD
    /// widget objects. Before the fixes the message widgets aborted (`DeferredWithReturnValue`
    /// on `Message.static.GetString` and a `void` `default.Class`); now `ClientSetHUD` spawns
    /// `XIIIBaseHud`, `MapInfo.Timer` runs `FirstFrame`, and the HUD message path completes.
    #[test]
    fn opt_in_plage00_hud_widgets_appear_after_level_start() {
        const WIDGETS: [&str; 8] = [
            "HudMsg",
            "HudObjMsg",
            "HudMPMsg",
            "HudEndMsg",
            "HudDlg",
            "HudWnd",
            "HudFoc",
            "HudStt",
        ];
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = session::Session::open(&game_dir, "Plage00").expect("open Plage00");
        let hud = {
            let vm = session.vm();
            session
                .controller
                .and_then(|c| match vm.get_property(c, "myHUD") {
                    Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(p)))) => {
                        Some(*p)
                    }
                    _ => None,
                })
                .or_else(|| hud::find_hud(vm))
                .expect("the script login must create a live HUD actor")
        };
        let mut found: Vec<(String, xiii_script::ObjectId)> = Vec::new();
        for _ in 0..180 {
            let loc = session.player_location().unwrap_or([0.0; 3]);
            session.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &crate::play::session::PlayerVMModes::default(),
            );
        }
        {
            let vm = session.vm();
            for name in WIDGETS {
                if let Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(id)))) =
                    vm.get_property(hud, name)
                {
                    let live = vm.objects.get(*id as usize).is_some_and(|o| !o.deleted);
                    println!(
                        "[play test] HUD widget {name} -> {}{}",
                        vm.objects[*id as usize].name,
                        if live { "" } else { " (deleted)" }
                    );
                    if live {
                        found.push((name.to_owned(), *id));
                    }
                }
            }
        }
        println!(
            "[play test] HUD {} widgets live after 3s: {:?}",
            found.len(),
            found.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>()
        );
        assert!(
            !found.is_empty(),
            "the HUD must create at least one widget after level start"
        );
        // The class-default object of the goal message must no longer be suspended by the
        // static `GetString` call.
        assert!(
            !session
                .suspended
                .iter()
                .any(|s| s == "Default__XIIIGoalMessage"),
            "Default__XIIIGoalMessage must not be suspended: {:?}",
            session.suspended
        );

        // Drive one `HUD.PostRender` with the live widgets and assert the retail draw commands
        // (player info + the live widget) are recorded, using the synthetic font provider.
        let canvas_class =
            xiii_world::runtime::resolve_class_path(session.vm().set(), "Engine.Canvas")
                .expect("Engine.Canvas class");
        let canvas = session
            .vm_mut()
            .spawn(canvas_class, "WidgetTestCanvas")
            .expect("spawn Canvas");
        let selected =
            hud::initialize_canvas_fonts(session.vm_mut(), canvas).expect("Canvas native fonts");
        assert!(
            selected[0].ends_with(".PoliceF16") && selected[1].ends_with(".PoliceF20"),
            "Canvas fonts must match Engine.Canvas.Init: {selected:?}"
        );
        for (prop, path) in [("SmallFont", &selected[0]), ("MedFont", &selected[1])] {
            assert_eq!(
                session.vm().get_property(canvas, prop),
                Some(&xiii_script::Value::Name(path.clone())),
                "Canvas.{prop} must match the Engine.Canvas.Init font"
            );
        }
        let vm = session.vm_mut();
        vm.set_property(canvas, "ClipX", 0, xiii_script::Value::Float(1280.0));
        vm.set_property(canvas, "ClipY", 0, xiii_script::Value::Float(720.0));
        vm.set_property(canvas, "Style", 0, xiii_script::Value::Byte(1));
        vm.set_canvas_fonts(Box::new(DummyFonts));
        let arg = xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(canvas)));
        session
            .vm_mut()
            .send_event(hud, "PostRender", vec![arg])
            .expect("HUD.PostRender with live widgets");
        let commands = session.vm_mut().drain_canvas();
        println!(
            "[play test] HUD.PostRender after level start: {} draw command(s)",
            commands.len()
        );
        for c in commands.iter().take(8) {
            println!("[play test]   {c:?}");
        }
        assert!(
            commands.len() >= 2,
            "HUD.PostRender must draw more than the player-info line once widgets are live: {}",
            commands.len()
        );
    }

    /// A synthetic `CanvasFonts` provider for any font: 4 units per character, 8 tall.
    struct DummyFonts;
    impl xiii_script::canvas::CanvasFonts for DummyFonts {
        fn measure(&self, _font: &str, text: &str) -> Option<(f32, f32)> {
            Some((text.chars().count() as f32 * 4.0, 8.0))
        }
    }

    /// Opt-in corpus test (requirement 5): the real Plage00 `XIIIBaseHud.PostRender(Canvas)`
    /// runs through the VM against a host-created `Engine.Canvas`, does not suspend the HUD and
    /// records at least one draw command. No Bevy assets are needed: the font provider is the
    /// synthetic one above, so the test covers the script + native path only.
    #[test]
    fn opt_in_plage00_hud_postrender_records_commands() {
        use xiii_script::Value;
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = session::Session::open(&game_dir, "Plage00").expect("open Plage00");
        assert_eq!(session.login_script, 1, "script login must create the HUD");
        let controller = session.controller;

        // Host-side Canvas (the same setup `hud::setup` performs), minus the Bevy textures.
        let canvas_class =
            xiii_world::runtime::resolve_class_path(session.vm().set(), "Engine.Canvas")
                .expect("Engine.Canvas class");
        let canvas = session
            .vm_mut()
            .spawn(canvas_class, "TestCanvas")
            .expect("spawn Canvas");
        let player = session.player;
        let missing_font = hud::initialize_canvas_fonts(session.vm_mut(), player)
            .expect_err("a pawn must not be accepted as an Engine.Canvas");
        assert!(
            missing_font.contains("has no SmallFont property"),
            "unexpected Engine.Canvas initialization error: {missing_font}"
        );
        let vm = session.vm_mut();
        vm.set_property(
            canvas,
            "DrawColor",
            0,
            Value::Struct(vec![
                ("b".to_owned(), Value::Byte(255)),
                ("g".to_owned(), Value::Byte(255)),
                ("r".to_owned(), Value::Byte(255)),
                ("a".to_owned(), Value::Byte(255)),
            ]),
        );
        vm.set_property(canvas, "ClipX", 0, Value::Float(1280.0));
        vm.set_property(canvas, "ClipY", 0, Value::Float(720.0));
        vm.set_property(canvas, "Style", 0, Value::Byte(1));
        vm.set_canvas_fonts(Box::new(DummyFonts));

        // The HUD is the controller's `myHUD` (or the first live HUD actor).
        let hud = {
            let vm = session.vm();
            controller
                .and_then(|c| match vm.get_property(c, "myHUD") {
                    Some(Value::Object(Some(xiii_script::ObjRef::Instance(p)))) => Some(*p),
                    _ => None,
                })
                .or_else(|| hud::find_hud(vm))
        }
        .expect("Plage00 has a live HUD");
        let selected =
            hud::initialize_canvas_fonts(session.vm_mut(), canvas).expect("Canvas native fonts");
        assert_eq!(
            session.vm().get_property(canvas, "SmallFont"),
            Some(&Value::Name(selected[0].clone()))
        );
        assert_eq!(
            session.vm().get_property(canvas, "MedFont"),
            Some(&Value::Name(selected[1].clone()))
        );
        let vm = session.vm_mut();
        let arg = Value::Object(Some(xiii_script::ObjRef::Instance(canvas)));
        match vm.send_event(hud, "PostRender", vec![arg]) {
            Ok(_) => {}
            Err(e) => panic!("HUD.PostRender failed: {e}"),
        }
        let commands = session.vm_mut().drain_canvas();
        let hud_class = session
            .vm()
            .set()
            .path(session.vm().objects[hud as usize].class);
        assert!(
            session.vm().objects[hud as usize].active,
            "the HUD must not be suspended by PostRender"
        );
        println!(
            "[play test] Plage00 HUD.PostRender: class {hud_class}, {} draw command(s)",
            commands.len()
        );
        for c in commands.iter().take(6) {
            println!("[play test]   {c:?}");
        }
        println!(
            "[play test]   HUD widgets: HudMsg={:?} HudWnd={:?} DrawnWeapon={:?}",
            session.vm().get_property(hud, "HudMsg"),
            session.vm().get_property(hud, "HudWnd"),
            session.vm().get_property(hud, "DrawnWeapon"),
        );
        // Survey of the Canvas/HUD natives this PostRender path called (requirement 1).
        let vm = session.vm();
        let mut used: Vec<(&String, &(Option<u16>, u64))> = vm
            .natives_used
            .iter()
            .filter(|(p, _)| p.starts_with("Canvas.") || p.starts_with("HUD."))
            .collect();
        used.sort_by(|a, b| a.0.cmp(b.0));
        for (path, (idx, count)) in used {
            println!("[play test]   native {path} index={idx:?} calls={count}");
        }
        assert!(
            !commands.is_empty(),
            "Plage00 HUD.PostRender produced no draw commands"
        );
    }

    /// Opt-in corpus fight (item14): on Plage01 the player's granted Beretta fires through the
    /// game's own `XIIIWeapon.Fire` -> `RealTraceFire` -> `XIIIBulletsAmmo.ProcessTraceHit` ->
    /// `XIIIPawn.TakeDamage` chain and kills `BaseSoldier6`. No host damage is applied: only the
    /// script `Fire` entry and the host view direction are supplied. The soldier's `Health` must
    /// fall and `bIsDead` become true.
    #[test]
    fn opt_in_plage01_player_fires_and_kills_soldier() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        // BaseSoldier6 is at (1802.4, -12832.0, 1070.8). Place the player 160 UU in -Y facing +Y
        // (yaw 90) and aim at the top of the head (pitch +5 deg). With the decoded per-bone hit
        // boxes (item14b) the large `X Spine1` box overlaps the lower head, so a point-blank
        // horizontal shot is a chest hit; the head needs the ray to clear the torso first. The
        // battle is entirely script-driven (no host damage).
        let script = script::Script::parse(
            "t=45.00 weapon XIII.Beretta\n\
             t=45.20 teleport 1802.4131 -12992.034 1070.843\n\
             t=45.20 yaw 90\n\
             t=45.20 pitch 5\n\
             t=45.30 fire\nt=45.90 fire\nt=46.50 fire\nt=47.10 fire\nt=47.70 fire\nt=48.30 fire\n\
             t=48.90 fire\nt=49.50 fire\nt=50.10 fire\nt=50.70 fire\nt=51.30 fire\n",
        )
        .unwrap();
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            53.0,
        )
        .expect("run Plage01 fight");
        let s = &outcome.session;
        let soldier = (0..s.vm().objects.len())
            .find(|&i| {
                s.vm().objects[i].is_actor
                    && !s.vm().objects[i].deleted
                    && s.vm().objects[i].name.eq_ignore_ascii_case("BaseSoldier6")
            })
            .expect("Plage01 has a live BaseSoldier6")
            as xiii_script::ObjectId;
        let health = s.actor_health(soldier);
        let dead = s.actor_is_dead(soldier);
        let weapon = s.player_weapon();
        println!(
            "[fight test] player weapon {:?}, BaseSoldier6 health {health:?} dead={dead}",
            weapon.map(|w| s.vm().objects[w as usize].name.clone())
        );
        assert!(weapon.is_some(), "the player must hold the granted weapon");
        assert!(
            dead || health.is_some_and(|h| h <= 0.0),
            "BaseSoldier6 did not die: health {health:?}, dead {dead}"
        );
    }

    /// Opt-in item18 B11 regression: the M60's decoded `RumbleFX` calls float `%` before its
    /// `TraceFire`. The VM must implement `Percent_FloatFloat` so the M60 continues through
    /// `ProcessTraceHit` -> `TakeDamage` instead of aborting in `IncrementFlashCount`.
    #[test]
    fn opt_in_plage01_m60_fires_through_rumblefx_and_damages_soldier() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Plage01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Plage01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let script = script::Script::parse(
            "t=0.00 weapon XIII.M60\n\
             t=0.20 teleport 1802.4131 -12992.034 1070.843\n\
             t=0.20 yaw 90\nt=0.20 pitch 5\n\
             t=0.30 fire\nt=0.90 fire\nt=1.50 fire\nt=2.10 fire\nt=2.70 fire\n",
        )
        .expect("parse M60 fight script");
        let outcome = run_script(&game_dir, "Plage01", &script, &resolved.params, &scene, 4.0)
            .expect("run Plage01 M60 fight");
        let soldier = outcome
            .session
            .vm()
            .find_object("BaseSoldier6")
            .expect("Plage01 BaseSoldier6");
        let health = outcome.session.actor_health(soldier);
        let ammo_amount = outcome.session.player_weapon().and_then(|w| {
            match outcome.session.vm().get_property(w, "AmmoType") {
                Some(Value::Object(Some(xiii_script::ObjRef::Instance(a)))) => {
                    match outcome.session.vm().get_property(*a, "AmmoAmount") {
                        Some(Value::Int(n)) => Some(*n),
                        _ => None,
                    }
                }
                _ => None,
            }
        });
        println!(
            "[M60 test] BaseSoldier6 health {:?}, bone {:?}, M60 ammo {:?}",
            health,
            outcome.session.vm().get_property(soldier, "LastBoneHit"),
            ammo_amount
        );
        assert!(
            health.is_some_and(|h| h < 625.0),
            "M60 fire stopped before damaging BaseSoldier6 (Health {health:?})"
        );
    }

    /// Opt-in item18 B11 comparison: use the same Base01 clear-line M60 script as the windowed
    /// `fixed_step` probe (`local/re/fight-base01-clear.script`). The trace first intersects
    /// BaseSoldier3, not BaseSoldier17; the actual trace target must take damage in the headless
    /// `run_script` path too, proving `Session::fire`/`ProcessTraceHit` agree across drivers.
    #[test]
    fn opt_in_base01_m60_run_script_damages_actual_trace_target() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Base01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Base01");
        let resolved = resolve_params(&game_dir).expect("resolve parameters");
        let script = script::Script::parse(
            "t=0.00 weapon XIII.M60\nt=0.20 teleport -10347.6 6904.2 867.0\
             \nt=0.20 yaw -90\nt=0.20 pitch 0\nt=0.40 fire\nt=1.00 fire\
             \nt=1.60 fire\nt=2.20 fire\nt=2.80 fire\n",
        )
        .expect("parse clear-line M60 script");
        let outcome = run_script(&game_dir, "Base01", &script, &resolved.params, &scene, 4.0)
            .expect("run Base01 clear-line M60 script");
        let target = outcome
            .session
            .vm()
            .find_object("BaseSoldier3")
            .expect("the ray's first pawn target");
        let health = outcome.session.actor_health(target);
        println!(
            "[B11 comparison] run_script BaseSoldier3 Health={health:?}, LastBoneHit={:?}",
            outcome.session.vm().get_property(target, "LastBoneHit")
        );
        assert!(
            health.is_some_and(|h| h < 750.0),
            "the actual trace target did not take M60 damage: {health:?}"
        );
    }

    /// Opt-in corpus test (requirement 5): on Banque01 the host walks the player onto the
    /// `LadderVolume5` brush, enters `PHYS_Ladder` and climbs up `ClimbDir` at `LadderSpeed`.
    /// The script teleports to the volume floor (the same harness bootstrap the other map demos
    /// use) and then *only* holds forward; the climb is the simulation's, not a teleport.
    #[test]
    fn opt_in_banque01_ladder_climb() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Banque01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Banque01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let volumes = xiii_world::movement_volumes::MovementVolumes::import(&game_dir, "Banque01")
            .expect("import Banque01 volumes");
        let lv = volumes
            .volumes
            .iter()
            .find(|v| {
                v.kind == xiii_world::movement_volumes::VolumeKind::Ladder
                    && v.name.eq_ignore_ascii_case("LadderVolume5")
            })
            .expect("Banque01 places LadderVolume5");
        let cx = (lv.min_uu[0] + lv.max_uu[0]) * 0.5;
        let cy = (lv.min_uu[1] + lv.max_uu[1]) * 0.5;
        let start_z = lv.min_uu[2] + resolved.params.height_uu;
        let script = script::Script::parse(&format!(
            "t=0.0 teleport {cx} {cy} {start_z}\nt=0.0 forward 1\nt=6.0 forward 0\n"
        ))
        .unwrap();
        let outcome = run_script(
            &game_dir,
            "Banque01",
            &script,
            &resolved.params,
            &scene,
            8.0,
        )
        .expect("run Banque01 ladder climb");
        let max_z = outcome
            .trace
            .iter()
            .map(|(_, _, p, _)| p[2])
            .fold(f32::MIN, f32::max);
        println!(
            "[ladder test] LadderVolume5 {:?} size {:?}; player climbed from {} to {max_z} UU (Health {})",
            lv.min_uu,
            lv.size_uu(),
            start_z,
            outcome
                .session
                .player_health()
                .map(|h| h.to_string())
                .unwrap_or_else(|| "-".into())
        );
        assert!(
            max_z > start_z + 200.0,
            "walking onto the ladder only raised the player to {max_z} UU (start {start_z})"
        );
    }

    /// Opt-in corpus test (requirement 5): Banque01's `WaterVolume0` brush is found by the host
    /// box query and drives the swim mode (`PHYS_Swimming`, `WaterSpeed`) for a player standing
    /// in the water. The thin (16 UU) water slab is only touched by the pawn's box, which is why
    /// the query uses the extent box rather than the centre point.
    #[test]
    fn opt_in_banque01_water_volume_swim() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Banque01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Banque01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let volumes = xiii_world::movement_volumes::MovementVolumes::import(&game_dir, "Banque01")
            .expect("import Banque01 volumes");
        let wv = volumes
            .volumes
            .iter()
            .find(|v| v.kind == xiii_world::movement_volumes::VolumeKind::Water)
            .expect("Banque01 places WaterVolume0");
        let center = [
            (wv.min_uu[0] + wv.max_uu[0]) * 0.5,
            (wv.min_uu[1] + wv.max_uu[1]) * 0.5,
            (wv.min_uu[2] + wv.max_uu[2]) * 0.5,
        ];
        let water_size = wv.size_uu();
        let motion = movement_modes::VolumeMotion::new(volumes);
        let world = CollisionWorld::new(scene.box_collision());
        let mut sim = PlayerSim::new(center, 0.0);
        sim.grounded = false;
        let start = sim.location;
        let mut saw_water = false;
        for _ in 0..30 {
            sim.step_with_modes(
                1.0 / 60.0,
                &world,
                &resolved.params,
                Input {
                    forward: 1.0,
                    ..Default::default()
                },
                &scene.collision_sources,
                &motion,
            );
            if sim.in_water {
                saw_water = true;
            }
        }
        println!(
            "[water test] WaterVolume0 centre {center:?} size {water_size:?}; state {} physics {} moved {} UU",
            sim.state(),
            sim.physics,
            (sim.location[0] - start[0]).abs()
        );
        assert!(saw_water, "the water box query never found WaterVolume0");
        assert_eq!(sim.physics, crate::play::sim::PHYS_SWIMMING);
    }

    /// Opt-in corpus test (requirement 4): finds a real low-clearance spot on a campaign map
    /// where the standing box collides with the ceiling but the crouch box does not, and checks
    /// the reverse at a clear spot. The spot is discovered by scanning navigation points (their
    /// `Location.Z - CollisionHeight` is the floor) and casting a ceiling ray, so it is evidence,
    /// not a hard-coded coordinate.
    #[test]
    fn opt_in_crouch_fits_where_standing_does_not() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        for map in ["Banque01", "Plage01", "Plage00"] {
            let opts = Options {
                map: Some(map.to_owned()),
                game_dir: Some(game_dir.clone()),
                ..Default::default()
            };
            let scene = viewer::load_scene(&opts).expect("import map");
            let resolved = resolve_params(&game_dir).expect("resolve player parameters");
            let params = resolved.params;
            let world = CollisionWorld::new(scene.box_collision());
            let mut cache = xiii_world::PackageCache::open(&game_dir).expect("cache");
            let mut defaults = xiii_world::ClassDefaults::open(&game_dir).expect("defaults");
            let nav = xiii_world::navigation::decode_navigation(&mut cache, &mut defaults, map)
                .expect("navigation");
            let standing_half = params.half_extents_bevy_for(false);
            let crouch_half = params.half_extents_bevy_for(true);
            for p in &nav.points {
                let floor = p.location[2] - p.collision_height;
                let up_start = to_bevy_position([p.location[0], p.location[1], floor + 1.0]);
                let up_end = to_bevy_position([p.location[0], p.location[1], floor + 400.0]);
                let Some(hit) = world.ray(up_start, up_end) else {
                    continue;
                };
                let ceiling = floor + hit.t * 400.0;
                let clearance = ceiling - floor;
                if !(100.0..148.0).contains(&clearance) {
                    continue;
                }
                let stand_center =
                    to_bevy_position([p.location[0], p.location[1], floor + params.height_uu]);
                let crouch_center = to_bevy_position([
                    p.location[0],
                    p.location[1],
                    floor + params.crouch_height_uu,
                ]);
                let stand_blocked = !world.overlap_aabb(stand_center, standing_half).is_empty();
                let crouch_clear = world.overlap_aabb(crouch_center, crouch_half).is_empty();
                if stand_blocked && crouch_clear {
                    println!(
                        "[crouch test] {map} {} at {:?} UU: floor {floor:.1}, clearance {clearance:.1} UU; standing box blocked, crouch box clear",
                        p.class, p.location
                    );
                    return;
                }
            }
        }
        panic!(
            "no low-clearance spot (100..148 UU) where standing is blocked and crouch fits was found"
        );
    }
}
