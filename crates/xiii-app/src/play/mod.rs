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

pub mod cinematics;
pub mod hud;
pub mod movement_modes;
pub mod movers;
pub mod pawns;
pub mod script;
pub mod session;
pub mod sim;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use bevy::camera::visibility::RenderLayers;
use bevy::ecs::system::{NonSend, NonSendMut};
use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::mesh::skinning::SkinnedMeshInverseBindposes;
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
use bevy::time::Fixed;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

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
struct PlayConfig {
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

#[derive(Resource)]
struct TraceState {
    tick: u64,
    start: Instant,
    exit_secs: Option<f32>,
    shot: u8,
    shot_done: bool,
    target_at: Option<Instant>,
}

#[derive(Resource, Default)]
struct ShotFlag(bool);

#[derive(Component)]
struct PlayCam;

#[derive(Component)]
struct PlayOverlay;

/// Cursor into the VM trace for the particle-trigger host bridge (index of the next unread
/// trace record).
#[derive(Resource, Default)]
struct ParticleTriggerCursor {
    trace_len: usize,
}

impl Plugin for PlayPlugin {
    fn build(&self, app: &mut App) {
        // Load the script session before the window opens. `Session` holds `Rc`-based VM state
        // (it is `!Send`), so it lives in a non-send resource on the main thread; a load failure
        // is stored and reported by `setup`, which exits with an error.
        let game_dir = self.options.game_dir.clone().unwrap_or_default();
        let map = self.options.map.clone().unwrap_or_default();
        let t0 = Instant::now();
        let mut session = session::Session::open(&game_dir, &map);
        println!(
            "[play] script session open (scripts, begin-play, providers): {:.2}s",
            t0.elapsed().as_secs_f32()
        );
        if let Ok(s) = session.as_mut() {
            s.enable_native_timers(self.options.perf_natives);
        }
        app.insert_non_send(session);
        app.insert_resource(PlayConfig {
            options: self.options.clone(),
        })
        .insert_resource(ClearColor(Color::srgb(0.45, 0.62, 0.82)))
        .insert_resource(Time::<Fixed>::from_hz(FIXED_HZ))
        .init_resource::<ShotFlag>()
        .init_resource::<RenderSync>()
        .init_resource::<ParticleTriggerCursor>()
        .add_plugins(viewer::particles::ParticlePlugin)
        .init_resource::<cinematics::CinematicState>()
        .add_systems(Startup, setup)
        .add_systems(FixedUpdate, fixed_step)
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
                sync_particle_triggers,
                pawns::update_pawns,
                hud::refresh,
                hud::draw,
                overlay,
                unattended,
            )
                .chain(),
        )
        .add_systems(Last, cinematics::report_exit);
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
    mut images: ResMut<Assets<Image>>,
    mut bindposes: ResMut<Assets<SkinnedMeshInverseBindposes>>,
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
        &mut images,
        &mut bindposes,
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
    images: &mut Assets<Image>,
    bindposes: &mut Assets<SkinnedMeshInverseBindposes>,
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
    println!(
        "[play] login path: script={} bootstrap={}",
        session.login_script, session.login_bootstrap
    );
    println!(
        "[play] localisation: language={} localized class-default overrides={}",
        session.localization_language, session.localized_overrides
    );
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
    let start_center = bevy_to_unreal_position(spawn.position);
    let start_rot = match (session.login_script, session.player_rotation()) {
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
    let geometry = viewer::spawn_scene_geometry(
        commands,
        meshes,
        materials,
        images,
        &scene,
        opts.lighting == crate::cli::Lighting::Baked,
        opts.particles == crate::cli::Particles::All,
    );
    for (o, entity) in scene.objects.iter().zip(&geometry) {
        let actor = o
            .path
            .split_once(" -> ")
            .map_or(o.path.as_str(), |(a, _)| a)
            .to_owned();
        sync.entities.entry(actor).or_default().push(*entity);
    }
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
    commands.spawn((
        Camera3d::default(),
        viewer::main_camera_config(sky_enabled),
        RenderLayers::layer(viewer::MAIN_LAYER),
        Transform::from_translation(Vec3::from_array(eye)),
        PlayCam,
    ));
    if sky_enabled && let Some(p) = sky_position {
        viewer::spawn_sky_camera(commands, p);
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
    commands.insert_resource(WorldRes {
        world,
        sources,
        movers: mover_collision,
    });
    commands.insert_resource(ScriptRes { drive });
    commands.insert_resource(TraceState {
        tick: 0,
        start: Instant::now(),
        exit_secs,
        shot: 0,
        shot_done: false,
        target_at: None,
    });
    println!(
        "[play] setup complete in {:.2}s",
        started.elapsed().as_secs_f32()
    );
    Ok(())
}

fn read_keyboard(keys: &ButtonInput<KeyCode>) -> Input {
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
            let outcome = sess.use_mover(&target);
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
    mut script: ResMut<ScriptRes>,
    mut state: ResMut<TraceState>,
    mut session: NonSendMut<Result<session::Session, String>>,
    sync: Res<RenderSync>,
    motion: Res<MotionRes>,
    mut transforms: Query<&mut Transform>,
    mut perf: ResMut<crate::perf::Perf>,
) {
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
    let input = if suppressed {
        Input::default()
    } else {
        match script.drive.as_mut() {
            Some(drive) => drive.advance(elapsed, &mut sim.0),
            None => read_keyboard(&keys),
        }
    };
    let use_action = input.use_action;
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
        if use_action {
            perform_use(sess, &wr.world, &wr.sources, &sim.0, &params.0);
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

/// Host bridge for triggered emitters. The VM does not instantiate `ParticleEmitter` subobjects
/// (their class chain is `Object`, not `Actor`, and `Vm::load_level` only creates Actors), so the
/// script's `Emitters[i].Disabled = ...` cannot be read back from the VM. Instead this observes the
/// VM trace for `Trigger` events delivered to a triggered-emitter actor and applies the same
/// effect the `TriggerEmit`/`TriggerToggle` state handlers would: toggle the matching host
/// simulator (a `TriggerControl` handler resets to the level-start state). Ordinary `Emitter`
/// actors are not toggled, matching their lack of a `Trigger` override.
fn sync_particle_triggers(
    mut session: NonSendMut<Result<session::Session, String>>,
    data: Res<viewer::particles::ParticleRenderData>,
    mut cursor: ResMut<ParticleTriggerCursor>,
    mut emitters: Query<&mut viewer::particles::ParticleEmitterRender>,
) {
    let Ok(sess) = session.as_mut() else {
        return;
    };
    let trace = &sess.vm().trace;
    if trace.len() < cursor.trace_len {
        cursor.trace_len = 0;
    }
    let start = cursor.trace_len.min(trace.len());
    let mut events: Vec<(String, String)> = Vec::new();
    for ev in &trace[start..] {
        if let xiii_script::TraceKind::Event {
            target, function, ..
        } = &ev.kind
            && function.to_ascii_lowercase().contains("trigger")
        {
            events.push((target.clone(), function.clone()));
        }
    }
    cursor.trace_len = trace.len();
    if events.is_empty() {
        return;
    }
    for mut e in &mut emitters {
        let Some(system) = data.systems.get(e.system) else {
            continue;
        };
        if !system.triggered {
            continue;
        }
        for (target, function) in &events {
            if !system.path.eq_ignore_ascii_case(target) {
                continue;
            }
            if function.to_ascii_lowercase().contains("triggercontrol") {
                e.sim.reset();
                if let Some(desc) = system.emitters.get(e.emitter) {
                    e.sim
                        .set_enabled(xiii_world::particles::initially_enabled(system, desc));
                }
            } else {
                e.sim.toggle();
            }
        }
    }
}

/// One VM status line: time, active/suspended counts, dispatcher state, player VM position,
/// event count and last player touch.
fn format_vm_trace(sess: &session::Session) -> String {
    format!(
        "vm t={:.3}s active={} suspended={} dispatcher={} player={} health={} physics={} events={} last_touch={}",
        sess.vm_time(),
        sess.active_actors(),
        sess.suspended.len(),
        sess.dispatcher_state().unwrap_or_else(|| "-".to_owned()),
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

fn overlay(
    cfg: Res<PlayConfig>,
    sim: Res<SimRes>,
    session: NonSend<Result<session::Session, String>>,
    pawns: Option<Res<pawns::PawnScene>>,
    hud: Option<Res<hud::HudRuntime>>,
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
    text.0 = format!(
        "XIII play prototype (NOT a playable mission; no weapons, no full AI)\n\
         map {} | pos ({:.1}, {:.1}, {:.1}) UU | vel ({:.1}, {:.1}, {:.1}) UU/s | state {}\n\
         floor normal ({:.2}, {:.2}, {:.2}) | last contact: {}\n\
         {}\n\
         {pawns_line}\n\
         {hud_line}\n\
         WASD move | mouse look | Space jump | Shift walk | C crouch | E use | Esc quit",
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
    mut perf: ResMut<crate::perf::Perf>,
    mut exit: MessageWriter<AppExit>,
) {
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
    /// VM session after the run.
    pub session: session::Session,
    /// Fixed ticks run.
    pub ticks: u64,
    /// Wall-clock seconds spent in the loop.
    pub wall_secs: f32,
    /// Trace samples: `(tick, seconds, position UU, velocity UU/s)`.
    pub trace: Vec<(u64, f32, [f32; 3], [f32; 3])>,
}

/// Opens a VM session and drives it with the movement simulation and an input script. No window
/// is opened; the fixed-step order is the one `fixed_step` uses.
///
/// The collision world (static soup + the VM's mover actors as dynamic objects) is built here so
/// the script can never diverge from the interactive path.
pub(crate) fn run_script(
    game_dir: &Path,
    map: &str,
    script: &script::Script,
    params: &PlayerParams,
    scene: &xiii_world::WorldScene,
    duration: f32,
) -> Result<ScriptOutcome, String> {
    let started = Instant::now();
    let mut session = session::Session::open(game_dir, map)?;
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
    let sources = &scene.collision_sources;
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
    // (the raw PlayerStart overlaps the floor) and the facing from the pawn's script-set
    // Rotation when the login path ran. The host writes the placed position back to the VM.
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
    let ticks = (duration / DT).ceil() as u64;
    let mut drive = script::Drive::new(script);
    let mut trace = Vec::new();
    for tick in 0..ticks {
        let elapsed = tick as f32 * DT;
        let input = drive.advance(elapsed, &mut sim);
        if volumes.is_empty() {
            sim.step(DT, &world, params, input, sources);
        } else {
            sim.step_with_modes(DT, &world, params, input, sources, &volumes);
        }
        let modes = session::PlayerVMModes {
            crouched: sim.crouched,
            in_water: sim.in_water,
            physics: sim.physics,
            landed_velocity_z: sim.landed.then_some(sim.land_velocity_z),
            floor_normal: sim.floor_normal,
        };
        session.step(DT, sim.location, sim.yaw, sim.velocity, &modes);
        let states = session.mover_states();
        mover_collision.update(&mut world, &states);
        if input.use_action {
            perform_use(&mut session, &world, sources, &sim, params);
        }
        if tick.is_multiple_of(TRACE_EVERY) || tick + 1 == ticks {
            trace.push((tick, elapsed, sim.location, sim.velocity));
            println!(
                "[play] {} | {} | {}",
                format_trace(tick, elapsed, &sim),
                format_vm_trace(&session),
                format_mover_trace(&session)
            );
        }
    }
    Ok(ScriptOutcome {
        session,
        ticks,
        wall_secs: started.elapsed().as_secs_f32(),
        trace,
    })
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
    let duration = opts
        .exit_after_secs
        .unwrap_or_else(|| script.last_time() + 2.0);
    let outcome = run_script(&game_dir, &map, &script, &params, &scene, duration)?;
    let session = &outcome.session;
    println!("[play] {}", session.bootstrap_note);
    match pawns::headless_report(session, &game_dir) {
        Ok(line) => println!("[play] {line}"),
        Err(e) => println!("[play] pawns headless report failed: {e}"),
    }
    println!(
        "[play] login path: script={} bootstrap={}",
        session.login_script, session.login_bootstrap
    );
    println!(
        "[play] localisation: language={} localized class-default overrides={}",
        session.localization_language, session.localized_overrides
    );
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
            "t=0.00 teleport -491.8 -414.1 1265.0\n\
             t=0.10 goto -491.84 -314.14\nt=0.30 jump\nt=0.80 jump\nt=1.30 jump\nt=1.80 jump\n\
             t=2.30 jump\nt=2.80 forward 0\n\
             t=3.20 teleport -742.1444 -808.429 1311.0449\n\
             t=3.20 yaw 312.891\nt=3.20 turn 2\nt=3.20 forward 1\n\
             t=4.80 turn -45\nt=5.50 forward 0\nt=5.80 use\nt=6.80 use\nt=7.00 forward 1\n\
             t=8.00 forward 0\n",
        )
        .unwrap();
        let outcome = run_script(&game_dir, "Plage01", &script, &resolved.params, &scene, 9.0)
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
        let vm = session.vm_mut();
        vm.set_property(canvas, "ClipX", 0, xiii_script::Value::Float(1280.0));
        vm.set_property(canvas, "ClipY", 0, xiii_script::Value::Float(720.0));
        vm.set_property(canvas, "Style", 0, xiii_script::Value::Byte(1));
        vm.set_property(
            canvas,
            "Font",
            0,
            xiii_script::Value::Name("Dummy".to_owned()),
        );
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

    /// A synthetic `CanvasFonts` provider: 4 units per character, 8 tall.
    struct DummyFonts;
    impl xiii_script::canvas::CanvasFonts for DummyFonts {
        fn measure(&self, font: &str, text: &str) -> Option<(f32, f32)> {
            (font.eq_ignore_ascii_case("Dummy")).then(|| (text.chars().count() as f32 * 4.0, 8.0))
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
        vm.set_property(canvas, "Font", 0, Value::Name("Dummy".to_owned()));
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
        // Host font bridge (as `hud::setup` does): assign the synthetic font to the HUD's own
        // font properties.
        for prop in ["SmallFont", "MedFont", "BigFont", "LargeFont"] {
            session
                .vm_mut()
                .set_property(hud, prop, 0, Value::Name("Dummy".to_owned()));
        }
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
