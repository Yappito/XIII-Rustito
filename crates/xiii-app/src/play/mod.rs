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

pub mod script;
pub mod session;
pub mod sim;

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use bevy::camera::visibility::RenderLayers;
use bevy::ecs::system::{NonSend, NonSendMut};
use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
use bevy::time::Fixed;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

use xiii_collision::CollisionWorld;
use xiii_decode::common::{UNREAL_UNITS_PER_METER, to_bevy_position};
use xiii_install::{Installation, OpenOptions};
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

#[derive(Resource)]
struct SimRes(PlayerSim);

#[derive(Resource)]
struct WorldRes {
    world: CollisionWorld,
    sources: Vec<String>,
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

impl Plugin for PlayPlugin {
    fn build(&self, app: &mut App) {
        // Load the script session before the window opens. `Session` holds `Rc`-based VM state
        // (it is `!Send`), so it lives in a non-send resource on the main thread; a load failure
        // is stored and reported by `setup`, which exits with an error.
        let game_dir = self.options.game_dir.clone().unwrap_or_default();
        let map = self.options.map.clone().unwrap_or_default();
        let session = session::Session::open(&game_dir, &map);
        app.insert_non_send(session);
        app.insert_resource(PlayConfig {
            options: self.options.clone(),
        })
        .insert_resource(ClearColor(Color::srgb(0.45, 0.62, 0.82)))
        .insert_resource(Time::<Fixed>::from_hz(FIXED_HZ))
        .init_resource::<ShotFlag>()
        .init_resource::<RenderSync>()
        .add_systems(Startup, setup)
        .add_systems(FixedUpdate, fixed_step)
        .add_systems(
            Update,
            (
                controls,
                grab_cursor,
                mouse_look,
                sync_camera,
                viewer::sky_follow,
                overlay,
                unattended,
            )
                .chain(),
        );
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
    let radius = req("CollisionRadius")?;
    let height = req("CollisionHeight")?;
    let eye = req("BaseEyeHeight")?;
    let ground = req("GroundSpeed")?;
    let jump = req("JumpZ")?;
    let accel = opt("AccelRate");
    let air = opt("AirControl");
    let walking = opt("WalkingPct");
    let maxfall = opt("MaxFallSpeed");
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
    lines.push(format!(
        "  fixed step {} Hz (hypothesis; UE2 used variable ticks); box half extents ({radius}, {height}, {radius}) UU",
        FIXED_HZ
    ));
    Ok(ResolvedParams {
        params: PlayerParams {
            radius_uu: radius,
            height_uu: height,
            base_eye_height_uu: eye,
            ground_speed: ground,
            jump_z: jump,
            accel_rate: accel,
            air_control: air,
            walking_pct: walking,
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
    session: NonSend<Result<session::Session, String>>,
    mut sync: ResMut<RenderSync>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut exit: MessageWriter<AppExit>,
) {
    let session = match &*session {
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
    ) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("[play] setup failed: {e}");
            exit.write(AppExit::error());
        }
    }
}

fn setup_inner(
    commands: &mut Commands,
    opts: &Options,
    session: &session::Session,
    sync: &mut RenderSync,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
) -> Result<(), String> {
    let started = Instant::now();
    let game_dir = opts
        .game_dir
        .clone()
        .ok_or("--game-dir is required for --play")?;
    let scene = viewer::load_scene(opts)?;
    let resolved = resolve_params(&game_dir)?;
    let params = resolved.params;
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
    for b in &session.blocked {
        println!("[play]   script path blocked: {b}");
    }

    let world = CollisionWorld::new(scene.box_collision());
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
    let yaw = rot[1] as f32 * std::f32::consts::TAU / 65536.0;
    let mut sim = PlayerSim::new(bevy_to_unreal_position(spawn.position), yaw);
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
    );
    for (o, entity) in scene.objects.iter().zip(&geometry) {
        let actor = o
            .path
            .split_once(" -> ")
            .map_or(o.path.as_str(), |(a, _)| a)
            .to_owned();
        sync.entities.entry(actor).or_default().push(*entity);
    }

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
        "[play] camera start {:?} m, yaw {:.1} deg | controls WASD, mouse look, Space jump, Shift walk, Esc quit",
        Vec3::from_array(eye),
        yaw.to_degrees()
    );
    commands.insert_resource(ParamsRes(params));
    commands.insert_resource(SimRes(sim));
    commands.insert_resource(WorldRes { world, sources });
    commands.insert_resource(ScriptRes { drive });
    commands.insert_resource(TraceState {
        tick: 0,
        start: Instant::now(),
        exit_secs,
        shot: 0,
        shot_done: false,
        target_at: None,
    });
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
    world: Res<WorldRes>,
    keys: Res<ButtonInput<KeyCode>>,
    mut script: ResMut<ScriptRes>,
    mut state: ResMut<TraceState>,
    mut session: NonSendMut<Result<session::Session, String>>,
    sync: Res<RenderSync>,
    mut transforms: Query<&mut Transform>,
) {
    let dt = time.delta_secs();
    if dt <= 0.0 {
        return;
    }
    let elapsed = state.tick as f32 * DT;
    let input = match script.drive.as_mut() {
        Some(drive) => drive.advance(elapsed, &mut sim.0),
        None => read_keyboard(&keys),
    };
    sim.0
        .step(dt, &world.world, &params.0, input, &world.sources);
    if let Ok(sess) = session.as_mut() {
        sess.step(dt, sim.0.location, sim.0.yaw, sim.0.velocity);
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
    }
    state.tick += 1;
    if state.tick.is_multiple_of(TRACE_EVERY) {
        println!("[play] {}", format_trace(state.tick, elapsed, &sim.0));
        if let Ok(sess) = session.as_ref() {
            println!("[play] {}", format_vm_trace(sess));
        }
    }
}

/// One VM status line: time, active/suspended counts, dispatcher state, player VM position,
/// event count and last player touch.
fn format_vm_trace(sess: &session::Session) -> String {
    format!(
        "vm t={:.3}s active={} suspended={} dispatcher={} player={} events={} last_touch={}",
        sess.vm_time(),
        sess.active_actors(),
        sess.suspended.len(),
        sess.dispatcher_state().unwrap_or_else(|| "-".to_owned()),
        sess.player_location()
            .map(|l| format!("({:.1},{:.1},{:.1})", l[0], l[1], l[2]))
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
    mut cams: Query<&mut Transform, With<PlayCam>>,
) {
    let eye = to_bevy_position(sim.0.eye_location(&params.0));
    for mut t in &mut cams {
        t.translation = Vec3::from_array(eye);
        t.rotation = Quat::from_euler(EulerRot::YXZ, -sim.0.yaw, sim.0.pitch, 0.0);
    }
}

fn overlay(
    cfg: Res<PlayConfig>,
    sim: Res<SimRes>,
    session: NonSend<Result<session::Session, String>>,
    mut text: Query<&mut Text, With<PlayOverlay>>,
) {
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
    text.0 = format!(
        "XIII play prototype (NOT a playable mission; no weapons, no full AI)\n\
         map {} | pos ({:.1}, {:.1}, {:.1}) UU | vel ({:.1}, {:.1}, {:.1}) UU/s | state {}\n\
         floor normal ({:.2}, {:.2}, {:.2}) | last contact: {}\n\
         {}\n\
         WASD move | mouse look | Space jump | Shift walk | Esc quit",
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
}

fn unattended(
    mut commands: Commands,
    cfg: Res<PlayConfig>,
    mut state: ResMut<TraceState>,
    flag: Res<ShotFlag>,
    sim: Res<SimRes>,
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
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_script(
    game_dir: &Path,
    map: &str,
    script: &script::Script,
    params: &PlayerParams,
    world: &CollisionWorld,
    sources: &[String],
    start_center: [f32; 3],
    start_yaw: f32,
    duration: f32,
) -> Result<ScriptOutcome, String> {
    let started = Instant::now();
    let mut session = session::Session::open(game_dir, map)?;
    let mut sim = PlayerSim::new(start_center, start_yaw);
    sim.grounded = true;
    let ticks = (duration / DT).ceil() as u64;
    let mut drive = script::Drive::new(script);
    let mut trace = Vec::new();
    for tick in 0..ticks {
        let elapsed = tick as f32 * DT;
        let input = drive.advance(elapsed, &mut sim);
        sim.step(DT, world, params, input, sources);
        session.step(DT, sim.location, sim.yaw, sim.velocity);
        if tick.is_multiple_of(TRACE_EVERY) || tick + 1 == ticks {
            trace.push((tick, elapsed, sim.location, sim.velocity));
            println!(
                "[play] {} | {}",
                format_trace(tick, elapsed, &sim),
                format_vm_trace(&session)
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
    let world = CollisionWorld::new(scene.box_collision());
    let sources = scene.collision_sources.clone();
    let (ps_bevy, rot) = scene.player_start.ok_or("map has no PlayerStart")?;
    let spawn = collision::place_spawn(&world, ps_bevy, params.half_extents_bevy())?;
    println!(
        "[play] spawn: PlayerStart {:?} UU -> box centre {:?} UU, raise {:.2} UU",
        bevy_to_unreal_position(ps_bevy),
        bevy_to_unreal_position(spawn.position),
        spawn.raise * UNREAL_UNITS_PER_METER
    );
    let duration = opts
        .exit_after_secs
        .unwrap_or_else(|| script.last_time() + 2.0);
    let outcome = run_script(
        &game_dir,
        &map,
        &script,
        &params,
        &world,
        &sources,
        bevy_to_unreal_position(spawn.position),
        rot[1] as f32 * std::f32::consts::TAU / 65536.0,
        duration,
    )?;
    let session = &outcome.session;
    println!("[play] {}", session.bootstrap_note);
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
        let world = CollisionWorld::new(scene.box_collision());
        let (ps, rot) = scene.player_start.expect("Plage00 has a PlayerStart");
        let spawn = collision::place_spawn(&world, ps, resolved.params.half_extents_bevy())
            .expect("spawn placement");
        // Walk from just short of TouchTrigger2 (3635.5, -44620.2, 4918) into its 130 UU cylinder.
        let script = script::Script::parse(
            "t=0.0 teleport 3380 -44620.2 4918\nt=0.0 yaw 0\nt=0.0 forward 1\nt=0.5 forward 0\n",
        )
        .unwrap();
        let outcome = run_script(
            &game_dir,
            "Plage00",
            &script,
            &resolved.params,
            &world,
            &scene.collision_sources,
            bevy_to_unreal_position(spawn.position),
            rot[1] as f32 * std::f32::consts::TAU / 65536.0,
            8.0,
        )
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
}
