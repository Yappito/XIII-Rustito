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

#[cfg(test)]
mod campaign_chain;
pub mod cartoon;
pub mod cinematics;
#[cfg(test)]
mod combat_survey;
pub mod cutscene;
pub mod footsteps;
pub mod hud;
/// item72: the retail in-game menu hosted in the play session's VM.
pub mod ingamemenu;
pub mod movement_modes;
pub mod movers;
pub mod pawns;
mod perception;
pub mod script;
pub mod session;
pub mod sim;
pub mod survey;
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

/// An edge-triggered live input the fixed tick consumes (a key or button going down).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum LiveEdge {
    /// `Fire` in DefUser.ini: LeftMouse, RControl.
    Fire,
    /// Jump (Space).
    Jump,
    /// Use / interact (E, Enter).
    Use,
}

/// The live edge table: every press the fixed tick reads. One entry per edge; adding an input
/// here routes it through [`latch_live_edges`] with no other change.
const LIVE_EDGES: [(LiveEdge, &[KeyCode], &[MouseButton]); 3] = [
    (
        LiveEdge::Fire,
        &[KeyCode::ControlRight],
        &[MouseButton::Left],
    ),
    (LiveEdge::Jump, &[KeyCode::Space], &[]),
    (LiveEdge::Use, &[KeyCode::KeyE, KeyCode::Enter], &[]),
];

/// A set of [`LiveEdge`]s: those pressed this frame, or latched and not yet consumed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct LiveEdges(u8);

impl LiveEdges {
    const fn bit(edge: LiveEdge) -> u8 {
        1 << edge as u8
    }

    fn contains(self, edge: LiveEdge) -> bool {
        self.0 & Self::bit(edge) != 0
    }

    /// The edges whose key or button went down this frame.
    fn pressed(keys: &ButtonInput<KeyCode>, buttons: &ButtonInput<MouseButton>) -> Self {
        let mut set = 0;
        for (edge, key_codes, mouse) in LIVE_EDGES {
            if keys.any_just_pressed(key_codes.iter().copied())
                || buttons.any_just_pressed(mouse.iter().copied())
            {
                set |= Self::bit(edge);
            }
        }
        Self(set)
    }

    /// Returns the latched edges and clears them: a latched press reaches one tick only.
    fn take(&mut self) -> Self {
        std::mem::take(self)
    }
}

/// item105/item105b: latches the live press edges once per render frame, before the fixed ticks
/// (`RunFixedMainLoopSystems::BeforeFixedMainLoop`), for exactly one fixed tick to consume
/// (`TraceState::edges`). `ButtonInput::just_pressed` is valid for one render frame, but the
/// fixed loop runs zero, one or several ticks in it: reading an edge inside `fixed_step` lost
/// the press when a frame ran no tick (a refresh rate above 60 Hz) and acted twice when it ran
/// two. For an automatic weapon the lost fire edge is worse than a lost click: with the key held
/// and nothing to start the shot, no `NormalFire` cycle begins, so nothing refires. Jump and use
/// had the same defect. Held (level) inputs are read directly and need no latch; the VM execs
/// (`live_execs`) are dispatched once per frame from `controls` and need none either.
fn latch_live_edges(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    mut state: ResMut<TraceState>,
) {
    state.edges.0 |= LiveEdges::pressed(&keys, &buttons).0;
}

/// A press made while the in-game menu page is open belongs to the page, not to the weapon.
fn live_menu_closed(menu: NonSend<ingamemenu::InGameMenu>) -> bool {
    !menu.is_open()
}

#[derive(Resource)]
struct WorldRes {
    world: CollisionWorld,
    sources: Vec<String>,
    movers: movers::MoverCollision,
    /// The mover the player stands on (a lift cage carries the player).
    rider: movers::Rider,
    /// A mover a script based the player pawn on (`Actor.SetBase`), carried like the rider.
    based_carry: movers::BasedCarry,
}

/// Render entities per map actor (the part of the scene object path before `" -> "`), for the
/// one-way sync of actors the VM moves.
#[derive(Resource, Default)]
struct RenderSync {
    entities: HashMap<String, Vec<Entity>>,
}

#[derive(Resource)]
pub(crate) struct ScriptRes {
    pub(crate) drive: Option<script::Drive>,
}

/// Player footstep cadence for `--play` (surface lookup table + accumulator). See
/// [`footsteps`] for the evidence (notify-driven in the original; synthesised here because the
/// player pawn has no third-person animation).
#[derive(Resource)]
struct FootstepRes(footsteps::FootstepDriver);

#[derive(Resource)]
pub(crate) struct TraceState {
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
    /// item72: the in-game menu page is open (retail pauses the level; the fixed step
    /// suppresses the player input while it is).
    menu_open: bool,
    /// item105/item105b: live press edges (fire, jump, use) latched by [`latch_live_edges`] and
    /// not yet consumed by a fixed tick.
    edges: LiveEdges,
}

#[derive(Resource, Default)]
struct ShotFlag(bool);

#[derive(Component)]
pub(crate) struct PlayCam;

#[derive(Component)]
struct PlayOverlay;

#[derive(Resource)]
pub(crate) struct DiagnosticOverlay(pub bool);

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
            // item89: the interactive session is not a trace consumer (`format_vm_trace` reads
            // session counters, not `vm.trace`), so recording every per-tick native/event into
            // the unbounded `Vec` is pure leak: it grew commit by ~10-37 MiB/s windowed. The
            // headless deterministic run keeps tracing (route tests assert on `map_traces`).
            let vm = s.vm_mut();
            vm.trace_enabled = false;
            vm.trace_natives = false;
        }
        app.insert_non_send(session);
        app.add_plugins((crate::video::CutscenePlugin, cutscene::CutsceneSystems));
        app.insert_non_send(cutscene::CutsceneHost(cutscene_host));
        app.insert_resource(DiagnosticOverlay(options.diagnostic_overlay));
        app.insert_non_send(ingamemenu::InGameMenu::default())
            .insert_resource(PlayConfig { options })
            .insert_resource(ClearColor(Color::srgb(0.45, 0.62, 0.82)))
            .insert_resource(Time::<Fixed>::from_hz(FIXED_HZ))
            .init_resource::<ShotFlag>()
            .init_resource::<RenderSync>()
            .init_resource::<weapons::WeaponView>()
            .init_resource::<RuntimeLights>()
            .add_systems(
                RunFixedMainLoop,
                latch_live_edges
                    .run_if(live_menu_closed)
                    .in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop),
            )
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
                    ingamemenu::frame,
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
                )
                    .chain(),
            )
            .add_systems(
                Update,
                (unattended, ingamemenu::scripted_shots).after(overlay),
            )
            // item87b: the windowed `LastRenderTime` report, after the pawn renderer has set
            // each instance's `Visibility`. Kept out of the big `Update` chain: that tuple is
            // already at Bevy's 20-system arity limit.
            .add_systems(Update, report_rendered_actors.after(pawns::update_pawns))
            .add_systems(
                Last,
                (
                    cinematics::report_exit,
                    cartoon::report_exit,
                    hud::report_exit,
                ),
            );
    }
}

/// Resolved player parameters plus the startup report lines.
pub(crate) struct ResolvedParams {
    pub params: PlayerParams,
    pub lines: Vec<String>,
}

/// Resolves the player parameters from the inherited class defaults of the pawn class named by
/// `Default.ini` -> GameInfo `DefaultPlayerClassName` (the same resolution `--collision-test`
/// uses), and gravity from the decoded `Engine.PhysicsVolume.Gravity` class default. Every
/// value is reported with its source; missing optional values are reported as such.
pub(crate) fn resolve_params(game_dir: &Path) -> Result<ResolvedParams, String> {
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
    mut menu: NonSendMut<ingamemenu::InGameMenu>,
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
    // item72: the in-game menu's save device writes into the configured save dir, and Quit
    // interception keys on the map.
    menu.configure(cfg.options.save_dir.clone(), cfg.options.map.clone());
    if let (Some(game), Some(config)) = (&cfg.options.game_dir, &cfg.options.config_dir)
        && let Err(error) = menu.install_settings(session, game, config)
    {
        eprintln!("[menu] settings installation failed: {error}");
        exit.write(AppExit::error());
        return;
    }
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
    // item87b: the windowed renderer owns `AActor.LastRenderTime` (it reports the actors it
    // actually drew each frame); the VM's headless stand-in is disabled for this session.
    session.use_renderer_render_times();
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
    let mover_parts = session.mover_states_including_pending_kill();
    let (mut world, mover_collision) = movers::MoverCollision::build(&scene, &mover_parts);
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
    if spawn.airborne {
        // Engine-faithful: `RestartPlayer` spawns the pawn at StartSpot.Location and
        // PHYS_Falling brings it down (USA01's PlayerStart is 14 m above the BSP floor).
        println!(
            "[play] spawn: no floor within the drop cap below the PlayerStart {:?} UU; \
             the pawn spawns at the start spot and falls (PHYS_Falling), raise {:.2} UU",
            bevy_to_unreal_position(ps_bevy),
            spawn.raise * UNREAL_UNITS_PER_METER
        );
    } else {
        println!(
            "[play] spawn: PlayerStart {:?} UU -> box centre {:?} UU, raise {:.2} UU, floor {:.2} UU below the centre",
            bevy_to_unreal_position(ps_bevy),
            bevy_to_unreal_position(spawn.position),
            spawn.raise * UNREAL_UNITS_PER_METER,
            (spawn.position[1] - spawn.floor) * UNREAL_UNITS_PER_METER
        );
    }
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
    sim.grounded = !spawn.airborne;

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
            (Some(script::Drive::new(&script)), Some(script.budget()))
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
        Visibility::Hidden,
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
        "[play] camera start {:?} m, yaw {:.1} deg | controls WASD/arrows, mouse look, Space jump, Shift walk, C crouch, E/Enter use, F3 diagnostics, Ctrl+Q quit",
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
            format!("; bone attachments: {}", pawn_scene.attachments.join(", "))
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
        rider: movers::Rider::default(),
        based_carry: movers::BasedCarry::default(),
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
            menu_open: false,
            edges: LiveEdges::default(),
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

/// Held (level) keyboard input. The press edges are latched per frame instead ([`LiveEdges`]).
fn read_keyboard(keys: &ButtonInput<KeyCode>) -> Input {
    let mut forward = 0.0;
    if keys.any_pressed([KeyCode::KeyW, KeyCode::ArrowUp]) {
        forward += 1.0;
    }
    if keys.any_pressed([KeyCode::KeyS, KeyCode::ArrowDown]) {
        forward -= 1.0;
    }
    let mut right = 0.0;
    if keys.any_pressed([KeyCode::KeyD, KeyCode::ArrowRight]) {
        right += 1.0;
    }
    if keys.any_pressed([KeyCode::KeyA, KeyCode::ArrowLeft]) {
        right -= 1.0;
    }
    Input {
        forward,
        right,
        // Edges (jump, use, fire) are not read here: see [`latch_live_edges`].
        jump: false,
        walk: keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]),
        use_action: false,
        fire: false,
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
    let hit = ray_target(world, sources, origin, dir, USE_REACH_M);
    let outcome = hit.as_ref().map(|target| sess.use_target(target));
    match (hit, outcome) {
        // A mover is used through its own lock/unlock/open chain; a dead pawn in front is
        // searched (the engine `Grab` interaction). `use_target` tries both, in that order.
        (Some(target), Some(outcome)) if outcome != session::UseOutcome::NotAMover => {
            println!("[play] use {target}: {outcome:?}");
        }
        // Nothing mover-like was hit: a card reader has no collision, so the game's own
        // interaction targeting (`MyInteraction.TargetActor`) decides what `Grab` acts on.
        // item64: with nothing targeted either, the use input still runs the game's own `Grab`
        // exec - its early branches are gameplay, not interaction: while hooked it is the rope
        // detach (`bHooked && !bWeaponMode && SelectedItem is Hook -> Hook.Release()`).
        (hit, _) => match sess.grab_interaction_target() {
            Some((target, outcome)) => println!("[play] use {target}: {outcome:?}"),
            None => match hit {
                Some(target) => println!("[play] use {target}: NotAMover"),
                None => {
                    println!("[play] use: nothing in reach (sending the bare Grab exec)");
                    sess.use_grab_exec();
                }
            },
        },
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
    // state. See `cinematics`. The in-game menu page (item72) pauses the level the same way:
    // retail input goes to the GUI while the page is open (`state.menu_open` is maintained by
    // the menu's per-frame system).
    let suppressed = match &*session {
        Ok(sess) => cinematics::input_suppressed(sess),
        Err(_) => false,
    } || state.menu_open;
    if let Some(drive) = script.drive.as_mut() {
        let track_height = drive.track_height();
        let tracked = drive.tracking_actor().and_then(|name| {
            (*session).as_ref().ok().and_then(|sess| {
                let id = sess.vm().find_live_object(name)?;
                // Head-zone aim as in the other drive site: for pawns, a fraction of
                // CollisionHeight above the tracked actor's Location keeps the ray inside the
                // aimed band (0.6 default: spine; 0.85 raises a level ray to the head band for
                // 3x bullet damage). Non pawns (map movers have oversized collision cylinders;
                // BreakAbleMover16's is 160 UU against a ~64 UU brush) are aimed at the raw
                // Location.
                let loc = sess.vm().vector_prop(id, "Location").map(|mut l| {
                    if sess.vm().is_a(id, "XIIIPawn")
                        && let Some(xiii_script::Value::Float(h)) =
                            sess.vm().get_property(id, "CollisionHeight")
                    {
                        l[2] += h * track_height;
                    }
                    l
                });
                Some((name.to_owned(), loc))
            })
        });
        if let Some((name, location)) = tracked {
            drive.set_track_location(Some(&name), location);
        } else {
            drive.set_track_location(None, None);
        }
        // item107: facts for the closed-loop blocking commands. The windowed VM keeps no trace,
        // so an `event` condition says so (`observe_trace`) instead of silently never holding.
        if drive.wants_facts()
            && let Ok(sess) = (*session).as_mut()
        {
            let facts =
                perception::gather(&drive.fact_request(), sess, &sim.0, &params.0, suppressed);
            drive.set_facts(facts);
            let vm = sess.vm();
            drive.observe_trace(&vm.trace, vm.trace_enabled);
        }
    }
    // item105b: consume the latched press edges every tick, also under a script or a
    // suppression, so a press never waits for a later tick.
    let edges = state.edges.take();
    // Always advance the script so an explicit `take_control` diagnostic can be read even while a cutscene
    // suppresses input; the axis input and the other command queues are dropped when suppressed.
    let (mut input, weapons, goals, use_named) = match script.drive.as_mut() {
        Some(drive) => {
            let input = drive.advance(elapsed, &mut sim.0);
            (
                input,
                drive.take_weapons(),
                drive.take_goals(),
                drive.take_use_named(),
            )
        }
        None => {
            let mut input = read_keyboard(&keys);
            // item105b: the press edges come from the per-frame latch, consumed exactly once.
            input.jump = edges.contains(LiveEdge::Jump);
            input.use_action = edges.contains(LiveEdge::Use);
            input.fire = edges.contains(LiveEdge::Fire);
            (input, Vec::new(), Vec::new(), Vec::new())
        }
    };
    let mut weapon_inputs = if let Some(drive) = script.drive.as_mut() {
        drive.take_weapon_inputs()
    } else {
        Vec::new() // Live execs are dispatched once per render/input frame in controls.
    };
    if suppressed {
        weapon_inputs.clear();
    }
    let control = script
        .drive
        .as_mut()
        .is_some_and(script::Drive::take_control);
    let heal = script
        .drive
        .as_mut()
        .is_some_and(script::Drive::take_quick_heal);
    let mut exec_inputs = script
        .drive
        .as_mut()
        .map(script::Drive::take_exec_inputs)
        .unwrap_or_default();
    if suppressed {
        exec_inputs.clear();
    }
    if suppressed {
        input = Input::default();
    }
    let (weapons, goals, use_named) = if suppressed {
        (Vec::new(), Vec::new(), Vec::new())
    } else {
        (weapons, goals, use_named)
    };
    let use_action = input.use_action;
    let fire = input.fire;
    // item105: a windowed `--play-script` run carries the same held buttons as the headless
    // route runner (`fire_hold`/`altfire_hold`); without this they were parsed and dropped. A
    // live (no script) run publishes the real key state from `controls` instead.
    let ((hold_fire, fire_edge), (hold_altfire, altfire_edge)) = script
        .drive
        .as_mut()
        .map(script::Drive::take_hook_buttons)
        .unwrap_or_default();
    let (fire_edge, altfire_edge) = (fire_edge && !suppressed, altfire_edge && !suppressed);
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
        let sim = &sim.0;
        let world = &mut *world;
        if let movers::BaseChange::Changed(base) =
            world
                .rider
                .observe(sim.grounded, sim.floor_source, &world.sources, |name| {
                    sess.vm().mover_state(name)
                })
        {
            sess.set_player_base(base.as_deref());
        }
    }
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
        // item94: mirror a script-set `Pawn.Base` mover carry for the host-owned pawn (see
        // `BasedCarry`); recorded before the VM step, applied after.
        let based_before = sess.player_base_mover();
        let floor_base = world.rider.base().map(str::to_owned);
        world.based_carry.observe(
            based_before.map(|(n, s)| (n, s.location)),
            floor_base.as_deref(),
        );
        let modes = session::PlayerVMModes {
            crouched: sim.0.crouched,
            in_water: sim.0.in_water,
            physics: sim.0.physics,
            landed_velocity_z: sim.0.landed.then_some(sim.0.land_velocity_z),
            floor_normal: sim.0.floor_normal,
            eye_height: sim.0.vm_eye_height(&params.0),
        };
        if script.drive.is_some() {
            sess.set_hook_buttons(hold_fire, hold_altfire);
        }
        sess.step(dt, sim.0.location, sim.0.yaw, sim.0.velocity, &modes);
        // Retail `APawn::SetZone`: publish the volume at the pawn's head (`Location +
        // CollisionHeight`) so `Pawn.BreathTimer` sees `HeadVolume.bWaterVolume` (item98e).
        let head_half = if sim.0.crouched {
            params.0.crouch_height_uu
        } else {
            params.0.height_uu
        };
        let head = [
            sim.0.location[0],
            sim.0.location[1],
            sim.0.location[2] + head_half,
        ];
        let head_volume = motion.0.volumes.physics_at(head).map(|v| v.name.clone());
        sess.publish_head_volume(head_volume.as_deref());
        sess.sync_view_rotation(sim.0.yaw, sim.0.pitch);
        world
            .rider
            .carry(&mut sim.0.location, |name| sess.vm().mover_state(name));
        world
            .based_carry
            .carry(&mut sim.0.location, |name| sess.vm().mover_state(name));
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
        sess.vm_mut().sync_mover_collision();
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
        for group in weapon_inputs {
            if let Err(e) = sess.weapon_input(group) {
                sess.blocked.push(format!("weapon input {group:?}: {e}"));
            }
        }
        if control {
            match sess.take_control() {
                Ok(state) => println!("[play] take_control diagnostic: controller -> {state}"),
                Err(e) => println!("[play] take_control failed: {e}"),
            }
        }
        for name in exec_inputs {
            if let Err(e) = sess.controller_input(name) {
                sess.blocked.push(format!("{name} input: {e}"));
            }
        }
        if heal && let Err(e) = sess.quick_heal() {
            println!("[play] quick_heal failed: {e}");
        }
        if use_action {
            perform_use(sess, &wr.world, &wr.sources, &sim.0, &params.0);
        }
        for target in &use_named {
            let outcome = sess.use_target(target);
            println!("[play] use {target}: {outcome:?}");
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
        if fire_edge {
            let outcome = sess.fire_pressed(sim.0.yaw, sim.0.pitch);
            println!("[play] fire_hold press: {outcome:?}");
        }
        if altfire_edge {
            let outcome = sess.altfire_pressed();
            println!("[play] altfire_hold press: {outcome:?}");
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

/// item87b: the windowed half of `AActor.LastRenderTime`. Retail writes
/// `LastRenderTime = Level.TimeSeconds` for every actor `FDynamicActor::Render` (Engine.dll
/// 0x103ccd10) draws; the Bevy renderer knows the drawn set through each entity's
/// `ViewVisibility` (frustum-culled). Runs after `pawns::update_pawns` and reports the visible
/// pawn and map-actor entities to the VM, so a cinematic `NotSeenByPlayer` wait only fires for
/// an actor the camera did not draw. The headless stand-in is [`Vm::update_render_times`].
fn report_rendered_actors(
    pawns: Option<Res<pawns::PawnScene>>,
    sync: Res<RenderSync>,
    vis: Query<&ViewVisibility>,
    mut session: NonSendMut<Result<session::Session, String>>,
) {
    let Ok(sess) = session.as_mut() else {
        return;
    };
    if !sess.renderer_reports_rendered() {
        return;
    }
    let mut drawn: Vec<xiii_script::ObjectId> = Vec::new();
    if let Some(pawns) = pawns {
        for inst in &pawns.instances {
            if vis
                .get(inst.root_entity())
                .map(|v| v.get())
                .unwrap_or(false)
            {
                drawn.push(inst.id);
            }
        }
    }
    for (name, entities) in &sync.entities {
        if entities
            .iter()
            .any(|e| vis.get(*e).map(|v| v.get()).unwrap_or(false))
            && let Some(id) = sess.vm().find_live_object(name)
        {
            drawn.push(id);
        }
    }
    sess.report_rendered(&drawn);
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

fn live_execs(
    keys: &ButtonInput<KeyCode>,
    buttons: &ButtonInput<MouseButton>,
    wheel: f32,
) -> Vec<(&'static str, Vec<Value>)> {
    let mut out = Vec::new();
    for (pressed, name) in [
        (
            buttons.just_pressed(MouseButton::Right) || keys.just_pressed(KeyCode::AltRight),
            "AltFire",
        ),
        (
            buttons.just_released(MouseButton::Left)
                || buttons.just_released(MouseButton::Right)
                || keys.any_just_released([KeyCode::ControlRight, KeyCode::AltRight]),
            "UnFire",
        ),
        (
            keys.any_just_pressed([KeyCode::KeyZ, KeyCode::PageDown]) || wheel < 0.0,
            "PrevWeapon",
        ),
        (
            keys.any_just_pressed([KeyCode::KeyX, KeyCode::PageUp]) || wheel > 0.0,
            "NextWeapon",
        ),
        (keys.just_pressed(KeyCode::KeyR), "Reload"),
        (
            keys.any_just_pressed([KeyCode::KeyQ, KeyCode::Delete])
                && !keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight]),
            "QuickHeal",
        ),
        (
            keys.any_just_pressed([KeyCode::KeyF, KeyCode::Numpad0]),
            "cPrevItem",
        ),
        (
            keys.any_just_pressed([KeyCode::KeyG, KeyCode::Numpad1]),
            "cNextItem",
        ),
        (keys.just_pressed(KeyCode::F1), "ShowScores"),
        (keys.just_released(KeyCode::F1), "HideScores"),
    ] {
        if pressed {
            out.push((
                name,
                if matches!(name, "Fire" | "AltFire") {
                    vec![Value::Float(1.0)]
                } else {
                    Vec::new()
                },
            ));
        }
    }
    for (key, group) in [
        KeyCode::Digit0,
        KeyCode::Digit1,
        KeyCode::Digit2,
        KeyCode::Digit3,
        KeyCode::Digit4,
        KeyCode::Digit5,
        KeyCode::Digit6,
        KeyCode::Digit7,
        KeyCode::Digit8,
        KeyCode::Digit9,
    ]
    .into_iter()
    .zip([0, 1, 2, 3, 4, 6, 9, 11, 14, 15])
    {
        if keys.just_pressed(key) {
            out.push(("SelectWeapon", vec![Value::Int(group)]));
        }
    }
    out
}

#[cfg(test)]
mod live_input_tests {
    use super::*;

    fn latch_state() -> TraceState {
        TraceState {
            tick: 0,
            saved_total: 0,
            start: Instant::now(),
            exit_secs: None,
            shot: 0,
            shot_done: false,
            target_at: None,
            post_travel_at: None,
            video_exit_at: None,
            menu_open: false,
            edges: LiveEdges::default(),
        }
    }

    /// Per fixed tick: the edges that tick saw.
    #[derive(Resource, Default)]
    struct EdgesSeen(Vec<LiveEdges>);

    /// How a test fixed tick reads the press edges.
    #[derive(Clone, Copy, PartialEq)]
    enum Reader {
        /// The shipped path: [`latch_live_edges`] before the fixed loop, `take` in the tick.
        Latch,
        /// The pre-item105 path: `just_pressed` read inside the fixed tick.
        DirectInTick,
    }

    /// One physical key or button driving an edge.
    #[derive(Clone, Copy, Debug)]
    enum Source {
        Key(KeyCode),
        Mouse(MouseButton),
    }

    /// Every physical source of every edge in the table, as `(edge, source)`.
    fn all_sources() -> Vec<(LiveEdge, Source)> {
        let mut out = Vec::new();
        for (edge, keys, mouse) in LIVE_EDGES {
            out.extend(keys.iter().map(|k| (edge, Source::Key(*k))));
            out.extend(mouse.iter().map(|m| (edge, Source::Mouse(*m))));
        }
        out
    }

    /// Presses `drive` at `press_frame`, releases it three frames later and presses it again ten
    /// frames after the first press; 40 frames at `frame_hz`. Returns what each fixed tick saw
    /// and, per press, the index of the first tick of the press frame (if that frame ran one).
    fn run_frames(
        frame_hz: f64,
        press_frame: usize,
        drive: &[(LiveEdge, Source)],
        reader: Reader,
    ) -> (Vec<LiveEdges>, Vec<Option<usize>>) {
        use bevy::time::{TimePlugin, TimeUpdateStrategy};
        let mut app = App::new();
        app.add_plugins(TimePlugin)
            .insert_resource(Time::<Fixed>::from_hz(FIXED_HZ))
            .init_resource::<ButtonInput<KeyCode>>()
            .init_resource::<ButtonInput<MouseButton>>()
            .insert_resource(latch_state())
            .init_resource::<EdgesSeen>();
        match reader {
            Reader::Latch => {
                app.add_systems(
                    RunFixedMainLoop,
                    latch_live_edges.in_set(RunFixedMainLoopSystems::BeforeFixedMainLoop),
                )
                .add_systems(
                    FixedUpdate,
                    |mut state: ResMut<TraceState>, mut seen: ResMut<EdgesSeen>| {
                        seen.0.push(state.edges.take());
                    },
                );
            }
            Reader::DirectInTick => {
                app.add_systems(
                    FixedUpdate,
                    |keys: Res<ButtonInput<KeyCode>>,
                     buttons: Res<ButtonInput<MouseButton>>,
                     mut seen: ResMut<EdgesSeen>| {
                        seen.0.push(LiveEdges::pressed(&keys, &buttons));
                    },
                );
            }
        }
        app.insert_resource(TimeUpdateStrategy::ManualDuration(Duration::from_secs_f64(
            1.0 / frame_hz,
        )));
        app.update(); // primes the clock: no time has elapsed yet
        let mut first_tick = Vec::new();
        for frame in 0..40 {
            // The engine's input system clears the per-frame edges, then applies the frame's
            // events, before the fixed loop runs.
            let down = frame == press_frame || frame == press_frame + 10;
            let up = frame == press_frame + 3 || frame == press_frame + 13;
            {
                let w = app.world_mut();
                w.resource_mut::<ButtonInput<KeyCode>>().clear();
                w.resource_mut::<ButtonInput<MouseButton>>().clear();
                for (_, source) in drive {
                    match (*source, down, up) {
                        (Source::Key(k), true, _) => {
                            w.resource_mut::<ButtonInput<KeyCode>>().press(k)
                        }
                        (Source::Key(k), _, true) => {
                            w.resource_mut::<ButtonInput<KeyCode>>().release(k);
                        }
                        (Source::Mouse(m), true, _) => {
                            w.resource_mut::<ButtonInput<MouseButton>>().press(m);
                        }
                        (Source::Mouse(m), _, true) => {
                            w.resource_mut::<ButtonInput<MouseButton>>().release(m);
                        }
                        _ => {}
                    }
                }
            }
            let before = app.world().resource::<EdgesSeen>().0.len();
            app.update();
            if down {
                let ran = app.world().resource::<EdgesSeen>().0.len() > before;
                first_tick.push(ran.then_some(before));
            }
        }
        (app.world().resource::<EdgesSeen>().0.clone(), first_tick)
    }

    /// item105/item105b: one render frame can run zero, one or several fixed ticks, and
    /// `ButtonInput::just_pressed` lives for exactly that frame. A press of fire, jump or use must
    /// reach exactly one fixed tick whatever the frame/tick ratio (two presses, two ticks):
    /// reading `just_pressed` inside the fixed tick lost it on a frame without a tick and acted
    /// twice on a frame with two. Every physical key and button of the edge table alone, and all
    /// edges pressed together, so a shared flag cannot merge or drop one.
    #[test]
    fn live_fire_press_reaches_exactly_one_fixed_tick() {
        let mut cases: Vec<Vec<(LiveEdge, Source)>> =
            all_sources().into_iter().map(|s| vec![s]).collect();
        cases.push(
            LIVE_EDGES
                .iter()
                .map(|(edge, keys, mouse)| {
                    (
                        *edge,
                        keys.first()
                            .map_or_else(|| Source::Mouse(mouse[0]), |k| Source::Key(*k)),
                    )
                })
                .collect(),
        );
        for drive in &cases {
            for frame_hz in [240.0f64, 144.0, 60.0, 45.0, 30.0, 20.0] {
                for press_frame in 0..6 {
                    let (seen, first) = run_frames(frame_hz, press_frame, drive, Reader::Latch);
                    for (edge, _) in drive {
                        let presses = seen.iter().filter(|e| e.contains(*edge)).count();
                        assert_eq!(
                            presses,
                            2,
                            "{drive:?} at {frame_hz} Hz, press frame {press_frame}: two presses of {edge:?} must reach two ticks ({} ticks ran)",
                            seen.len()
                        );
                    }
                    for tick in first.into_iter().flatten() {
                        for (edge, _) in drive {
                            assert!(
                                seen[tick].contains(*edge),
                                "{drive:?} at {frame_hz} Hz, press frame {press_frame}: the frame's first tick must see the {edge:?} press"
                            );
                        }
                    }
                    // No edge appears that was not driven.
                    for (edge, _, _) in LIVE_EDGES {
                        if !drive.iter().any(|(e, _)| *e == edge) {
                            assert!(
                                seen.iter().all(|e| !e.contains(edge)),
                                "{drive:?}: {edge:?} appeared without being pressed"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The same harness with the pre-fix read (`just_pressed` inside the fixed tick) must fail the
    /// exactly-once check: it loses presses at 240 Hz and doubles them at 20 Hz, for every edge.
    /// This keeps the test above honest: it distinguishes the old code from the new.
    #[test]
    fn direct_read_in_the_fixed_tick_loses_and_doubles_presses() {
        for (edge, source) in all_sources() {
            let count = |hz: f64, press_frame: usize| {
                let (seen, _) =
                    run_frames(hz, press_frame, &[(edge, source)], Reader::DirectInTick);
                seen.iter().filter(|e| e.contains(edge)).count()
            };
            let lost = (0..6).any(|f| count(240.0, f) < 2);
            let doubled = (0..6).any(|f| count(20.0, f) > 2);
            assert!(lost, "{edge:?}/{source:?}: a press must be lost at 240 Hz");
            assert!(doubled, "{edge:?}/{source:?}: a press must double at 20 Hz");
        }
    }

    /// A latched edge is consumed by one take and never replays; edges are independent bits.
    #[test]
    fn taken_edges_do_not_replay() {
        let mut edges = LiveEdges::default();
        edges.0 |= LiveEdges::bit(LiveEdge::Jump) | LiveEdges::bit(LiveEdge::Use);
        let first = edges.take();
        assert!(first.contains(LiveEdge::Jump) && first.contains(LiveEdge::Use));
        assert!(!first.contains(LiveEdge::Fire));
        assert_eq!(
            edges.take(),
            LiveEdges::default(),
            "a take clears the latch"
        );
    }

    /// A key held across frames is one edge, not one per frame (level is not an edge).
    #[test]
    fn held_key_latches_one_edge() {
        let mut keys = ButtonInput::<KeyCode>::default();
        let buttons = ButtonInput::<MouseButton>::default();
        keys.press(KeyCode::Space);
        assert!(LiveEdges::pressed(&keys, &buttons).contains(LiveEdge::Jump));
        keys.clear(); // next frame, still held
        assert_eq!(LiveEdges::pressed(&keys, &buttons), LiveEdges::default());
        assert!(keys.pressed(KeyCode::Space));
    }

    #[test]
    fn live_input_edges_release_and_retail_groups() {
        let mut keys = ButtonInput::default();
        let mut buttons = ButtonInput::default();
        assert!(live_execs(&keys, &buttons, 0.0).is_empty());
        keys.press(KeyCode::Digit5);
        keys.press(KeyCode::KeyZ);
        keys.press(KeyCode::KeyQ);
        buttons.press(MouseButton::Right);
        let execs = live_execs(&keys, &buttons, 0.0);
        assert!(execs.contains(&("SelectWeapon", vec![Value::Int(6)])));
        assert!(execs.contains(&("PrevWeapon", Vec::new())));
        assert!(execs.contains(&("QuickHeal", Vec::new())));
        assert!(execs.contains(&("AltFire", vec![Value::Float(1.0)])));
        keys.clear();
        buttons.clear();
        assert!(
            live_execs(&keys, &buttons, 0.0).is_empty(),
            "holding must not replay exec edges"
        );
        buttons.release(MouseButton::Right);
        assert!(live_execs(&keys, &buttons, 0.0).contains(&("UnFire", Vec::new())));
        keys.press(KeyCode::ControlLeft);
        keys.release(KeyCode::KeyQ);
        keys.press(KeyCode::KeyQ);
        assert!(
            !live_execs(&keys, &buttons, 0.0)
                .iter()
                .any(|(n, _)| *n == "QuickHeal"),
            "quit chord must not consume a medkit"
        );
        assert!(live_execs(&keys, &buttons, -1.0).contains(&("PrevWeapon", Vec::new())));
        assert!(live_execs(&keys, &buttons, 1.0).contains(&("NextWeapon", Vec::new())));
        keys.press(KeyCode::ArrowUp);
        keys.press(KeyCode::ArrowDown);
        assert_eq!(
            read_keyboard(&keys).forward,
            0.0,
            "opposing directions cancel"
        );
    }
}

#[allow(clippy::too_many_arguments)]
fn controls(
    keys: Res<ButtonInput<KeyCode>>,
    buttons: Res<ButtonInput<MouseButton>>,
    wheel: Res<bevy::input::mouse::AccumulatedMouseScroll>,
    cfg: Res<PlayConfig>,
    mut session: NonSendMut<Result<session::Session, String>>,
    mut menu: NonSendMut<ingamemenu::InGameMenu>,
    mut menu_was_open: Local<bool>,
    mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>,
    window: Query<&Window, With<PrimaryWindow>>,
    mut diagnostics: ResMut<DiagnosticOverlay>,
    mut exit: MessageWriter<AppExit>,
) {
    if keys.just_pressed(KeyCode::F3) {
        diagnostics.0 = !diagnostics.0;
    }
    // Ctrl+Q stays the explicit hard quit, also while the menu is open.
    if keys.just_pressed(KeyCode::KeyQ)
        && keys.any_pressed([KeyCode::ControlLeft, KeyCode::ControlRight])
    {
        exit.write(AppExit::Success);
    }
    if let Ok(sess) = session.as_mut() {
        // Escape is the retail Showmenu binding: it opens the in-game page through the game's
        // own ShowTheMenu path; while the page is open the page's own Esc handler closes it.
        if keys.just_pressed(KeyCode::Escape) {
            if menu.is_open() {
                menu.request_close(sess);
            } else {
                menu.open(sess);
            }
        }
        if menu.is_open() {
            // Route the menu keys to the page (the retail GUIController dispatch). Movement,
            // weapons and the held fire flags stay suppressed below.
            for (key, code) in [
                (KeyCode::ArrowUp, 38u8),
                (KeyCode::ArrowDown, 40),
                (KeyCode::ArrowLeft, 37),
                (KeyCode::ArrowRight, 39),
                (KeyCode::Enter, 13),
                (KeyCode::Backspace, 8),
            ] {
                if keys.just_pressed(key)
                    && let Err(e) = menu.key(sess, code)
                {
                    menu.errors.push(format!("menu key {key:?}: {e}"));
                }
            }
            if buttons.just_pressed(MouseButton::Left) {
                let at = window
                    .single()
                    .ok()
                    .and_then(|w| w.cursor_position())
                    .map(|p| (p.x, p.y));
                menu.click(sess, at);
            }
        }
    }
    // Mouse capture follows the menu state: the page is mouse-driven, so the cursor is
    // released while it is open and re-grabbed when it closes.
    let open = menu.is_open();
    if open != *menu_was_open {
        if open {
            cursor.grab_mode = CursorGrabMode::None;
            cursor.visible = true;
        } else {
            cursor.grab_mode = CursorGrabMode::Locked;
            cursor.visible = false;
        }
        *menu_was_open = open;
    }
    if cfg.options.play_script.is_none()
        && !menu.is_open()
        && let Ok(sess) = session.as_mut()
        && let Some(pc) = sess.controller
    {
        let fire_down = buttons.pressed(MouseButton::Left) || keys.pressed(KeyCode::ControlRight);
        let altfire_down = buttons.pressed(MouseButton::Right) || keys.pressed(KeyCode::AltRight);
        sess.vm_mut()
            .set_property(pc, "bFire", 0, Value::Byte(u8::from(fire_down)));
        sess.vm_mut()
            .set_property(pc, "bAltFire", 0, Value::Byte(u8::from(altfire_down)));
        // item105: the fixed step's `Session::step` republishes the held buttons every tick
        // (the hook owns them while selected/attached, a held weapon fire otherwise). Feed it the
        // live key state too: while the hook owned the buttons it published the scripted
        // default (nothing held) over the byte written above on the next tick, so a held key
        // never reached the rope's `PressingFire` polls in windowed play.
        sess.set_hook_buttons(fire_down, altfire_down);
        // State-aware dispatch preserves retail NoControl/NoMove exec overrides.
        for (name, args) in live_execs(&keys, &buttons, wheel.delta.y) {
            if let Err(e) = sess.vm_mut().send_event(pc, name, args) {
                eprintln!("[play] live input {name} failed: {e}");
            }
        }
        sess.drain_events();
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
    menu: NonSend<ingamemenu::InGameMenu>,
    mut sim: ResMut<SimRes>,
) {
    // A menu click is the page's input, not a re-grab request.
    if buttons.just_pressed(MouseButton::Left)
        && cursor.grab_mode != CursorGrabMode::Locked
        && !menu.is_open()
    {
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
    mut session: NonSendMut<Result<session::Session, String>>,
    mut cams: Query<&mut Transform, With<PlayCam>>,
) {
    let death_camera = match session.as_mut() {
        Ok(sess) => {
            let controller = sess.controller;
            controller.and_then(|controller| {
                let state = sess.vm().state_name(controller);
                matches!(
                    state.as_deref(),
                    Some("GameEndedDeath" | "GameEndedDrown" | "GameEndedFalling")
                )
                .then(|| {
                    // The engine calls the controller's state-scoped PlayerCalcView every
                    // rendered frame. Drive that same VM event here so its script-owned camera
                    // location/rotation, pitch easing and roll effect remain authoritative.
                    let zero_vector = Value::Struct(vec![
                        ("X".into(), Value::Float(0.0)),
                        ("Y".into(), Value::Float(0.0)),
                        ("Z".into(), Value::Float(0.0)),
                    ]);
                    let zero_rotator = Value::Struct(vec![
                        ("Pitch".into(), Value::Int(0)),
                        ("Yaw".into(), Value::Int(0)),
                        ("Roll".into(), Value::Int(0)),
                    ]);
                    if let Err(e) = sess.vm_mut().send_event(
                        controller,
                        "PlayerCalcView",
                        vec![Value::Object(None), zero_vector, zero_rotator],
                    ) {
                        eprintln!("[play] controller PlayerCalcView failed: {e}");
                        return None;
                    }
                    let vm = sess.vm();
                    let location = vm.vector_prop(controller, "vGameEndedCamLoc")?;
                    let rotation = vm.rotation_prop(controller)?;
                    Some(cinematics::camera_transform(location, rotation))
                })
                .flatten()
            })
        }
        Err(_) => None,
    };
    for mut t in &mut cams {
        if let Some(v) = &cine.view {
            // A script selected a cutscene camera (`CamView`/`ViewTarget`); render from it.
            let (loc, rot) = cinematics::camera_transform(v.location, v.rotation);
            t.translation = loc;
            t.rotation = rot;
        } else if let Some((loc, rot)) = death_camera {
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
    diagnostics: Res<DiagnosticOverlay>,
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
    mut text: Query<(&mut Text, &mut Visibility), With<PlayOverlay>>,
) {
    let t0 = Instant::now();
    let Ok((mut text, mut visibility)) = text.single_mut() else {
        return;
    };
    *visibility = if diagnostics.0 {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };
    if !diagnostics.0 {
        return;
    }
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
        "map {} | pos ({:.1}, {:.1}, {:.1}) UU | vel ({:.1}, {:.1}, {:.1}) UU/s | state {}\n\
         floor normal ({:.2}, {:.2}, {:.2}) | last contact: {}\n\
         {combat_line}\n\
         {}\n\
         {pawns_line}\n\
         {lights_line}\n\
         {hud_line}\n\
         {projectors_line}\n\
          F3 diagnostics | Ctrl+Q quit",
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
    if cfg.options.play_script.is_none()
        && cfg.options.screenshot.is_some()
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
    // Script timestamps use fixed simulation time. A slow startup or capped catch-up must not
    // end a scripted run before its requested inputs have been delivered.
    let elapsed = unattended_elapsed(
        cfg.options.play_script.is_some(),
        state.tick,
        state.start.elapsed().as_secs_f32(),
    );
    if flag.0 {
        state.shot_done = true;
    }
    if let Some(path) = &cfg.options.screenshot
        && state.shot == 0
        && elapsed >= screenshot_at(cfg.options.play_script.is_some(), secs)
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

fn unattended_elapsed(scripted: bool, tick: u64, wall_secs: f32) -> f32 {
    if scripted {
        tick as f32 * DT
    } else {
        wall_secs
    }
}

fn screenshot_at(scripted: bool, exit_secs: f32) -> f32 {
    if scripted {
        exit_secs
    } else {
        exit_secs * 0.75
    }
}

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
    mut menu: NonSendMut<ingamemenu::InGameMenu>,
    mut cinematic: ResMut<cinematics::CinematicState>,
    mut cartoon: ResMut<cartoon::CartoonState>,
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
    // item72: the in-game menu's `QuitGame` travels to the front-end map; the host hands off to
    // the `--menu` mode instead of loading it as a play level (one event loop per process).
    match travel::TravelPlan::from_request(&req) {
        Ok(plan) if ingamemenu::InGameMenu::is_front_end_map(&plan.map) => {
            println!(
                "[play] travel to the front-end map {} requested by {} at t={:.3}s; the host relaunches --menu",
                plan.map, req.actor, req.time
            );
            ingamemenu::request_relaunch(ingamemenu::MenuRelaunch::FrontEnd);
            exit.write(AppExit::Success);
            return;
        }
        _ => {}
    }
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
    println!("[play] outgoing objectives: {}", sess.objective_summary());
    println!(
        "[play] outgoing suspended={:?} blocked={:?}",
        sess.suspended, sess.blocked
    );
    for (actor, error) in &sess.failures {
        println!("[play] outgoing failure {actor}: {error}");
    }
    println!(
        "[play] outgoing presentation: dialogue_lines={} view_changes={} cartoon_updates={}",
        cinematic.lines, cinematic.view_changes, cartoon.render_updates
    );
    if let Some(host) = cutscene_host.0.as_ref() {
        println!("{}", host.report_line());
        for diagnostic in host.diagnostics() {
            println!("[video host]   {diagnostic}");
        }
    }
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
    // These caches contain entity IDs from the outgoing scene. They must not issue commands
    // against despawned entities or mistake a reused VM object ID for an existing light.
    commands.insert_resource(RuntimeLights::default());
    commands.insert_resource(viewer::decals::RuntimeProjectorDecals::default());
    commands.insert_resource(weapons::WeaponView::default());
    commands.insert_resource(cartoon::CartoonRenderTarget::default());
    commands.insert_resource(crate::video::VideoOverlay::default());
    *cinematic = cinematics::CinematicState::default();
    *cartoon = cartoon::CartoonState::default();
    let new_session = match travel::open_next_session(sess, &game_dir, &plan) {
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
    // item89: keep the interactive (windowed) session's trace off across in-process travel,
    // for the same reason as in `PlayPlugin::build`.
    s.vm_mut().trace_enabled = false;
    s.vm_mut().trace_natives = false;
    // item72: the in-game menu's VM references died with the outgoing session; re-configure for
    // the new map (the next `open` reinstalls the GUI graph into the new VM).
    menu.configure(cfg.options.save_dir.clone(), cfg.options.map.clone());
    if let Some(config) = &cfg.options.config_dir
        && let Err(error) = menu.install_settings(s, Path::new(&game_dir), config)
    {
        eprintln!("[menu] travel settings installation failed: {error}");
        exit.write(AppExit::error());
        return;
    }
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
    // The scripted run drives the VM; run it on the explicit VM host stack (see vmstack) so
    // the engine-limit recursion guard fires before the thread runs out of stack.
    let owned = opts.clone();
    crate::vmstack::run_on_vm_stack(move || match run_headless_inner(&owned) {
        Ok(()) => AppExit::Success,
        Err(e) => {
            eprintln!("error: {e}");
            AppExit::error()
        }
    })
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
    /// item97b: script time (seconds since the player gained control) at which the run first
    /// relinquished player input (death/game-end state or an authored cinematic). `None` when the
    /// player kept control for the whole run. The combat survey uses it to count only the
    /// commanded shots issued while the player still had control.
    #[allow(dead_code)] // test-evidence field (combat survey)
    pub control_ended: Option<f32>,
    /// item97b: the same boundary in VM time, so trace events (which carry VM time) can be
    /// classified as before/after the player lost control.
    #[allow(dead_code)] // test-evidence field (combat survey)
    pub control_ended_vm: Option<f64>,
    /// Trace samples: `(tick, seconds, position UU, velocity UU/s)`.
    pub trace: Vec<(u64, f32, [f32; 3], [f32; 3])>,
    /// Map stem active at the end of the run.
    pub final_map: String,
    /// One entry per level transition, in order (empty when the run did not travel).
    pub travel: Vec<TravelHop>,
    /// Each map the run passed through and its VM event trace as the run left it (the final
    /// map's trace is the last entry). The session's VM is rebuilt per map, so this is the only
    /// place a travelled-from map's scripted events remain observable.
    #[allow(dead_code)] // test-evidence field (opt-in route tests)
    pub map_traces: Vec<(String, Vec<xiii_script::TraceEvent>)>,
    /// item18: each map the run passed through and its `MapInfo.Objectif[]` states as the run left
    /// it (the final map's states are the last entry). Requirement 4's "objective states over
    /// time" across a multi-map run.
    pub map_objectives: Vec<(String, Vec<session::ObjectiveState>)>,
    /// item40m: each map the run passed through and the minimum player `Health` observed on it
    /// (the final map's value is the last entry). The route tests pin the lowest value so a
    /// change that lets the live AI bleed the player more fails visibly.
    #[allow(dead_code)] // test-evidence field (opt-in route tests)
    pub map_min_health: Vec<(String, f32)>,
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
    /// The mover the player stands on (a lift cage carries the player).
    rider: movers::Rider,
    /// A mover a script based the player pawn on (`Actor.SetBase`), carried like the rider.
    based_carry: movers::BasedCarry,
    volumes: movement_modes::VolumeMotion,
    sim: PlayerSim,
    player_params: PlayerParams,
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
    build_map_runtime(
        game_dir,
        map,
        scene,
        params,
        session::Session::open(game_dir, map)?,
    )
}

fn build_map_runtime(
    game_dir: &Path,
    map: &str,
    scene: &xiii_world::WorldScene,
    params: &PlayerParams,
    mut session: session::Session,
) -> Result<MapRuntime, String> {
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
        .set_voice_duration(Box::new(voice_provider.clone()));
    // item48: `Actor.WaveHasPosition` needs the audio-subsystem seam; without a provider the
    // native fails explicitly and a speaking `DialogueManager` would be suspended. The provider
    // cannot classify the positional bit yet (see `play::voice`), which keeps the native's
    // visible note path.
    session.vm_mut().set_wave_position(Box::new(voice_provider));
    session.register_movers(scene);
    // Retail `AActor::IsOverlapping` tests a volume brush with its `Brush` UModel primitive, not
    // its collision cylinder (Engine.dll @0x10352bd0; see `xiii_script::brush`). Decode each
    // placed volume brush's BSP to world space and register it on the VM actor so the touch
    // refresh fires `DetectionVolume`/`PhysicsVolume`/`Trigger` overlaps on the real geometry.
    match xiii_world::volume_brushes::import(game_dir, map) {
        Ok(vb) => {
            let total = vb.brushes.len();
            let mut registered = 0usize;
            for b in vb.brushes {
                if let Some(id) = session.vm_mut().find_live_object(&b.name) {
                    session.vm_mut().set_brush_primitive(id, b.primitive);
                    registered += 1;
                } else {
                    println!(
                        "[play]   volume brush '{}' has no live VM actor; cylinder fallback",
                        b.name
                    );
                }
            }
            println!("[play] volume brushes: {registered}/{total} registered");
            for d in &vb.diagnostics {
                println!("[play]   volume-brush diagnostic: {d}");
            }
        }
        Err(e) => {
            println!("[play] volume brushes unavailable ({e}); volume overlaps use cylinders")
        }
    }
    let mover_states = session.mover_states();
    let mover_parts = session.mover_states_including_pending_kill();
    let (mut world, mover_collision) = movers::MoverCollision::build(scene, &mover_parts);
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
    if spawn.airborne {
        println!(
            "[play] spawn: no floor within the drop cap below the PlayerStart {:?} UU; \
             the pawn spawns at the start spot and falls (PHYS_Falling), raise {:.2} UU",
            bevy_to_unreal_position(ps_bevy),
            spawn.raise * UNREAL_UNITS_PER_METER
        );
    } else {
        println!(
            "[play] spawn: PlayerStart {:?} UU -> box centre {:?} UU, raise {:.2} UU",
            bevy_to_unreal_position(ps_bevy),
            bevy_to_unreal_position(spawn.position),
            spawn.raise * UNREAL_UNITS_PER_METER
        );
    }
    // The script login chain (item3h) created the pawn at the PlayerStart; the host owns its
    // movement fields (item8a rule), so the position comes from the host's FindSpot placement
    // and the facing from the pawn's script-set Rotation when the login path ran.
    let start_yaw = match (session.login_script, session.player_rotation()) {
        (1, Some(r)) => r[1] as f32 * std::f32::consts::TAU / 65536.0,
        _ => rot[1] as f32 * std::f32::consts::TAU / 65536.0,
    };
    let mut sim = PlayerSim::new(bevy_to_unreal_position(spawn.position), start_yaw);
    sim.grounded = !spawn.airborne;
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
        rider: movers::Rider::default(),
        based_carry: movers::BasedCarry::default(),
        volumes,
        sim,
        player_params: *params,
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
/// level travel, the next map is imported through the shared native travel bridge, reusing
/// Login's pawn for imported travel properties; the run continues until the duration is spent.
#[cfg(test)]
pub(crate) fn run_script(
    game_dir: &Path,
    map: &str,
    script: &script::Script,
    params: &PlayerParams,
    scene: &xiii_world::WorldScene,
    duration: f32,
) -> Result<ScriptOutcome, String> {
    run_script_inner(
        game_dir,
        map,
        script,
        params,
        scene,
        duration,
        false,
        RunnerOptions::default(),
    )
}

/// Player-route variant of [`run_script`]: honor the controller's authored cinematic states in
/// exactly the same way as the interactive runtime. Diagnostic movement probes retain the
/// legacy unsuppressed harness path unless they explicitly request this behavior.
pub(crate) fn run_script_with_cinematic_input(
    game_dir: &Path,
    map: &str,
    script: &script::Script,
    params: &PlayerParams,
    scene: &xiii_world::WorldScene,
    duration: f32,
) -> Result<ScriptOutcome, String> {
    run_script_inner(
        game_dir,
        map,
        script,
        params,
        scene,
        duration,
        true,
        RunnerOptions::default(),
    )
}

#[derive(Default)]
struct RunnerOptions {
    initial: Option<MapRuntime>,
    stop_on_travel: bool,
    wait_for_control: bool,
}

fn headless_projection(
    location: [f32; 3],
    eye_height: f32,
    yaw: f32,
    pitch: f32,
) -> xiii_script::canvas::ScreenProjection {
    let (sy, cy) = yaw.sin_cos();
    let (sp, cp) = pitch.sin_cos();
    let forward = [cp * cy, cp * sy, sp];
    let right = [-sy, cy, 0.0];
    let up = [-sp * cy, -sp * sy, cp];
    let eye = [location[0], location[1], location[2] + eye_height];
    let dot = |a: [f32; 3]| a.into_iter().zip(eye).map(|(a, b)| a * b).sum::<f32>();
    // Horizontal FOV 90 degrees, matching the default headless 1280x720 Canvas.
    let aspect = 1280.0 / 720.0;
    xiii_script::canvas::ScreenProjection {
        clip_from_world: [
            [right[0], up[0] * aspect, forward[0], forward[0]],
            [right[1], up[1] * aspect, forward[1], forward[1]],
            [right[2], up[2] * aspect, forward[2], forward[2]],
            [-dot(right), -dot(up) * aspect, -dot(forward), -dot(forward)],
        ],
        viewport: [1280.0, 720.0],
    }
}

/// The innermost decoded movement volume at the player pawn's head
/// (`Location + (0, 0, CollisionHeight)`), the value retail `APawn::SetZone` publishes to
/// `Pawn.HeadVolume` (`Engine.dll` 0x1038CA50). `None` when no volume contains the head point.
fn head_volume_name(runtime: &MapRuntime, params: &PlayerParams) -> Option<String> {
    let half = if runtime.sim.crouched {
        params.crouch_height_uu
    } else {
        params.height_uu
    };
    let head = [
        runtime.sim.location[0],
        runtime.sim.location[1],
        runtime.sim.location[2] + half,
    ];
    runtime
        .volumes
        .volumes
        .physics_at(head)
        .map(|v| v.name.clone())
}

fn player_has_control(session: &session::Session) -> bool {
    !cinematics::input_suppressed(session)
        && session
            .controller
            .is_some_and(|pc| session.vm().is_in_state(pc, "PlayerWalking"))
}

#[allow(clippy::too_many_arguments)]
fn run_script_inner(
    game_dir: &Path,
    map: &str,
    script: &script::Script,
    params: &PlayerParams,
    scene: &xiii_world::WorldScene,
    duration: f32,
    respect_cinematic_input: bool,
    options: RunnerOptions,
) -> Result<ScriptOutcome, String> {
    let started = Instant::now();
    let mut ticks = (duration / DT).ceil() as u64;
    let mut drive = script::Drive::new(script);
    let mut trace = Vec::new();
    let mut travel = Vec::new();
    let mut map_objectives = Vec::new();
    let mut map_traces = Vec::new();
    // item40m: minimum player Health seen on the current map (reset after each travel).
    let mut map_min_health: Vec<(String, f32)> = Vec::new();
    let mut cur_min_health = f32::INFINITY;
    let mut runtime = match options.initial {
        Some(runtime) => runtime,
        None => open_map_runtime(game_dir, map, scene, params)?,
    };
    let mut control_tick = (!options.wait_for_control).then_some(0);
    // item97b: first script second at which the player lost input (death/game-end or a cinematic).
    let mut control_ended: Option<f32> = None;
    let mut control_ended_vm: Option<f64> = None;
    // Accumulated unresolved voice names across maps (the provider is re-installed per map).
    let mut voice_unresolved_total = 0u64;
    // Player footsteps (item6e): the same notify-free synthesis `fixed_step` uses, so the
    // headless path reports and can play them. Rebuilt for each map after a level transition.
    let mut surfaces = footsteps::SurfaceSounds::from_scene(scene);
    let mut step_driver = footsteps::FootstepDriver::new();
    let mut footstep_log: Vec<(f32, String, Option<String>)> = Vec::new();
    let mut tick = 0u64;
    while tick < ticks {
        if control_tick.is_none() && player_has_control(&runtime.session) {
            control_tick = Some(tick);
            println!(
                "[combat-control] map={map} t={:.3} state={}",
                tick as f32 * DT,
                runtime.session.player_controller_state()
            );
        }
        if control_tick.is_none() && tick as f32 * DT >= 120.0 {
            return Err(format!(
                "{map}: player control not returned within 120 seconds; state={}",
                runtime.session.player_controller_state()
            ));
        }
        let elapsed = tick.saturating_sub(control_tick.unwrap_or(tick)) as f32 * DT;
        if let Some(name) = drive.tracking_actor().map(str::to_owned) {
            // Head-zone aim, as in the other drive site: for pawns, a fraction of
            // CollisionHeight above the tracked actor's Location keeps the ray inside the aimed
            // band (0.6 default: spine; 0.85 raises a level ray to the head band for 3x bullet
            // damage); non pawns are aimed at the raw Location.
            let track_height = drive.track_height();
            let location = runtime.session.vm().find_live_object(&name).and_then(|id| {
                let vm = runtime.session.vm();
                vm.vector_prop(id, "Location").map(|mut l| {
                    if vm.is_a(id, "XIIIPawn")
                        && let Some(xiii_script::Value::Float(h)) =
                            vm.get_property(id, "CollisionHeight")
                    {
                        l[2] += h * track_height;
                    }
                    l
                })
            });
            drive.set_track_location(Some(&name), location);
        } else {
            drive.set_track_location(None, None);
        }
        // item107: facts for the closed-loop blocking commands (`engage`, `wait_until`).
        drive.set_travel_count(travel.len() as u32);
        if drive.wants_facts() {
            let suppressed =
                respect_cinematic_input && cinematics::input_suppressed(&runtime.session);
            let facts = perception::gather(
                &drive.fact_request(),
                &mut runtime.session,
                &runtime.sim,
                params,
                suppressed,
            );
            drive.set_facts(facts);
            let vm = runtime.session.vm();
            drive.observe_trace(&vm.trace, vm.trace_enabled);
        }
        let mut input = if control_tick.is_some() {
            drive.advance(elapsed, &mut runtime.sim)
        } else {
            Input::default()
        };
        let weapons = drive.take_weapons();
        let items = drive.take_items();
        let ((fire_hold, fire_edge), (altfire_hold, altfire_edge)) = drive.take_hook_buttons();
        let goals = drive.take_goals();
        let mut weapon_inputs = drive.take_weapon_inputs();
        let mut use_named = drive.take_use_named();
        let control = drive.take_control();
        let mut heal = drive.take_quick_heal();
        let mut exec_inputs = drive.take_exec_inputs();
        // Match the interactive fixed_step: FPC/FPL/CameraView/PlayingVideo own the pawn while
        // the authored cinematic runs. The headless route must still advance its script cursor,
        // but player-axis/action commands are ignored. Explicit test-only set_goal commands are
        // retained; they are host bridges, not player inputs, and existing Banque01 coverage
        // labels them as such.
        if respect_cinematic_input && cinematics::input_suppressed(&runtime.session) {
            input = Input::default();
            // A diagnostic weapon grant is a host command, like wake/set_goal, rather than
            // a player action. Keep it available while an authored cinematic owns input.
            weapon_inputs.clear();
            use_named.clear();
            heal = false;
            exec_inputs.clear();
        }
        // item97b: track the last tick at which player control was still active. Control can be
        // taken for an authored cinematic and handed back, so the effective end is the *last*
        // active tick, not the first suppressed one; consumers use it to count commanded shots
        // issued while alive and to reject input after control ended.
        if control_tick.is_some()
            && !cinematics::input_suppressed(&runtime.session)
            && !runtime.session.player_input_ended()
        {
            control_ended = Some(elapsed);
            control_ended_vm = Some(runtime.session.vm_time());
        }
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
        if let movers::BaseChange::Changed(base) = runtime.rider.observe(
            runtime.sim.grounded,
            runtime.sim.floor_source,
            &runtime.sources,
            |name| runtime.session.vm().mover_state(name),
        ) {
            runtime.session.set_player_base(base.as_deref());
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
            eye_height: runtime.sim.vm_eye_height(&runtime.player_params),
        };
        // item64: publish the held rope buttons before the VM tick so `PressingFire`/
        // `PressingAltFire` polls inside this tick's hook `Timer`/`Tick` see them. The drive's
        // walk axes and yaw ride along: while the grapple owns the pawn, `ProcessMove`'s
        // `newAccel` steers the hanging player from them (the retail input stack's role).
        runtime.session.set_hook_buttons(fire_hold, altfire_hold);
        runtime
            .session
            .set_hook_move(input.forward, input.right, runtime.sim.yaw);
        // item94: the host owns the player pawn's `Location`; a script-set `Base` (an
        // `Actor.SetBase` on a moving brush) is carried by the VM for scripted actors, so record
        // it before the VM step and mirror the base's displacement after (see `BasedCarry`).
        let based_before = runtime.session.player_base_mover();
        let floor_base = runtime.rider.base().map(str::to_owned);
        runtime.based_carry.observe(
            based_before.map(|(n, s)| (n, s.location)),
            floor_base.as_deref(),
        );
        runtime.session.step(
            DT,
            runtime.sim.location,
            runtime.sim.yaw,
            runtime.sim.velocity,
            &modes,
        );
        let head_volume = head_volume_name(&runtime, &runtime.player_params);
        runtime.session.publish_head_volume(head_volume.as_deref());
        runtime
            .session
            .sync_view_rotation(runtime.sim.yaw, runtime.sim.pitch);
        // item40m: record the minimum player Health this tick (before any heal/damage later).
        if let Some(h) = runtime.session.player_health() {
            cur_min_health = cur_min_health.min(h);
        }
        runtime.rider.carry(&mut runtime.sim.location, |name| {
            runtime.session.vm().mover_state(name)
        });
        runtime
            .based_carry
            .carry(&mut runtime.sim.location, |name| {
                runtime.session.vm().mover_state(name)
            });
        // Headless rendering still needs the camera context required by WorldToScreen,
        // including the Hook's authored target-outline pass. Use the simulated view.
        runtime.session.vm_mut().canvas.screen_projection = Some(headless_projection(
            runtime.sim.location,
            runtime.sim.vm_eye_height(&runtime.player_params),
            runtime.sim.yaw,
            runtime.sim.pitch,
        ));
        runtime.session.drive_render_phase();
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
        runtime.session.vm_mut().sync_mover_collision();
        let states = runtime.session.mover_states();
        runtime.mover_collision.update(&mut runtime.world, &states);
        for path in &weapons {
            match runtime.session.grant_weapon(path) {
                Ok(msg) => println!("[play] weapon {msg}"),
                Err(e) => println!("[play] weapon grant failed {path}: {e}"),
            }
        }
        for path in &items {
            match runtime.session.grant_item(path) {
                Ok(msg) => println!("[play] item {msg}"),
                Err(e) => println!("[play] item grant failed {path}: {e}"),
            }
        }
        if drive.take_next_item()
            && let Err(e) = runtime.session.next_item()
        {
            println!("[play] next_item failed: {e}");
        }
        for n in &goals {
            if let Err(e) = runtime.session.set_goal(*n) {
                println!("[play] set_goal {n} failed: {e}");
            }
        }
        for group in weapon_inputs {
            if let Err(e) = runtime.session.weapon_input(group) {
                return Err(format!("weapon input {group:?}: {e}"));
            }
        }
        if control {
            match runtime.session.take_control() {
                Ok(state) => println!("[play] take_control diagnostic: controller -> {state}"),
                Err(e) => println!("[play] take_control failed: {e}"),
            }
        }
        for name in exec_inputs {
            if let Err(e) = runtime.session.controller_input(name) {
                runtime.session.blocked.push(format!("{name} input: {e}"));
            }
        }
        if heal && let Err(e) = runtime.session.quick_heal() {
            println!("[play] quick_heal failed: {e}");
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
        if fire_edge {
            let outcome = runtime
                .session
                .fire_pressed(runtime.sim.yaw, runtime.sim.pitch);
            println!(
                "[play] fire_hold press [{elapsed:.3}s]: {outcome:?} | player {} | bone {} | {}",
                runtime
                    .session
                    .player_health()
                    .map(|h| format!("{h:.0} hp"))
                    .unwrap_or_else(|| "? hp".to_owned()),
                runtime.session.vm().last_trace_bone(),
                combat_snapshot(&runtime.session)
            );
        }
        if altfire_edge {
            let outcome = runtime.session.altfire_pressed();
            println!("[play] altfire_hold press [{elapsed:.3}s]: {outcome:?}");
        }
        if tick.is_multiple_of(TRACE_EVERY) || tick + 1 == ticks {
            trace.push((tick, elapsed, runtime.sim.location, runtime.sim.velocity));
            println!(
                "[play] {} | {} | {}",
                format_trace(tick, elapsed, &runtime.sim),
                format_vm_trace(&runtime.session),
                format_mover_trace(&runtime.session),
            );
        }

        // Level transition: the game's own goal/travel code requested it. The VM reported the
        // URL; the host imports the next map and carries the native travel actor properties.
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
            map_traces.push((runtime.name.clone(), runtime.session.vm().trace.clone()));
            map_min_health.push((runtime.name.clone(), cur_min_health));
            cur_min_health = f32::INFINITY;
            if options.stop_on_travel {
                tick += 1;
                break;
            }
            // The script block (if any) is released; the next map starts a fresh run.
            drive.notify_travel();
            drive.session_changed();
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
            let next_session = travel::open_next_session(&mut runtime.session, game_dir, &plan)?;
            runtime = build_map_runtime(game_dir, &plan.map, &next_scene, params, next_session)?;
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
        if control_tick.is_none() {
            ticks += 1;
        }
    }
    if drive.waiting_travel() && travel.is_empty() {
        println!(
            "[play] script is still waiting for travel after {:.1}s; no travel was requested",
            duration
        );
    }
    map_objectives.push((runtime.name.clone(), runtime.session.objective_states()));
    map_traces.push((runtime.name.clone(), runtime.session.vm().trace.clone()));
    map_min_health.push((runtime.name.clone(), cur_min_health));
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
        ticks: tick,
        wall_secs: started.elapsed().as_secs_f32(),
        control_ended,
        control_ended_vm,
        trace,
        final_map: runtime.name,
        travel,
        map_objectives,
        map_traces,
        map_min_health,
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
            script.budget() + 60.0
        } else {
            script.budget() + 2.0
        }
    });
    let outcome =
        run_script_with_cinematic_input(&game_dir, &map, &script, &params, &scene, duration)?;
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

    #[test]
    fn scripted_exit_budget_survives_slow_frames_and_startup() {
        assert_eq!(unattended_elapsed(true, 0, 90.0), 0.0);
        assert_eq!(unattended_elapsed(true, 120, 90.0), 2.0);
        assert_eq!(unattended_elapsed(false, 120, 90.0), 90.0);
        assert_eq!(screenshot_at(true, 164.0), 164.0);
        assert_eq!(screenshot_at(false, 164.0), 123.0);
    }

    /// item40e: the authored Plage01 opening of `tests/data/plage01_route.script` up to its
    /// `# fight-probe cut` marker line: hut escape, objective promotion, the full beach nav walk
    /// and the TouchTrigger7 shore touch that wakes BaseSoldier6 (the killer) out of his
    /// IAController `faction` stasis (`SetCollision(false)`, DrawType none). UE2 traces only
    /// reach collision-hash actors (`bCollideActors`), so the combat probes can shoot the awake
    /// killer after the wake chain. The fight tests hold the truck-shore position while he
    /// approaches the water's edge (round 8, measured in r8_e2/r9_route1).
    fn plage01_killer_awake_prefix() -> String {
        let route = include_str!("../../tests/data/plage01_route.script");
        let mut out = route
            .lines()
            .take_while(|l| !l.starts_with("# fight-probe cut"))
            .collect::<Vec<_>>()
            .join("\n");
        out.push('\n');
        out
    }

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

    /// item64 opt-in regression: the hook use chain on Toits01. Grant + select `XIII.Hook`, aim
    /// at `HookPoint0` from the start roof, fire (the exec `Fire` -> `ActivatedByFire` ->
    /// `Idle.Activate` -> `UseMe` attach), hold fire (the `HookInUse.Timer` -> `bGoUp` climb and
    /// the `PlayerWalking.ProcessMove` taut-rope pull lift the pawn), then use (the `Grab` exec's
    /// `bHooked` branch -> `Hook.Release()` detach). The retail input model: a continuous fire
    /// hold never climbs (the first `HookInUse.Tick` consumes the Walking `bGoDown` and clears
    /// the `ActivatedBy*` flags) — the climb needs a fresh fire press.
    #[test]
    fn opt_in_hook_use_chain_smoke() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Toits01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Toits01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let prefix = "t=0.50 item XIII.Hook\nt=1.50 teleport -4926 1446 979\n\
             t=2.00 track HookPoint0\nt=2.50 fire_hold 1\nt=3.00 fire_hold 0\n\
             t=3.50 fire_hold 1\nt=6.00 fire_hold 0\n";
        // Run A stops before the use key: the attach and the climb must be observable.
        let attach = run_script_with_cinematic_input(
            &game_dir,
            "Toits01",
            &script::Script::parse(prefix).expect("parse attach script"),
            &resolved.params,
            &scene,
            6.0,
        )
        .expect("run attach smoke");
        let vm = attach.session.vm();
        // The map carries its own XIII.Hook pickup, so resolve the GRANTED item through the
        // session's SelectedItem (the grant selects it).
        let hook = attach
            .session
            .selected_item()
            .expect("the grant must leave the hook selected");
        assert!(
            vm.is_a(hook, "Hook"),
            "SelectedItem must be the hook, got class {}",
            vm.set().path(vm.objects[hook as usize].class)
        );
        // The attach: the exec's ActivatedByFire -> ActivateItem -> Idle.Activate -> UseMe path
        // completed - MyHook spawned, the rope length is locked, and the pawn started hooking.
        // (ActivatedByFire is consumed by the HookInUse Timer afterwards, so it is not asserted.)
        assert!(
            vm.get_property(hook, "MyHook")
                .is_some_and(|v| !matches!(v, Value::Object(None))),
            "UseMe must spawn the HookProjectile (Hook.MyHook)"
        );
        // MaxHookLength is UseMe's copy of the aimed anchor's RopeLength; only HookPoint0
        // carries 1900, so this proves the render-phase item trace aimed at it. (PotHookPoint
        // itself is transient: ResetPotentialTargets clears it once the aim moves off.)
        assert_eq!(
            vm.get_property(hook, "MaxHookLength"),
            Some(&Value::Float(1900.0)),
            "UseMe must read the anchor's RopeLength (the range check ran)"
        );
        assert_eq!(
            vm.get_property(hook, "bHooked"),
            Some(&Value::Bool(true)),
            "UseMe must set the hook's bHooked (the attach completed)"
        );
        // The climb: 2.25 s of held fire from the fresh press re-arms the Timer -> bGoUp ->
        // ReduceRopeLength (150 UU/s), and each ProcessMove pulls the pawn up the excess. The
        // pawn must have LEFT the start roof (979) toward the anchor.
        let pawn = match vm.get_property(hook, "Owner") {
            Some(Value::Object(Some(xiii_script::ObjRef::Instance(i)))) => *i,
            other => panic!("the hook must own the pawn, got {other:?}"),
        };
        let z = match vm.get_property(pawn, "Location") {
            Some(Value::Vector(v)) => v[2],
            other => panic!("pawn Location, got {other:?}"),
        };
        assert!(
            z > 1050.0,
            "the held fire must lift the pawn off the start roof (z={z})"
        );
        // Run B adds the use key: the Grab exec's bHooked branch detaches the rope.
        let detach = run_script_with_cinematic_input(
            &game_dir,
            "Toits01",
            &script::Script::parse(&format!("{prefix}t=6.50 use\n")).expect("parse detach script"),
            &resolved.params,
            &scene,
            10.0,
        )
        .expect("run detach smoke");
        let vm = detach.session.vm();
        let hook = detach
            .session
            .selected_item()
            .expect("the grant must leave the hook selected");
        // The detach: the use key's Grab exec -> bHooked && !bWeaponMode && SelectedItem is
        // Hook -> Hook.HookInUse.Release destroys MyHook and clears bHooked on both sides.
        assert_eq!(
            vm.get_property(hook, "bHooked"),
            Some(&Value::Bool(false)),
            "the use key must detach the rope (Hook.Release ran)"
        );
        assert!(
            vm.get_property(hook, "MyHook").is_none_or(|v| match v {
                Value::Object(None) => true,
                Value::Object(Some(xiii_script::ObjRef::Instance(i))) => {
                    // Actor.Destroy marks the object deleted; the property keeps the last ref.
                    vm.objects[*i as usize].deleted
                }
                other => panic!("Hook.MyHook, got {other:?}"),
            }),
            "Hook.Release must destroy the HookProjectile (Hook.MyHook)"
        );
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
        sim.grounded = !spawn.airborne;
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

    /// Opt-in corpus test (item48): the four maps where the item45 survey reported
    /// `AdjustAimForDisplay` (Hual01a + the HUD-render suspension set), `VisibleDamageableActors`
    /// (Amos01, SPADS02b explosive canisters) and `WaveHasPosition` (SMarin01, SPADS02b intro
    /// dialogue), opened through the same headless helper the route tests use (empty input
    /// script, 90 s, no player input). Asserts the natives are no longer reported (no suspended
    /// actors, no failures) and prints what happens instead: the `PlayStrVoice` dialogue lines,
    /// the `TakeDamage` events the `HurtRadius` iterator delivered, the player's final health
    /// and the natives' call counts.
    #[test]
    fn opt_in_item48_natives_no_longer_reported_on_survey_maps() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        for map in ["Hual01a", "Amos01", "SMarin01", "SPADS02b"] {
            let opts = Options {
                map: Some(map.to_owned()),
                game_dir: Some(game_dir.clone()),
                ..Default::default()
            };
            let scene = viewer::load_scene(&opts).unwrap_or_else(|e| panic!("import {map}: {e}"));
            let script = script::Script::parse("").expect("empty input script");
            let outcome = run_script_with_cinematic_input(
                &game_dir,
                map,
                &script,
                &resolved.params,
                &scene,
                90.0,
            )
            .unwrap_or_else(|e| panic!("{map}: {e}"));
            let sess = &outcome.session;
            assert!(
                sess.suspended.is_empty(),
                "{map}: suspended actors {:#?}",
                sess.suspended
            );
            assert!(
                sess.failures.is_empty(),
                "{map}: script failures {:#?}",
                sess.failures
            );
            let used = &sess.vm().natives_used;
            for path in [
                "PlayerController.AdjustAimForDisplay",
                "Actor.VisibleDamageableActors",
                "Actor.WaveHasPosition",
            ] {
                let hit = used
                    .iter()
                    .find(|(p, _)| p.eq_ignore_ascii_case(path))
                    .map(|(_, v)| *v);
                println!(
                    "[item48] {map}: {path} calls {:?}",
                    hit.unwrap_or((None, 0))
                );
            }
            for (t, d) in &sess.dialogues {
                println!(
                    "[item48] {map}: t={t:.3}s dialogue sound={:?} speaker={:?} text={:?}",
                    d.sound, d.speaker, d.text
                );
            }
            let damages: Vec<String> = sess
                .vm()
                .trace
                .iter()
                .filter_map(|e| match &e.kind {
                    xiii_script::TraceKind::Event {
                        target,
                        function,
                        args,
                    } if function.ends_with("TakeDamage") => Some(format!(
                        "t={:.3}s {} <- {}({})",
                        e.time,
                        target,
                        function,
                        args.join(", ")
                    )),
                    _ => None,
                })
                .collect();
            println!("[item48] {map}: {} TakeDamage events", damages.len());
            for line in damages.iter().take(20) {
                println!("[item48] {map}:   {line}");
            }
            let health = sess.vm().get_property(sess.player, "Health").cloned();
            println!(
                "[item48] {map}: final player health {:?}, {} dialogue event(s)",
                health, sess.dialogue_total
            );
        }
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
        let outcome = run_script_with_cinematic_input(
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
        // Start 3 s after the old 45 s mark: with the engine's cine arrival rule
        // (XIDCine IsTargetReached) the Plage01 intro returns control at ~46.5 s, not ~44.5 s.
        let script = script::Script::parse(
            "t=48.00 teleport -491.8 -414.1 1265.0\n\
             t=48.10 goto -491.84 -314.14\nt=48.30 jump\nt=48.80 jump\nt=49.30 jump\nt=49.80 jump\n\
             t=50.30 jump\nt=50.80 forward 0\n\
             t=51.20 teleport -742.1444 -808.429 1311.0449\n\
             t=51.20 yaw 312.891\nt=51.20 turn 2\nt=51.20 forward 1\n\
             t=52.80 turn -45\nt=53.50 forward 0\nt=53.80 use\nt=54.80 use\nt=55.00 forward 1\n\
             t=56.00 forward 0\n",
        )
        .unwrap();
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            57.0,
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
    /// the route file uses walked input without teleports. Pickup,
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
        // Round 8: the walked route fights at the truck shore and uses Porte1 at t=345-346.
        // Keep the existing 640 s budget, including the level-end cinematic and map load;
        // the travel timestamp is measured by the result below.
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            640.0,
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
        // item49b acceptance, post-travel half: the truck key taken from the killer's corpse
        // must survive the `items=true` server travel inside the player's inventory. The
        // pre-travel corpse-search leg is asserted by
        // `opt_in_item49b_plage01_corpse_search_runs_game_path`.
        assert!(
            outcome
                .session
                .vm()
                .objects
                .iter()
                .enumerate()
                .any(|(i, o)| {
                    let id = i as xiii_script::ObjectId;
                    !o.deleted
                        && outcome.session.vm().is_a(id, "keys")
                        && matches!(
                            outcome.session.vm().get_property(id, "Owner"),
                            Some(xiii_script::Value::Object(Some(
                                xiii_script::ObjRef::Instance(owner),
                            ))) if *owner == outcome.session.player
                        )
                }),
            "a live truck key must still be carried by the player after the travel"
        );
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
                || rendered.contains("SearchPawn")
                || rendered.contains(".Transfer")
                || rendered.contains("AddInventory")
                || rendered.contains("Grab")
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

    /// item33 acceptance: execute the tracked Plage01 route through the picked-up Beretta's fire
    /// burst, deliver
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
        // item30c round 8: the truck-shore route fights at t=291-311 (the killer dies at ~305,
        // scratch r9_route1) and the game's auto-search drains the corpse as the player walks
        // onto it at ~313; the cut keeps the corpse search and several seconds so the game's
        // own post-burst weapon state has settled (the e7f/e7g measurement), and stops before
        // the route's `use Porte1` (t=345) so the run cannot reach the level-end travel.
        script.events.retain(|event| event.t <= 320.0);
        let mut outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            336.0,
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
        // item107: the killer's burst is a closed-loop `engage BaseSoldier6` (it presses the
        // trigger as often as the target stays alive and visible) instead of >= 10 recorded
        // `fire` lines; the kill asserted below is the measured proof that the burst fired.
        let killer_engagements = script
            .events
            .iter()
            .filter(|event| {
                matches!(&event.command, script::Command::Engage(spec)
                    if spec.target.as_deref() == Some("BaseSoldier6"))
            })
            .count();
        assert!(
            requested_shots >= 10 || killer_engagements >= 1,
            "tracked route must fire a burst, got {requested_shots} fire commands and {killer_engagements} killer engagements"
        );
        let target = outcome
            .session
            .vm()
            .find_object("BaseSoldier6")
            .expect("route killer");
        assert!(
            outcome.session.actor_is_dead(target),
            "tracked route's fired Beretta did not kill BaseSoldier6"
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
        // Round 8: switch_weapon 2 requests the picked-up Beretta before the burst and
        // selects it again at t=315 after the corpse search. Save/restore preserves this state.
        assert!(
            weapon_class.to_ascii_lowercase().contains("beretta"),
            "selected weapon after route: {weapon_class}"
        );
        // The picked-up Beretta and the selected weapon must remain real inventory entries.
        let before_inventory = outcome.session.inventory_items();
        assert!(
            before_inventory
                .iter()
                .any(|(_, class)| class.eq_ignore_ascii_case("xiii.beretta")),
            "the picked-up Beretta is absent from the inventory chain: {before_inventory:?}"
        );
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
        // The chain now begins with the authored entry defaults (`Fists` -> `FistsAmmo` -> ...),
        // so "the first ammo item" is no longer the route weapon's ammunition. Compare the
        // selected weapon's own ammo class end to end (the route fights with the picked-up
        // Beretta, so its AmmoType is `c9mmAmmo` - the old m60-era assert hardcoded `XIII.M60Ammo`).
        let ammo_class = match outcome.session.vm().get_property(weapon, "AmmoType") {
            Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(id)))) => outcome
                .session
                .vm()
                .set()
                .path(outcome.session.vm().objects[*id as usize].class),
            other => panic!("selected weapon has no instance AmmoType: {other:?}"),
        };
        let ammo = saved
            .inventory
            .iter()
            .find(|item| item.class_path.eq_ignore_ascii_case(&ammo_class))
            .and_then(|item| {
                item.ammo_amount
                    .map(|amount| (item.class_path.clone(), amount))
            })
            .unwrap_or_else(|| {
                panic!("saved travel inventory includes the selected weapon's ammunition ({ammo_class})")
            });
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
        // Retail AcceptInventory @429895..431665, bytecode 0x0173, restores
        // Max(int(default.Health * 0.25 + 1), Health). The live-clock route
        // saves 26 HP; the authored 150-HP class therefore raises it to 38.
        assert_eq!(resumed.player_health(), Some(saved.health.max(38.0)));
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
            other => panic!("restored selected weapon has no instance AmmoType: {other:?}"),
        };
        let restored_ammo_count = match resumed.vm().get_property(restored_ammo_id, "AmmoAmount") {
            Some(xiii_script::Value::Int(n)) => *n,
            other => panic!("restored selected weapon's ammo has no int AmmoAmount: {other:?}"),
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
        // Authored `XIIIGameInfo.AcceptInventory` adds exactly three classes beyond the saved
        // travel inventory, and only when the restored chain lacks them (all measured script):
        // the default weapon `Fists` (`AddDefaultInventory` 0x04CF ->
        // `BaseMutator.GetDefaultWeapon` -> `XIIISoloMutator.DefaultWeaponName`="XIII.Fists",
        // Spawn+GiveTo; `Weapon.GiveAmmo` links `FistsAmmo` at amount 0) and `XIIILeftHand`
        // (0x0844-0x08A0, Spawn+GiveTo when `FindInventoryType` misses). A fresh-start route
        // save legitimately lacks them because `XIIIGameInfo.RestartPlayer` omits stock UE2's
        // `AddDefaultInventory` and these maps carry no MapInfo `InitialInv`; a save that
        // already holds the defaults must be restored without duplicates.
        let mut expected_extras: Vec<String> = Vec::new();
        for class in ["xiii.fists", "xiii.fistsammo", "xiii.xiiilefthand"] {
            if !saved_classes.iter().any(|c| c == class) {
                expected_extras.push(class.to_owned());
            }
        }
        expected_extras.sort();
        assert_eq!(
            remaining, expected_extras,
            "only AcceptInventory's authored default weapon/ammo/left-hand entries may be added"
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
        // The route's fired Beretta is in the saved+restored chain (not the selected weapon -
        // the game's own post-burst switch left Fists selected), so AcceptInventory must
        // preserve its saved clip; the BringUp ReloadCount clamp at restore applies only to the
        // selected weapon.
        let saved_beretta = saved
            .inventory
            .iter()
            .find(|item| item.class_path.eq_ignore_ascii_case("XIII.Beretta"))
            .expect("saved Beretta actor");
        let restored_beretta = restored_inventory
            .iter()
            .find(|item| item.class_path.eq_ignore_ascii_case("XIII.Beretta"))
            .expect("restored Beretta actor");
        assert_eq!(
            restored_beretta.reload_count, saved_beretta.reload_count,
            "the saved Beretta's ReloadCount must survive the restore"
        );
        let saved_c9mm = saved
            .inventory
            .iter()
            .find(|item| item.class_path.eq_ignore_ascii_case("XIII.c9mmAmmo"))
            .and_then(|item| item.ammo_amount)
            .expect("saved Beretta ammunition actor");
        assert!(
            saved_c9mm >= 0,
            "invalid saved c9mm ammo count: {saved_c9mm}"
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
    /// keeps four labelled flow teleports whose legs are re-measured unwalkable on this tree
    /// (item93, by input and `box-probe`: the strongroom is sealed at the safe level
    /// (Model69 x=-6976, z 800..1312) and the platform level (x=-7360, z 1312..1504) and nothing
    /// in the map opens it; the lift cage has no floor collision and the closed outer doors
    /// answer the ray use with the empty `PlayerTrigger`, so no ride carries a player; the
    /// shaft wall y=-2080 and the floor-2 corridor's desk + full-height hall wall y=-3664
    /// dead-end the TT2 side; the banksortiescasecour tower's ramp face and east cheek x=998.2
    /// stall every walk-off/jump from the TT2 landing and the courtyard's own walls allow no
    /// street-side entry). The old t=139 TT10 teleport is gone: the fbi_dial2 cutscene owns the
    /// player pose from t=134.017 (the TT3/TT11 touch tick, measured), so it never relocated
    /// the player nor fired TouchTrigger10 (measured); the CarOut beat does not fire in this
    /// engine run.
    ///
    /// item86 re-time on the integrated volume-brush tree: `DetectionVolume12` now uses its
    /// `Brush` BSP (x in (-7824,-7616), y in (-2784,-2336), z in (784,1248), measured), so its
    /// Location (-7616,-2784,1456) is above the brush and the old teleport-to-Location touch at
    /// t=93.017 no longer fires. The route walks into the volume on the safe level: DV12 Touch
    /// at t=96.6, then the walkway waypoint back to Jones, then the walkway; Cine15 reaches
    /// `fin_flash` at t=127.1 and XIIIDispatcher4 promotes objective 0 at t=140.1. The escape
    /// teleports are re-timed to t=128/131/134 so they follow `fin_flash`; objective 1 completes
    /// by t=147 and the measured `ServerTravel` runs at t=183.0.
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
        // 185 s: the measured schedule needs it — the route walks to the vault door (touches
        // t~88), teleports to DetectionVolume12's platform at t=93.0 and walks into the volume's
        // brush, delivering DV12's Touch at t=96.6; Cine15's scene then runs to `fin_flash` at
        // t=127.1 (its `wait player 300` actions pass at t=104.5/111.0). The escape-flow
        // teleports run t=128..134 (TT5 128.0, TT2 131.0, TT3 134.0, TT4 133.333 on the natural
        // fall), objective 0 completes at t=140.1 (fin_flash + the 13 s XIIIDispatcher4 delay)
        // and objective 1 by t=147 (Cine11's fin_map -> XIIIGoalTrigger2). The map-outro video
        // then runs its full 33.92 s headless before `PlayingVideo.PlayerTick` issues the
        // `ServerTravel` (measured travel t=182.967 in the item86 run).
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "banque01",
            &script,
            &resolved.params,
            &scene,
            185.0,
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
        // item49b diagnostics for the ending flow (CarOut -> goal completion -> fin_map video ->
        // ServerTravel); bounded prints, kept visible for route forensics.
        println!("[route] banque01 failures: {:?}", outcome.session.failures);
        for (i, o) in outcome.session.vm().objects.iter().enumerate() {
            if o.suspended && !o.deleted {
                println!("[route] banque01 suspended actor: #{} {}", i, o.name);
            }
        }
        let mut endflow = 0;
        for ev in outcome.session.vm().trace.iter() {
            let rendered = format!("{:?}", ev.kind);
            // Script events, state changes and timers only: the per-tick native polling is noise.
            let event_like = rendered.starts_with("Event")
                || rendered.starts_with("StateChange")
                || rendered.starts_with("Timer")
                || rendered.starts_with("Latent");
            let interesting = event_like && ev.time >= 138.0;
            if interesting {
                println!("[route] banque endflow t={:.3}: {rendered}", ev.time);
                endflow += 1;
                if endflow > 200 {
                    break;
                }
            }
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

    #[test]
    fn opt_in_banque01_vm_profile() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to run the Banque01 VM profile");
            return;
        };
        let opts = Options {
            map: Some("Banque01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Banque01");
        let resolved = resolve_params(&game_dir).expect("player parameters");
        let route = script::Script::load(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/banque01_route.script"),
        )
        .expect("route");
        let mut runtime =
            open_map_runtime(&game_dir, "Banque01", &scene, &resolved.params).expect("runtime");
        runtime.session.enable_native_timers(true);
        let outcome = run_script_inner(
            &game_dir,
            "Banque01",
            &route,
            &resolved.params,
            &scene,
            181.0,
            true,
            RunnerOptions {
                initial: Some(runtime),
                ..Default::default()
            },
        )
        .expect("profile route before travel");
        println!("[vm-profile] {:#?}", outcome.session.vm().native_profile());
        assert!(
            outcome.session.failures.is_empty(),
            "{:?}",
            outcome.session.failures
        );
        assert!(outcome.session.vm().native_profile().enabled);
    }

    #[test]
    fn opt_in_banque01_postrender_completes_while_player_moves() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = session::Session::open(&game_dir, "Banque01").expect("open Banque01");
        let dt = 1.0f32 / 30.0;
        let mut location = session.player_location().expect("player location");
        let velocity = [1.0, 0.0, 0.0];
        for _ in 0..90 {
            session.drive_render_phase();
            location[0] += velocity[0] * dt;
            session.step(
                dt,
                location,
                0.0,
                velocity,
                &session::PlayerVMModes::default(),
            );
            if let Some(pose) = session.script_pawn_pose() {
                location = pose.0;
            }
        }
        assert!(
            session.failures.is_empty(),
            "render/step failures: {:?}",
            session.failures
        );
        let mi = session.map_info().expect("MapInfo");
        assert_eq!(
            session.vm().get_property(mi, "EndCartoonEffect"),
            Some(&xiii_script::Value::Bool(true))
        );
        let cine = session.vm().find_live_object("Cine1").expect("Cine1 actor");
        assert_eq!(
            session.vm().get_property(cine, "bInitialized"),
            Some(&xiii_script::Value::Bool(true))
        );
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
        for map in ["Plage00", "Plage01", "Amos01"] {
            probe_objectifs(&game_dir, map);
        }
    }

    /// item40/item40e route: player input plus named use/grab of map actors (no teleports,
    /// goal bridges or weapon grants). item40f extends it past the duct: the chair swing
    /// breaks the duct grille `BreakAbleMover16`, the crawl and west/north loop punch the
    /// vent grilles (`BreakableMover9/8/57/5/6`) through the game's own fire chain, the
    /// rooftop window crawl reaches `TouchTrigger39` (objective 1) and the level travels to
    /// Toits01 (see the item40f report). item40m re-timed the post-duct tail to continuous
    /// movement so the live AI (whose fire killed the player at t=644 with the old
    /// dash-and-wait cadence) never engages: the route is densely resampled along the same
    /// polyline at a conservative ground speed, and the test pins the measured minimum player
    /// Health. The Beretta+c9mmAmmo is the level's own `BerettaPick1` touch pickup. item86 re-timed
    /// the descent on the integrated engine-fidelity tree (automatic AI bursts + retail
    /// volume-brush overlap): the route now passes east of the west-facing Roulade patroller and
    /// keeps moving through the Porte7 wait (see the item86 report).
    #[test]
    fn opt_in_amos01_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Amos01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Amos01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let route_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/amos01_route.script");
        let script = script::Script::load(&route_path).expect("load item40 Amos01 route");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Amos01",
            &script,
            &resolved.params,
            &scene,
            1400.0,
        )
        .expect("run Amos01 route");
        let pc = outcome
            .session
            .controller
            .expect("Amos01 player controller");
        let player_state = outcome.session.vm().state_name(pc);
        let objectives = outcome.session.objective_states();
        println!(
            "[amos01 route] player={} final_state={player_state:?} inventory={:?} bWeaponMode={:?} OldWeap={:?} objectives={objectives:?} travel={:?}",
            outcome.session.vm().objects[pc as usize].name,
            outcome.session.inventory_items(),
            outcome.session.vm().get_property(pc, "bWeaponMode"),
            outcome.session.vm().get_property(pc, "OldWeap"),
            outcome.travel
        );
        assert_eq!(
            outcome.final_map, "Toits01",
            "the route must reach TouchTrigger39 and travel to Toits01"
        );
        // item86: the integrated engine-fidelity tree makes the AI fire automatic bursts (item76
        // `Pawn.PressingFire`) and the retail volume-brush overlap fires the level's detection
        // volumes; the same route then bled 150 -> 0 at the descent and the final-area
        // reinforcements. The route now passes east of the dormant Roulade patroller (BaseSoldier9,
        // facing west at -1035,595) so the player is behind its 180-degree sight cone, and keeps
        // moving through the Porte7 wait. Measured floor 44 (was 28 pre-integration, dead on
        // integration); pin it so a change that lets the AI engage again fails visibly.
        // item101: the retail ledge rule keeps the map's AI walkers on their platforms (they no
        // longer walk off and remove themselves), so a soldier that used to fall now stays and
        // adds fire; the measured Amos01 floor moves 44 -> 29 (run: route filter). The route
        // player still survives; the floor is re-pinned to the new measured value so a further
        // regression still fails visibly.
        let amos_min_health = outcome
            .map_min_health
            .iter()
            .find(|(m, _)| m == "Amos01")
            .map(|(_, h)| *h)
            .expect("the run must record Amos01's minimum player Health");
        println!(
            "[amos01 route] minimum player Health along Amos01: {amos_min_health} (measured; \
             survived the live-AI fire)"
        );
        assert!(
            amos_min_health.is_finite() && amos_min_health >= 25.0,
            "the route player must survive the live-AI fire with margin (item101 floor 29); \
             got {amos_min_health} (a non-finite value means no Health was ever observed)"
        );
        assert!(
            outcome
                .travel
                .iter()
                .any(|hop| hop.from == "Amos01" && hop.to == "Toits01"),
            "expected an Amos01 -> Toits01 travel hop: {:?}",
            outcome.travel
        );
        assert_eq!(
            player_state.as_deref(),
            Some("PlayerWalking"),
            "the authored cutscenes must return control through the initialized interaction path"
        );
        assert!(
            matches!(
                outcome.session.vm().get_property(pc, "MyInteraction"),
                Some(xiii_script::Value::Object(Some(
                    xiii_script::ObjRef::Instance(_)
                )))
            ),
            "InitInputSystem must provide MyInteraction"
        );
        // item40f acceptance (task item 3): Amos01's rooftop objective completes through the
        // game's own chain (TouchTrigger39 Event 'findemap' -> XIIIGoalTrigger0, GoalNumber 1,
        // -> CauseGoal -> SetGoalComplete -> ServerTravel) before the map travels.
        let (amos_map, amos_objectives) = outcome
            .map_objectives
            .iter()
            .find(|(m, _)| m == "Amos01")
            .expect("the run must record Amos01's objective snapshot");
        let rooftop = amos_objectives
            .iter()
            .find(|o| o.index == 1)
            .expect("rooftop objective");
        assert!(
            rooftop.primary && rooftop.completed,
            "the rooftop objective must complete before the travel: {amos_map} {rooftop:?}"
        );
        let anti_goal = amos_objectives
            .iter()
            .find(|o| o.index == 0)
            .expect("FBI anti-goal");
        assert!(
            anti_goal.anti_goal && anti_goal.completed,
            "the do-not-kill anti-goal must be completed (not violated): {anti_goal:?}"
        );
        // item40e milestones, from the travelled-from map's retained VM trace: the chair grab
        // runs through the controller script (no host bridge) and the swing destroys the grille.
        let amos_trace = &outcome
            .map_traces
            .iter()
            .find(|(m, _)| m == "Amos01")
            .expect("the run must retain Amos01's VM trace")
            .1;
        assert!(
            amos_trace.iter().any(|event| {
                (123.0..123.1).contains(&(event.time as f32))
                    && matches!(&event.kind,
                xiii_script::TraceKind::Event { function, .. }
                    if function.ends_with("XIIIPlayerController.Grab"))
            }),
            "the chair must be grabbed by the controller script, not a host Touch bridge"
        );
        assert!(
            !amos_trace.iter().any(|event| matches!(&event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                if target.eq_ignore_ascii_case("ChaiseDeco2")
                    && function.ends_with(".TakeDamage"))),
            "the chair swing breaks grilles through the mover damage path, not by damaging the chair"
        );
        let broken = [
            "BreakAbleMover16",
            "BreakAbleMover44",
            "BreakAbleMover56",
            "BreakableMover9",
            "BreakAbleMover8",
            "BreakableMover6",
        ];
        let damaged: Vec<String> = amos_trace
            .iter()
            .filter_map(|event| match &event.kind {
                xiii_script::TraceKind::Event {
                    target, function, ..
                } if function.contains("TakeDamage") => Some(target.to_ascii_lowercase()),
                _ => None,
            })
            .collect();
        for name in broken {
            assert!(
                damaged.iter().any(|t| t.eq_ignore_ascii_case(name)),
                "{name} must take scripted damage along the route"
            );
        }
        let finish = outcome.trace.last().expect("player trace").2;
        let first_after_intro_state = outcome
            .trace
            .iter()
            .find(|sample| sample.1 >= 0.5)
            .expect("post-cutscene-state player trace")
            .2;
        assert_ne!(
            first_after_intro_state, finish,
            "the returned PlayerWalking controller must accept the route's forward input"
        );
        println!(
            "[amos01 route] PASS: route to TouchTrigger39, objective 1 completed via XIIIGoalTrigger0, travel to Toits01 at t={:.3}",
            outcome
                .travel
                .first()
                .map(|h| h.vm_time)
                .unwrap_or_default()
        );
    }

    /// item52 route: Hual01a with both MapInfo objectives completed by the game's own chains,
    /// then the game's own campaign travel to Hual01b (`MapInfo.NextMapLevelWithUnr`). The
    /// fixture walks the whole map by player input (no teleports except two labelled measured
    /// movement gaps, no `take_control`, no `set_goal`, no weapon grant). Measured chains:
    /// the EDF handle `Porte6` fires 'Goal_Manette_EDF' -> `XIIIGoalTrigger3` (goal 666) ->
    /// the map's own `xidmaps.Hual01a.SetGoalComplete` override (promotes objective 1 and
    /// completes it); the dam-crest focus window (`CWndFocusTrigger3`, armed by `TouchTrigger8`)
    /// hands its 'PontA' Tag off on dismissal and the `XIIIMover0/5` panels close the bridge
    /// through their own `TriggerToggle`; `Trigger2` ('End_of_level') fires `XIIIGoalTrigger1`
    /// (goal 0) and `TestGoalComplete` -> `DoTravel` -> `ServerTravel` requests the travel.
    /// The host `fire` at the lever supplies only the press moment the decoded scripts never
    /// show (the native consumer is a labelled evidence gap, see `session.fire`); everything
    /// downstream - the trigger's own `WaitEndFocus.Trigger` dismissal and the `PontA` Tag
    /// delivery - is game code.
    #[test]
    fn opt_in_hual01a_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Hual01a".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Hual01a");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        // Tracked fixture (our own route commands; no game data).
        let route_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/hual01a_route.script");
        let script = script::Script::load(&route_path).expect("load item52 Hual01a route");
        assert!(
            script.events.iter().all(|event| {
                !matches!(
                    &event.command,
                    script::Command::TakeControl
                        | script::Command::SetGoal(_)
                        | script::Command::Weapon(_)
                )
            }),
            "the Hual01a route must not bridge control, goals or weapons"
        );
        // 210 s: the route reaches the shaft teleport at t=194, the game's own travel request
        // lands at t~196.5 (measured), the host reloads Hual01b and the run ends there.
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Hual01a",
            &script,
            &resolved.params,
            &scene,
            210.0,
        )
        .expect("run Hual01a route");
        for (map, states) in &outcome.map_objectives {
            println!(
                "[hual01a route] objectives as the run left {map}: {}",
                states
                    .iter()
                    .map(|o| format!(
                        "[{}{}{}{}] {}",
                        o.index,
                        if o.primary { " P" } else { " -" },
                        if o.completed { " C" } else { " ." },
                        if o.anti_goal { " A" } else { "" },
                        o.text
                    ))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        println!(
            "[hual01a route] travel: {:?}, final map {}",
            outcome.travel, outcome.final_map
        );
        let hual01a = outcome
            .map_objectives
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Hual01a"))
            .expect("Hual01a objective states");
        assert!(
            hual01a.1.len() >= 2,
            "Hual01a MapInfo must expose its two objectives: {:?}",
            hual01a.1
        );
        assert!(
            hual01a.1[0].primary && hual01a.1[0].completed,
            "objective 0 (penetrate the base enclosure) must complete through the game's own \
             End_of_level chain: {:?}",
            hual01a.1[0]
        );
        assert!(
            hual01a.1[1].primary && hual01a.1[1].completed,
            "objective 1 (re-connect the power supply) must be promoted and completed by the \
             map's own SetGoalComplete override through the EDF handle chain: {:?}",
            hual01a.1[1]
        );
        assert!(
            !outcome.travel.is_empty(),
            "the level must travel; blocked actors: {:?}",
            outcome.session.suspended
        );
        assert_eq!(outcome.final_map, "Hual01b");
        assert_eq!(outcome.travel[0].from, "Hual01a");
        assert_eq!(outcome.travel[0].to, "Hual01b");
        assert_eq!(outcome.travel[0].url, "Hual01b.unr");
        let hual01b = outcome
            .map_objectives
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Hual01b"))
            .expect("the run must load Hual01b and capture its objective states");
        assert!(
            hual01b.1.len() >= 7,
            "Hual01b must load with its own MapInfo objectives: {:?}",
            hual01b.1
        );
    }

    /// item54 route: Hual01b completed end-to-end by player input: the four generator
    /// sabotages and the `BreakableMover13` tunnel grille punch through the game's own
    /// break-by-hand chain (`XIIIWeapon.RealTraceFire` melee -> the interaction's
    /// `TargetActor`/`bCanBreak` -> `XIIIH2HAmmo.ProcessTraceHit` `TakeDamage(DTFisted)` ->
    /// `BreakableMover.Breaked` -> `TriggerEvent(self.Event)` -> the level's Dispatcher/
    /// XIIIGoalTrigger GoalNumber-99 chains -> `xidmaps.Hual01b.SetGoalComplete`; the same
    /// events dismiss the level's own comic-focus windows), the ladder-base `TouchTrigger0`
    /// (goal 1), the terrain hole onto the GR_sortie deck, the crawl room and the shaft fall
    /// into `Trigger0` -> `XIIIGoalTrigger0` (goal 6). With all four generator counter
    /// objectives complete the map's `xidmaps.Hual01b.SetGoalComplete` override promotes goal
    /// 6, so the shaft trigger completes it and `TestGoalComplete` -> `DoTravel` ->
    /// `ServerTravel` requests `Hual02.unr`. The fixture's labelled teleports cover measured
    /// impassables only (the buried entry corridor, the riverbed wall, the stalled east chain,
    /// the sealed baraque pocket, the mountain spur, the deck-to-room gap; evidence under
    /// local/re/item54b/ and local/re/item54/); no `take_control`, no `set_goal`, no weapon
    /// grant.
    #[test]
    fn opt_in_hual01b_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Hual01b".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Hual01b");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        // Tracked fixture (our own route commands; no game data).
        let route_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/hual01b_route.script");
        let script = script::Script::load(&route_path).expect("load item54 Hual01b route");
        assert!(
            script.events.iter().all(|event| {
                !matches!(
                    &event.command,
                    script::Command::TakeControl
                        | script::Command::SetGoal(_)
                        | script::Command::Weapon(_)
                )
            }),
            "the Hual01b route must not bridge control, goals or weapons"
        );
        // 300 s: the route's shaft fall lands at t~270, the game's own travel request lands
        // at t~272.9 (measured probe_z5), the host reloads Hual02 and the run ends there.
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Hual01b",
            &script,
            &resolved.params,
            &scene,
            300.0,
        )
        .expect("run Hual01b route");
        for (map, states) in &outcome.map_objectives {
            println!(
                "[hual01b route] objectives as the run left {map}: {}",
                states
                    .iter()
                    .map(|o| format!(
                        "[{}{}{}{}] {}",
                        o.index,
                        if o.primary { " P" } else { " -" },
                        if o.completed { " C" } else { " ." },
                        if o.anti_goal { " A" } else { "" },
                        o.text
                    ))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        println!(
            "[hual01b route] travel: {:?}, final map {}",
            outcome.travel, outcome.final_map
        );
        let hual01b = outcome
            .map_objectives
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Hual01b"))
            .expect("Hual01b objective states");
        assert!(
            hual01b.1.len() >= 7,
            "Hual01b MapInfo must expose its seven objectives: {:?}",
            hual01b.1
        );
        for (index, why) in [
            (0usize, "the anti-goal must be authored completed at spawn"),
            (
                1,
                "goal 1 (infiltrate the base) must complete through the ladder-base chain",
            ),
            (
                2,
                "the generator counter must complete through the sabotage chains",
            ),
            (
                3,
                "the generator counter must complete through the sabotage chains",
            ),
            (
                4,
                "the generator counter must complete through the sabotage chains",
            ),
            (
                5,
                "the generator counter must complete through the sabotage chains",
            ),
            (
                6,
                "goal 6 (reach the extraction shaft) must complete through Trigger0",
            ),
        ] {
            assert!(
                hual01b.1[index].completed,
                "objective {index} must complete: {why}: {:?}",
                hual01b.1[index]
            );
        }
        assert!(
            hual01b.1[1].primary && hual01b.1[6].primary,
            "goals 1 and 6 must be primary when completed: {:?} {:?}",
            hual01b.1[1],
            hual01b.1[6]
        );
        assert!(
            !outcome.travel.is_empty(),
            "the level must travel; blocked actors: {:?}",
            outcome.session.suspended
        );
        assert_eq!(outcome.final_map, "Hual02");
        assert_eq!(outcome.travel[0].from, "Hual01b");
        assert_eq!(outcome.travel[0].to, "Hual02");
        assert_eq!(outcome.travel[0].url, "Hual02.unr");
        let hual02 = outcome
            .map_objectives
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("Hual02"))
            .expect("the run must load Hual02 and capture its objective states");
        assert!(
            hual02.1.len() >= 5,
            "Hual02 must load with its own MapInfo objectives: {:?}",
            hual02.1
        );
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
        let script =
            script::Script::parse(include_str!("../../tests/data/plage00_goal.script")).unwrap();
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
            "[route] travel: {:?}, final map {}",
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
        // item91b: the T7 shore touch wakes the killer at ~289.6 s. Mesh primitive
        // import removes the old WallExtinguisher cylinder shielding during the prefix;
        // the player reaches this fight with 29 HP. The old truck-shore stand shoots
        // StaticMeshActor3 while he approaches and dies before he clears it. This labelled
        // diagnostic teleport stands north of his approach, with a clear firing lane.
        // Measured: five 31 HP hits at292.5..294.7 kill him; player survives with20 HP.
        // item95d: with `ULevel::FindSpot` ported, the map's script-spawned `BaseSoldier6` is
        // re-placed out of the BSP floor (`Model333`) it was authored into; the level-only
        // CheckSlice sweep (flags 0x86 exclude actors/terrain, `WorldPhysics::trace_level`) no
        // longer settles it onto the adjacent `GR_falaise03` static-mesh cliff, so the killer's
        // approach shifts and the same input leaves the player at 17 HP (was 20). The kill and
        // the player surviving are unchanged.
        // item101: the retail AI ledge rule (APawn::physWalking -> CheckForLedges) keeps the
        // map's AI walkers on their ledges instead of walking off and removing themselves, so a
        // soldier that used to fall now stays; the measured surviving fight health moves 20 -> 40
        // (re-pinned; BaseSoldier6 still dies). A change that lets the AI engage harder again
        // still fails visibly.
        // item102: re-measured on the merged tree (FindSpot + ledge rule): the surviving fight
        // health is 31 (FindSpot alone was 17, the ledge rule alone 40). BaseSoldier6 still dies.
        let script = script::Script::parse(&format!(
            "{}t=291.00 teleport 1802.0 -12700.0 1100.0\n\
             t=291.02 goto 1802.0 -12700.0\n\
             t=291.00 track BaseSoldier6\n\
             t=291.10 weapon XIII.Beretta\n\
             t=292.50 fire\nt=293.05 fire\nt=293.60 fire\nt=294.15 fire\nt=294.70 fire\n\
             t=295.25 fire\nt=295.80 fire\nt=296.35 fire\nt=296.90 fire\nt=297.45 fire\n\
             t=298.00 fire\nt=298.55 fire\nt=299.10 fire\nt=299.65 fire\nt=300.20 fire\n\
             t=300.75 fire\nt=301.30 fire\nt=301.85 fire\nt=302.40 fire\nt=302.95 fire\n\
             t=303.50 fire\nt=304.05 fire\nt=304.60 fire\nt=305.15 fire\nt=305.70 fire\n\
             t=306.25 fire\nt=306.80 fire\nt=307.35 fire\nt=307.90 fire\nt=308.45 fire\n\
             t=309.00 fire\nt=309.55 fire\nt=310.10 fire\nt=310.65 fire\nt=311.20 fire\n\
             t=311.75 fire\nt=312.30 fire\nt=312.85 fire\nt=313.40 fire\nt=313.95 fire\n\
             t=314.50 track off\n",
            plage01_killer_awake_prefix()
        ))
        .unwrap();
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            322.0,
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
        // item100j: re-pinned from 78 to the measured 88. Measured cause: the item100h
        // negative-SightCounter gate delays the shore/wandering soldiers' SeePlayer acquisition,
        // so the prefix's standing `engage_on` takes less return fire and the player reaches the
        // killer stand with 10 more hp; the killer fight itself still kills BaseSoldier6. Same
        // assertion (the measured surviving fight health), value re-measured on this tree.
        assert_eq!(
            s.player_health(),
            Some(88.0),
            "measured surviving fight health (item100j: 88 after the sight-countdown decode \
             delays the prefix soldiers' acquisition; item107d 78. The item107c prefix shoots the \
             shore soldiers with a standing `engage_on` (the retail `PickStartPoint` leaves them \
             wandering instead of holding). The item107c pin was 86, measured while the prefix's \
             last `goto` was still pending, so the diagnostic stand drifted off the teleport spot; \
             this test issues `goto 1802 -12700` after the teleport to cancel the orphaned route \
             destination and hold the intended fixed stand (measured (1802,-12700,1072.4)). \
             BaseSoldier6 still dies. Older history: item107c 86, item107 12, item104 18 on the \
             integrated tree; item102 measured 31 on its own base with FindSpot + the ledge rule.)"
        );
        let position = s.vm().location_prop(s.player).expect("player location");
        assert!(
            (position[0] - 1802.0).abs() < 1.0
                && (position[1] + 12700.0).abs() < 1.0
                && (position[2] - 1072.443).abs() < 1.0,
            "measured diagnostic shooter stand: {position:?}"
        );
        assert!(
            dead || health.is_some_and(|h| h <= 0.0),
            "BaseSoldier6 did not die: health {health:?}, dead {dead}"
        );
    }

    /// item44: collect the placed Beretta through the map pickup and verify its ammunition class
    /// and authored damage path. The teleport only positions the player on the pickup; no weapon
    /// grant command is used. The authored entry defaults equip `Fists` (`AcceptInventory` at
    /// map entry), and the authored `Weapon.ClientWeaponSet(True)` deliberately does not switch
    /// a human-controlled pawn that already holds a weapon (bytecode 0x0084), so the route
    /// selects the picked-up Beretta through SwitchWeapon(2), which waits for the old weapon's
    /// decoded Down animation before ChangedWeapon brings up the new weapon.
    /// The route wakes `BaseSoldier6` first: the map parks him in `IAController.faction`
    /// (invisible, `bCollideActors=false`) until a scripted trigger fires his controller.
    #[test]
    fn opt_in_plage01_beretta_pickup_uses_nine_mm_and_damages_soldier() {
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
        // item40e: BaseSoldier6 is in IAController `faction` stasis (no collision, not drawn)
        // until the authored chain wakes him, so after the pickup touch the route's triggers:
        // the objective-0 trigger (after Cine2's promotion), TouchTrigger8 (XIIIDispatcher3 ->
        // `tueur_conducteur`) and TouchTrigger7, then aim at his head as before.
        let script = script::Script::parse(
            "t=48.00 teleport -737.654 -511.886 1254.94\n\
             t=53.50 teleport -307.0 -1500.0 1311.0\n\
             t=58.00 teleport 1227.0 -12624.0 1100.0\n\
             t=58.50 teleport 1093.0 -13854.0 1113.0\n\
             t=60.00 teleport 1802.0 -12700.0 1100.0\n\
             t=60.00 track BaseSoldier6\n\
             t=60.05 track off\nt=60.05 pitch 5\n\
             t=60.10 switch_weapon 2\n\
             t=60.20 fire\nt=60.80 fire\nt=61.40 fire\nt=62.00 fire\nt=62.60 fire\n",
        )
        .expect("parse pickup combat route");
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            64.0,
        )
        .expect("run pickup combat route");
        let session = &outcome.session;
        let weapon = session
            .player_weapon()
            .expect("map pickup must equip the Beretta");
        let vm = session.vm();
        let down = vm
            .trace
            .iter()
            .find(|event| {
                event.time >= 60.1
                    && matches!(&event.kind,
                xiii_script::TraceKind::StateChange { actor, to: Some(state), .. }
                    if actor.starts_with("Fists") && state == "DownWeapon")
            })
            .expect("SwitchWeapon must lower the old Fists through DownWeapon");
        let ended = vm
            .trace
            .iter()
            .find(|event| {
                event.time > down.time
                    && matches!(&event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.starts_with("Fists") && function.ends_with("DownWeapon.AnimEnd"))
            })
            .expect("decoded Down clip must deliver the state-scoped AnimEnd later");
        assert!(
            vm.trace.iter().any(|event| {
                event.time == ended.time
                    && matches!(&event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target == &session.player_name && function.ends_with("ChangedWeapon"))
            }),
            "the old weapon's AnimEnd must call Pawn.ChangedWeapon"
        );
        println!(
            "[item49] Fists DownWeapon at {:.3}s -> AnimEnd/ChangedWeapon at {:.3}s",
            down.time, ended.time
        );
        let pickup = vm
            .objects
            .iter()
            .position(|object| object.name.eq_ignore_ascii_case("BerettaPick0"))
            .expect("Plage01 BerettaPick0") as xiii_script::ObjectId;
        println!(
            "[item44] BerettaPick0.InventoryType={} picked weapon.AmmoName={}",
            vm.obj_path(
                vm.get_property(pickup, "InventoryType")
                    .expect("BerettaPick0 InventoryType")
            )
            .unwrap_or_else(|| "<None>".to_owned()),
            vm.obj_path(
                vm.get_property(weapon, "AmmoName")
                    .expect("picked Beretta AmmoName")
            )
            .unwrap_or_else(|| "<None>".to_owned())
        );
        assert!(
            vm.is_a(weapon, "Beretta"),
            "expected picked Beretta, got {} ({})",
            vm.objects[weapon as usize].name,
            vm.class_path_of(vm.objects[weapon as usize].class)
                .unwrap_or_default()
        );
        let ammo_type = match vm.get_property(weapon, "AmmoType") {
            Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(ammo)))) => *ammo,
            other => panic!("picked Beretta AmmoType is not an ammo actor: {other:?}"),
        };
        let ammo_name = vm.objects[ammo_type as usize].name.clone();
        println!("[item44] picked Beretta AmmoType={ammo_name}");
        assert!(
            vm.is_a(ammo_type, "c9mmAmmo"),
            "picked Beretta's AmmoType {ammo_name} is not a c9mmAmmo"
        );
        let soldier = vm
            .find_object("BaseSoldier6")
            .expect("Plage01 BaseSoldier6");
        let health = session.actor_health(soldier);
        println!("[item44] BaseSoldier6 health after picked Beretta shots: {health:?}");
        assert!(
            health.is_some_and(|health| health < 550.0),
            "a shot from the picked Beretta must damage BaseSoldier6"
        );
    }

    /// Item47b continuation (A): with the engine's decoded actor-trace filter, a parked soldier
    /// (`IAController.faction`: `SetCollision(false,false,false)`, `SetDrawType(0)`, `bStasis`)
    /// does not block hitscan bullets — it is not in the collision hash. Two otherwise identical
    /// Plage01 runs: one fires five Beretta shots at the parked `BaseSoldier6`, one does not
    /// fire. Both must report the same `Health`. The pre-decode "extent OR bCollideActors"
    /// heuristic made the parked soldier shootable, contradicting the engine's hash gating
    /// (`ULevel::SpawnActor`/`SetActorCollision`/`FarMoveActor` all insert only while
    /// `bCollideActors` holds). The woken case is covered by the Beretta pickup test above.
    #[test]
    fn opt_in_plage01_parked_soldier_does_not_block_bullets() {
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
        let health_for = |fires: &str| -> Option<f32> {
            let script = script::Script::parse(&format!(
                "t=48.00 weapon XIII.Beretta\n\
                 t=48.20 teleport 1802.4131 -12992.034 1070.843\n\
                 t=48.20 yaw 90\nt=48.20 pitch 5\n\
                 {fires}"
            ))
            .expect("parse parked-soldier route");
            let outcome = run_script(
                &game_dir,
                "Plage01",
                &script,
                &resolved.params,
                &scene,
                52.0,
            )
            .expect("run parked-soldier route");
            let soldier = outcome
                .session
                .vm()
                .find_object("BaseSoldier6")
                .expect("Plage01 BaseSoldier6");
            outcome.session.actor_health(soldier)
        };
        let no_shots = health_for("").expect("control-run health");
        let five_shots =
            health_for("t=49.20 fire\nt=49.80 fire\nt=50.40 fire\nt=50.90 fire\nt=51.30 fire\n");
        println!(
            "[parked test] BaseSoldier6 health: no shots {no_shots:?}, five shots at the parked soldier {five_shots:?}"
        );
        assert_eq!(
            Some(no_shots),
            five_shots,
            "bullets must pass through the parked (bCollideActors=false) soldier"
        );
    }

    /// Item38 acceptance: run the authored Plage01 route fixture, then verify the VM selected a
    /// real death sequence for the named killer and left its non-looping channel at its final
    /// frame. The renderer samples that VM channel every frame, including after it becomes
    /// inactive, so the same state produces the corpse pose rather than a host-selected pose.
    #[test]
    fn opt_in_item38_plage01_route_killer_finishes_in_scripted_death_pose() {
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
        let route = script::Script::parse(include_str!("../../tests/data/plage01_route.script"))
            .expect("parse the checked-in Plage01 route fixture");
        // item30c round 8: the truck-shore route kills BaseSoldier6 at ~305 s; the death pose
        // needs a few seconds after that, and the run must stop before the route's level-end
        // travel (~402 s) because a server travel replaces the map's objects.
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &route,
            &resolved.params,
            &scene,
            360.0,
        )
        .expect("run the requested Plage01 route through the killer");
        let vm = outcome.session.vm();
        let killer = vm
            .find_object("BaseSoldier6")
            .expect("Plage01 BaseSoldier6 remains addressable as a corpse");
        assert!(
            outcome.session.actor_is_dead(killer),
            "the route must kill BaseSoldier6"
        );
        let animation = vm
            .actor_animation(killer)
            .expect("the dead pawn keeps its VM animation channels");
        println!(
            "[item38 Plage01] BaseSoldier6 animation channels: {:?}",
            animation.channels
        );
        let death = animation
            .channels
            .iter()
            .find(|c| c.sequence.to_ascii_lowercase().starts_with("death"))
            .expect("XIIIPawn.PlayDyingAnim must select a Death* sequence");
        println!(
            "[item38 Plage01] BaseSoldier6 bIsDead=true, death sequence {} frame {:.2}/{} active={}",
            death.sequence, death.frame, death.frames, death.active
        );
        assert!(!death.looping, "a corpse death sequence must not loop");
        assert!(
            !death.active && death.frame >= death.frames.saturating_sub(1) as f32,
            "the dying pose must remain at the sequence's final frame: {death:?}"
        );
    }

    /// item49b acceptance, pre-travel half: the Plage01 killer's truck key must reach the player
    /// through the game's own corpse search — the `XIIIPlayerPawn.Tick` auto-search
    /// (0x017B..0x024F) or the controller's `Grab` -> `SearchPawn` -> `Inventory.Transfer` —
    /// never a host inventory bridge. Asserts the game-path fingerprint in the VM trace
    /// (`SearchPawn` on the corpse) plus the drained corpse chain and the carried key.
    #[test]
    fn opt_in_item49b_plage01_corpse_search_runs_game_path() {
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
        let route = script::Script::parse(include_str!("../../tests/data/plage01_route.script"))
            .expect("parse the checked-in Plage01 route fixture");
        // Stop before the truck-door ending so the VM trace still covers Plage01 (a server
        // travel resets it for the next map). The round-8 truck-shore route kills BaseSoldier6
        // at ~305 s and the game's own auto-search drains the corpse as the player walks onto
        // it at ~313 s. Stop before Porte1's unlock at t=345.10 consumes the carried key,
        // so this acceptance observes the search result before the key is used.
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &route,
            &resolved.params,
            &scene,
            336.0,
        )
        .expect("run the requested Plage01 route through the killer");
        let vm = outcome.session.vm();
        let corpse = vm
            .find_object("BaseSoldier6")
            .expect("Plage01 BaseSoldier6 remains addressable as a corpse");
        assert!(
            outcome.session.actor_is_dead(corpse),
            "the route must kill BaseSoldier6"
        );
        let mut search_events = Vec::new();
        for ev in vm.trace.iter().filter(|e| e.time >= 55.0) {
            let rendered = format!("{:?}", ev.kind);
            if rendered.contains("SearchPawn")
                || rendered.contains(".Grab\"")
                || rendered.contains(".Transfer")
                || rendered.contains("AddInventory")
                || rendered.contains("DeleteInventory")
            {
                println!("[item49b corpse] t={:.3}: {rendered}", ev.time);
                if rendered.contains("SearchPawn") {
                    search_events.push(rendered);
                }
            }
        }
        assert!(
            !search_events.is_empty(),
            "the game's SearchPawn must run on the killer's corpse; no host bridge may replace it"
        );
        assert!(
            search_events
                .iter()
                .any(|e| e.to_ascii_lowercase().contains("basesoldier6")),
            "SearchPawn must target the killer's corpse: {search_events:?}"
        );
        assert_eq!(
            vm.get_property(corpse, "Inventory"),
            Some(&xiii_script::Value::Object(None)),
            "SearchPawn -> Transfer must drain the killer's chain"
        );
        assert!(
            vm.objects.iter().enumerate().any(|(i, o)| {
                let id = i as xiii_script::ObjectId;
                !o.deleted
                    && vm.is_a(id, "keys")
                    && matches!(
                        vm.get_property(id, "Owner"),
                        Some(xiii_script::Value::Object(Some(
                            xiii_script::ObjRef::Instance(owner),
                        ))) if *owner == outcome.session.player
                    )
            }),
            "the truck key must be carried by the player after the corpse search"
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
        // item40e: fire at the awake killer (see `plage01_killer_awake_prefix`). The walked
        // route's T7 shore touch wakes him at ~289.6 s; he hunts the player's position, so the
        // probe stands where the route stands - the truck shore (PathNode47) - where he settles
        // at the water's edge ~(1127,-13750) from ~300 s (measured round 8, scratch r9_route1;
        // the old southern spot is now point-blank to his approach path, scratch r8_fight1).
        // The burst is timed at the settled window so every granted-M60 round lands (the route
        // measured 45 hp/hit there); tracking stays on through the burst.
        let script = script::Script::parse(&format!(
            "{}t=291.00 teleport 643.2 -14174.5 1087.0\n\
             t=291.00 track BaseSoldier6\n\
             t=291.10 weapon XIII.M60\n\
             t=302.50 fire\nt=303.10 fire\nt=303.70 fire\nt=304.30 fire\nt=304.90 fire\n\
             t=305.50 fire\nt=306.10 fire\nt=306.70 fire\nt=307.30 fire\nt=307.90 fire\n\
             t=308.50 fire\nt=309.10 fire\n\
             t=309.60 track off\n",
            plage01_killer_awake_prefix()
        ))
        .expect("parse M60 fight script");
        let outcome = run_script(
            &game_dir,
            "Plage01",
            &script,
            &resolved.params,
            &scene,
            316.0,
        )
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
    /// `LadderVolume5` brush, enters `PHYS_Ladder` and climbs at `LadderSpeed` by looking up.
    /// The script teleports to the volume floor (the same harness bootstrap the other map demos
    /// use), faces the volume's decoded `LookDir` - `LadderVolume.PawnEnteredVolume` /
    /// `PotentialClimbWatcher.Tick` attach the pawn only while `Dot(facing, LookDir) > 0.9`
    /// (item85b) - and then holds forward while looking up (PlayerClimbing's decoded view-space
    /// acceleration). The climb is the simulation's, not a teleport.
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
        // Face the wall the volume climbs (LookDir = vector(WallDir)); the sim's yaw convention
        // is forward = (cos yaw, sin yaw).
        let yaw_deg = lv.look_dir[1].atan2(lv.look_dir[0]).to_degrees();
        let script = script::Script::parse(&format!(
            "t=0.0 teleport {cx} {cy} {start_z}\nt=0.0 yaw {yaw_deg}\nt=0.0 pitch 90\nt=0.0 forward 1\nt=6.0 forward 0\n"
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

    /// item98e opt-in corpus test of the host swim bridge: `Session::publish_head_volume` writes
    /// the live `PhysicsVolume` actor into the pawn's `HeadVolume` and fires the engine's
    /// `HeadVolumeChange` event, so `Pawn.BreathTimer` reads `HeadVolume.bWaterVolume`. Retail
    /// source: `APawn::SetZone` (`Engine.dll` 0x1038CA50) and `Pawn.HeadVolumeChange` (`Engine.u`).
    #[test]
    fn opt_in_banque01_head_volume_is_published_to_the_pawn() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let volumes = xiii_world::movement_volumes::MovementVolumes::import(&game_dir, "Banque01")
            .expect("import Banque01 volumes");
        let wv = volumes
            .volumes
            .iter()
            .find(|v| v.kind == xiii_world::movement_volumes::VolumeKind::Water)
            .expect("Banque01 places WaterVolume0");
        let name = wv.name.clone();
        let mut session = session::Session::open(&game_dir, "Banque01").expect("open Banque01");
        let player = session.player;
        // A dry head point clears the property.
        session.publish_head_volume(None);
        assert_eq!(
            session.vm().get_property(player, "HeadVolume").cloned(),
            Some(xiii_script::Value::Object(None)),
            "a dry head must publish no HeadVolume"
        );
        // The water volume actor is live and the bridge stores it on the pawn.
        assert!(
            session.vm().find_object(&name).is_some(),
            "the water volume actor {name} must be loaded in the VM"
        );
        session.publish_head_volume(Some(&name));
        let head = session.vm().get_property(player, "HeadVolume").cloned();
        let id = session.vm().find_object(&name).unwrap();
        assert_eq!(
            head,
            Some(xiii_script::Value::Object(Some(
                xiii_script::ObjRef::Instance(id)
            ))),
            "the water volume must be published as the pawn's HeadVolume"
        );
        println!("[head volume test] {name} published to HeadVolume");
        assert!(
            session.first_error().is_none(),
            "publishing the head volume must not raise: {:?}",
            session.first_error()
        );
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

    /// Opt-in corpus test (item50): a player-input route across Toits01 from the start roof to
    /// TouchTrigger10 at the pad. The route fires the game's own Touch on TT10, but goal 0 does
    /// not complete: XIII's TouchTrigger.Touch requires `self.bActif`, and TT10 is authored
    /// `bActif=false` (`bActivableParTrigger=true`, Tag `PorteDebloquee`) - it arms only when
    /// BreakableMover12 (the generator, Health 50) is destroyed, and the generator's yard is
    /// sealed against the route's input in the current sim. Goals 1 and 2 are unreachable for
    /// the independent demo-wedge reason (the CineController2 grapple demonstration can never
    /// complete, so the scene blocks forever at `wait event JonesHookEnd`). This test pins the
    /// measured state so the blockers cannot silently regress. item48 update: with
    /// `VisibleDamageableActors` implemented the route's player is killed by the tarmac's
    /// scripted bazooka fire (see the health assertion below); the fixture was authored while
    /// that native was missing and the blasts were no-ops. item62 update: the post-grapple
    /// dialogue chain is no longer a blocker - the demo's `wait event SpeechJones11T` completes
    /// through the map's own touch chain (TouchTrigger24 -> CineTrigger20 -> 'SpeechJones11T')
    /// once the route's player enters the armed trigger's zone after the demo's arm gate, and
    /// the follow-up dialogue 'dialtoits1bis' starts.
    #[test]
    fn opt_in_toits01_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Toits01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Toits01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let route_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/toits01_route.script");
        let script = script::Script::load(&route_path).expect("load item50 Toits01 route");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Toits01",
            &script,
            &resolved.params,
            &scene,
            440.0,
        )
        .expect("run Toits01 route");
        let session = &outcome.session;
        let objectives = session.objective_states();
        println!(
            "[toits01 route] objectives={objectives:?} travel={:?} final_map={}",
            outcome.travel, outcome.final_map
        );
        // item87e/item101: pin the ending-scene blocker precisely. Under the render-time clock
        // the ending scene's `CineController2` interpreter advances past action[211]
        // `wait notbeseenbyplayer` (the item86 stall). It used to park at action[227]
        // `wait event counter_finact2b` (`CineTrigger10`, InitialCounterValue 3), which needs
        // all three `act2b` courtyard soldiers (`BaseSoldier13/14/15`, Tag 'act2b') dead.
        //
        // item101: before the fix, `BaseSoldier14`'s `Errance` wander walked it off the
        // courtyard roof parapet; the VM's controller walk lacked the retail ledge rule and
        // flipped it to PHYS_Falling, and it fell ~6,434 UU to the ground level (Health 200),
        // far below the fight. item101 implements the decoded retail rule
        // (`APawn::physWalking` -> `APawn::CheckForLedges` shortens an AI walker's step at a
        // ledge; see crates/xiii-script/src/vm.rs), so the soldier now stays on the courtyard
        // surface and the route's fire kills it (Health -55, `Dying`). With all three act2b
        // soldiers dead, `counter_finact2b` fires and the ending scene advances past
        // action[227]; it now parks at action[238] `wait event counter_finact2a` (the next
        // counter), `ScriptedActionIndex` 239.
        //
        // Measured (route run18): `BaseSoldier14` dead (Health -55), `BaseSoldier17` dead
        // (Health -18), `BaseSoldier18` dead (Health -97), player health 65, goals 0/1/2
        // incomplete and goal 3 complete, no travel. The courtyard fight restored by the fix is
        // the authored 'act2b' combat. Full evidence in `local/reports/item101-pawn-falls-out.md`.
        {
            let vm = session.vm();
            let s14 = vm
                .find_object("BaseSoldier14")
                .expect("BaseSoldier14 must exist");
            assert!(
                session.actor_is_dead(s14),
                "item101 pin: BaseSoldier14 (the third counter_finact2b soldier) must die to the \
                 route's fire now that the ledge rule keeps it on the courtyard"
            );
            // item107e: re-pinned from -161 to the measured -12. Measured cause: the sniper
            // phase is closed-loop and, after `Session::fire_pressed` learned to fire the
            // selected weapon in weapon mode even when `SelectedItem` still names the Hook,
            // every closed-loop shot in this route lands. Both snipers die at their first
            // clear shot (BaseSoldier4 at 133.1 s, BaseSoldier at 134.8 s; measured
            // local/reports/item107e-toits-run5.txt) instead of the recorded 138.5/148.0, and
            // the courtyard fire kills the same act2b soldier (item101's ledge rule keeps it
            // on the courtyard). The assertion's purpose (it must die to the route's fire, not
            // walk off) is unchanged.
            assert_eq!(
                vm.get_property(s14, "Health"),
                Some(&xiii_script::Value::Int(-12)),
                "item101/item107e pin: the route's courtyard fire must kill BaseSoldier14 \
                 (measured -12 after the closed-loop sniper phase; it must be < 0, i.e. dead)"
            );
            // The scene controller is now parked at action[238] `wait event counter_finact2a`:
            // the VM's cine trace treats `ScriptedActionIndex - 1` as the action that ran/is
            // waiting (see `play::survey`).
            let cc2 = vm
                .find_object("CineController2")
                .expect("CineController2 must exist");
            assert!(
                vm.is_in_state(cc2, "PlayingSequence"),
                "the ending scene must still be running at the cutoff"
            );
            let action_index = match vm.get_property(cc2, "ScriptedActionIndex") {
                Some(xiii_script::Value::Int(v)) => *v,
                other => panic!("CineController2 ScriptedActionIndex: {other:?}"),
            };
            // item100j: the scene now advances further than the item101 pin. Measured: with the
            // sight-countdown decode the route's courtyard fire also clears the act2a phase, so
            // after `counter_finact2b` the scene runs through `counter_finact2a` and parks at
            // action[271] `wait event MortProprioChien` (ScriptedActionIndex 272; measured
            // action[269] "vf none", [270] "peerat basesoldier3", [271] "wait event
            // MortProprioChien"). The prior pin expected action[238] `wait event
            // counter_finact2a` (index 239); the wait itself is unchanged, only how far the
            // authored counter chain resolves before the cutoff.
            assert_eq!(
                action_index, 272,
                "item100j: after counter_finact2a the scene must park at action[271] \
                 (`wait event MortProprioChien`), i.e. ScriptedActionIndex 272"
            );
            let action_text = vm
                .get_property(cc2, "MyPawn")
                .and_then(|pawn| match pawn {
                    xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(p))) => Some(*p),
                    _ => None,
                })
                .and_then(|pawn| {
                    let tab = match vm.get_property(pawn, "CurrentTabActionIndex") {
                        Some(xiii_script::Value::Int(t)) => *t,
                        _ => 0,
                    };
                    let name = match tab {
                        2 => "tabActions2",
                        3 => "tabActions3",
                        _ => "tabActions",
                    };
                    let items = match vm.get_property(pawn, name) {
                        Some(xiii_script::Value::Array(items)) => items.clone(),
                        _ => return None,
                    };
                    let idx = usize::try_from(action_index.saturating_sub(1)).ok()?;
                    items.get(idx).map(|v| match v {
                        xiii_script::Value::Str(s) | xiii_script::Value::Name(s) => s.clone(),
                        other => format!("{other:?}"),
                    })
                });
            assert!(
                action_text
                    .as_deref()
                    .is_some_and(|t| t.contains("MortProprioChien")),
                "item100j pin: the parked action must be `wait event MortProprioChien`, got \
                 {action_text:?}"
            );
            println!(
                "[toits01 route] item101 scene advance: CineController2 action[{}] {action_text:?} \
                 (index {action_index}), BaseSoldier14 dead health={:?}",
                action_index - 1,
                vm.get_property(s14, "Health"),
            );
        }
        assert_eq!(outcome.final_map, "Toits01");
        assert!(
            outcome.travel.is_empty(),
            "route unexpectedly travelled: {:?}",
            outcome.travel
        );
        // item68b: with the live Level.TimeSeconds, the old tarmac wait dies at -30 HP.
        // Input engages the rocket soldier on landing. item69: PickWallAdjust unblocked
        // both bataillefinale soldiers' pursuit, which changes the tarmac fight (measured
        // local/re/item69/toits01-retiming-v1.txt): soldier18 closes and fires a rocket
        // while dying, and soldier17's pursuit flank around the Tmetaldoorhelico door line
        // puts his muzzle on the player side of the old door-line cover (DTGunned 75 at
        // 342.1, then a 74-dmg series through the generator phase killed the item68b
        // route player at -10). The route input is re-timed: kill soldier18 in the
        // landing burst, then duel soldier17 at long range from the landing spot (where
        // the same sight line serves both ways and his DTGunned fire misses; the door
        // line blocked only the player's rays), then cross. Measured (v5): soldier18
        // 150 -> 94 at317.2 -> 38 at317.4 -> dead at318.2; his in-flight rocket lands at
        // 318.817 (the one unavoidable hit: none/one-juke/two-juke dodges all took the
        // 318.8 blast for 50/45/41, v2/v3/v4); soldier17 150 -> 94 -> 38 -> dead by323.3
        // without landing a hit; player 150 -> 100 and stays there.
        // item64f: the grapple route plays the sniper section Jones's scene gates the
        // HookPoint2 descent on, so it reaches the tarmac landing teleport at 351.5
        // (34.5 s after main's 317.0) and main's stationary burst from there met a direct
        // rocket hit (two DTRocketed, 600 + 575 raw, dead at -25; measured
        // local/re/item64f/run11.txt). The burst strafes left (south) from 352.0 to 353.4:
        // soldier18 150 -> 94 -> 38 -> dead by 352.3 (main's -18), no rocket damage;
        // soldier17 150 -> 94 -> 38 by 356.4, one DTGunned hit at 356.517 (67 raw,
        // 150 -> 140), dead by 356.8; player ends at 140 (local/re/item64f/run12.txt).
        let health = session
            .player_health()
            .expect("the player must still have a Health property across the route");
        for event in &session.vm().trace {
            if let xiii_script::TraceKind::Event {
                target,
                function,
                args,
            } = &event.kind
                && target == &session.player_name
                && function.ends_with("TakeDamage")
            {
                println!("[toits01 damage] {:.3} {function} {args:?}", event.time);
            }
        }
        for name in ["BaseSoldier17", "BaseSoldier18"] {
            let vm = session.vm();
            if let Some(id) = vm.find_object(name) {
                let controller = match vm.get_property(id, "Controller") {
                    Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(c)))) => {
                        vm.state_name(*c)
                    }
                    _ => None,
                };
                println!(
                    "[toits01 tarmac] {name} health={:?} location={:?} controller_state={controller:?} dead={}",
                    vm.get_property(id, "Health"),
                    vm.vector_prop(id, "Location"),
                    session.actor_is_dead(id)
                );
            }
        }
        assert!(
            health > 0.0,
            "the route player must survive the tarmac rockets and the fight phase with normal-health soldiers, got health {health}"
        );
        // item101: the retail ledge rule keeps BaseSoldier14 in the courtyard and the route's
        // return fire now kills it (the aim fix below), so the ending scene advances past
        // counter_finact2b and the courtyard damage is shorter: the route ends at health 65
        // instead of the old 79 (item86) or the 35 measured while BaseSoldier14 survived.
        // item107c: the route now ends at health 70 (was 65). Measured cause: the pawn bump path
        // (`APawn::NotifyBump` -> `IAController.NotifyBump`) lets `BaseSoldier5` open the door `porte1`
        // it presses against (first bump 194.9 s), so it reaches the player at 202 s (hits 25+25+21+
        // 15x5 raw) and the tarmac soldier `BaseSoldier18` lands 116 raw instead of 268 (without the
        // bump path the same run ends at 65). Same kills.
        // item100j: re-pinned from 70 to the measured 145. Measured cause: the item100h
        // negative-SightCounter gate delays the courtyard and tarmac soldiers' SeePlayer
        // acquisition (BaseSoldier14 166.5 s, BaseSoldier13 173.4 s, BaseSoldier15 182.4 s,
        // BaseSoldier18 380.6 s, BaseSoldier17 382.4 s), so the returning fire lands far less
        // of the raw damage the pre-decode timing inflicted (same kills: BaseSoldier14/17/18
        // all dead, the scene still advances past counter_finact2b). The route survives; the
        // value is pinned so a further regression still fails visibly.
        // item107e: re-pinned from 145 to the measured 150. Measured cause: the closed-loop
        // sniper phase plus the `Session::fire_pressed` weapon-mode fix (closed-loop shots now
        // use the host view even when `SelectedItem` still names the Hook) kills both snipers at
        // their first clear shot (133.1/134.8 s; local/reports/item107e-toits-run5.txt) and
        // clears the courtyard/tarmac return fire before it lands, so the route ends at full
        // health. Same kills; the pin still catches a further regression.
        assert_eq!(
            health, 150.0,
            "the re-timed route must end at the measured health 150 (item100j: the sight-countdown \
             decode delays the courtyard/tarmac acquisition; item107c: door-bump path; item101: \
             the courtyard BaseSoldier14 fight the ledge fix restored; item107e: closed-loop \
             combat and the weapon-mode fire fix)"
        );
        let soldier17 = session
            .vm()
            .find_object("BaseSoldier17")
            .expect("BaseSoldier17 must exist");
        assert!(
            session.actor_is_dead(soldier17),
            "the long-range duel must kill soldier17"
        );
        // Goal 3 (Jones must not die) completes; goals 0/1/2 do not.
        for o in &objectives {
            match o.index {
                3 => assert!(o.completed, "the survival objective must complete: {o:?}"),
                0..=2 => assert!(
                    !o.completed,
                    "objective {} must stay incomplete in this fixture (measured blockers): {o:?}",
                    o.index
                ),
                _ => {}
            }
        }
        // The route fires the game's own Touch on TouchTrigger10 with the player pawn.
        let vm = session.vm();
        let tt10_touched = vm.trace.iter().any(|event| {
            matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, args }
                    if target.eq_ignore_ascii_case("TouchTrigger10")
                        && function.ends_with("TouchTrigger.Touch")
                        && args.iter().any(|a| a.contains("XIIIPlayerPawn"))
            )
        });
        assert!(
            tt10_touched,
            "the route must reach and touch TouchTrigger10 (the game's own trigger)"
        );
        // The measured blockers, pinned: TT10 stays disarmed (bActif=false) because the
        // generator BreakableMover12 that drives the PorteDebloquee chain is never destroyed;
        // goal 0's Touch fired but its XIII TouchTrigger.Touch guard requires bActif.
        let tt10 = vm
            .objects
            .iter()
            .enumerate()
            .position(|(i, o)| {
                vm.set().path(o.class).ends_with("TouchTrigger")
                    && vm
                        .get_property(i as u32, "Event")
                        .map(|v| v.to_string().contains("RenfortHelico02"))
                        .unwrap_or(false)
            })
            .expect("TouchTrigger10 (Event RenfortHelico02) must exist on Toits01");
        assert_eq!(
            vm.get_property(tt10 as u32, "bActif"),
            Some(&xiii_script::Value::Bool(false)),
            "TouchTrigger10 must stay disarmed: the generator chain never ran"
        );
        let generator = vm
            .find_object("BreakableMover12")
            .expect("the generator BreakableMover12 must exist");
        assert_eq!(
            vm.get_property(generator, "Health"),
            Some(&xiii_script::Value::Int(50)),
            "the generator must be undamaged: the route's input cannot reach it (the yard's              ForeverLocked door line blocks every walk line and the through-window shot never              lands; see local/reports/item50-toits01-route.md)"
        );
        println!(
            "[toits01 route] blockers pinned: TT10.bActif=false (generator Health 50 intact),              goal 0's Touch fired but its XIII TouchTrigger.Touch guard requires bActif"
        );
        // item53: the grapple demonstration completes. The demonstrator runs its seven-state
        // machine (Walking to the marker, the CineHook Projectile cast with the decoded
        // physProjectile -> physFalling fall-through, the Flying ascent, the retract) and its
        // final state is STA_Retract, whose Timer fired `TriggerEvent('JonesHookEnd')`.
        let demonstrator_retract = vm.trace.iter().any(|event| {
            matches!(
                &event.kind,
                xiii_script::TraceKind::StateChange { actor, to, .. }
                    if actor.eq_ignore_ascii_case("RoofGrapnleDemonstrator0")
                        && to.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("STA_Retract"))
            )
        });
        assert!(
            demonstrator_retract,
            "the grapple demonstrator must reach STA_Retract (the demo machine completes)"
        );
        // item53d: the STA_Retract Timer's posed teleport works. GetBoneCoords('X') now
        // resolves the posed root-bone position through the decoded MeshAnimation (the old
        // item19 actor-origin fallback made the SetLocation a no-op and Jones stayed on the
        // demo ledge, wedging action[117]'s move against the corniche forever). Measured:
        // Jones's yaw turns to ~0 (east, the rWantedRotation facing) during the climb; the
        // teleport moves him by the posed offset (+226.4, +26.6, +159..187) to the building
        // roof (feet z 2160), east of the corniche band (x -4570..-4509), where action[117]'s
        // movseqb walk proceeds. item87b: with the level clock and every actor's
        // `LastRenderTime` reset to 0 at load (as `UGameEngine::LoadMap` does) and the headless
        // render stand-in (`Vm::update_render_times`) the scene no longer stalls at action[211]
        // `wait notbeseenbyplayer`; it walks Jones on through the courtyard, so the FINAL Cine0
        // position is no longer the retract landing. The retract is verified by
        // `spoke_after_retract` (the [117] movseqb and forced dialogue only run once the posed
        // teleport freed him) and by the east-of-corniche x; the z pin below is the measured
        // final alley position of the now-advancing scene.
        let cine0 = vm
            .find_object("Cine0")
            .expect("the cine Jones pawn must exist");
        let cine0_loc = vm
            .vector_prop(cine0, "Location")
            .expect("Cine0 must keep a Location");
        assert!(
            cine0_loc[0] > -4509.0,
            "Cine0 must stand east of the corniche band after the retract teleport, got {cine0_loc:?}"
        );
        // item100j: re-centred from -48 to the measured 51.2. Measured cause: the sight-countdown
        // decode lets the route's courtyard fire clear the act2a phase too, so the ending scene
        // advances past `counter_finact2a` to action[271] `wait event MortProprioChien` and Jones
        // (Cine0) walks on to the measured final (168.17, 3409.91, 51.19) instead of the alley
        // floor -48; the corridor floor moved with the further advance. The check keeps the same
        // 40-UU tolerance so a scene that stops moving still fails.
        assert!(
            (cine0_loc[2] - 51.2).abs() < 40.0,
            "Cine0 ends the run on the courtyard floor once the scene advances past the \
             demo (item86 gen, item100j: further to MortProprioChien), got {cine0_loc:?}"
        );
        // The fixed [117] movseqb completed: the demo's post-move dialogue ran — the forced
        // `dial dialtoits1 11` line speaks (DialogueManager0 enters STA_HeadAnimation, the
        // voiced-line state, after the demonstrator's retract).
        let spoke_after_retract = {
            let retract_at = vm
                .trace
                .iter()
                .position(|event| {
                    matches!(
                        &event.kind,
                        xiii_script::TraceKind::StateChange { actor, to, .. }
                            if actor.eq_ignore_ascii_case("RoofGrapnleDemonstrator0")
                                && to.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("STA_Retract"))
                    )
                })
                .expect("demonstrator retract state change recorded");
            vm.trace[retract_at..].iter().any(|event| {
                matches!(
                    &event.kind,
                    xiii_script::TraceKind::StateChange { actor, to, .. }
                        if actor.eq_ignore_ascii_case("DialogueManager0")
                            && to.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("STA_HeadAnimation"))
                )
            })
        };
        assert!(
            spoke_after_retract,
            "the demo must reach action[111]/[118]: the forced dialogue line speaks after the retract"
        );
        // item62: the demo's post-grapple `wait event SpeechJones11T` completes through the
        // map's own touch chain. The '<speech>T' event is not raised by the DialogueManager,
        // the HUD or HXAudio (all decoded, local/re/item62/): it is the map's trigger chain -
        // TouchTrigger24 (r220 at -3609,1316,2263, on the grapple-landing roof) fires
        // 'SpeechJones11Ttempo' only on a Touch while its scene-gate has armed it (bActif,
        // set by the demo's [118] `event SpeechJones11` at ~81.2), and CineTrigger20
        // (InitialState BufferTrigger, Period 0.1, MaxDelay 60) forwards it as
        // 'SpeechJones11T'. item64: the route's player reaches the zone on the grapple - the
        // hook swing from HookPoint0 lands ~(-3910,858) at ~t=114.5 and the run into the zone
        // touches TT24 at ~t=121 (the start roof is ~1200 below the trigger's z band, so the
        // entry is a measured impassable on foot). Measured (probe72/74/75,
        // local/re/item62/): TouchTrigger24.Touch -> CineTrigger20.BufferTrigger.Trigger ->
        // CineController2.PlayingSequence.Trigger ('SpeechJones11T') at 85.0 s, [119]
        // satisfied; [128] `event jones11` at 86.63 releases DialogueManager0's 'jones11'
        // EndOfLine park (the manager parks on the NEXT beat's ExpectedEventBeforeNext, not
        // on '...T' - the item53b report's pinned mechanism claim was wrong and is corrected
        // in local/reports/item62-dialogue-finish.md); DialogueManager4 starts
        // 'dialtoits1bis' at 93.85.
        let tt24_touched = vm.trace.iter().any(|event| {
            matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, args }
                    if target.eq_ignore_ascii_case("TouchTrigger24")
                        && function.ends_with("TouchTrigger.Touch")
                        && args.iter().any(|a| a.contains("XIIIPlayerPawn"))
            )
        });
        assert!(
            tt24_touched,
            "the route's player must touch TouchTrigger24 (the '...T' chain's entry gate)"
        );
        let buffer_triggered = vm.trace.iter().any(|event| {
            matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.eq_ignore_ascii_case("CineTrigger20")
                        && function.ends_with("BufferTrigger.Trigger")
            )
        });
        assert!(
            buffer_triggered,
            "CineTrigger20 (BufferTrigger) must forward the touch as 'SpeechJones11T'"
        );
        let tt24_at = vm
            .trace
            .iter()
            .position(|event| {
                matches!(
                    &event.kind,
                    xiii_script::TraceKind::Event { target, function, .. }
                        if target.eq_ignore_ascii_case("TouchTrigger24")
                            && function.ends_with("TouchTrigger.Touch")
                )
            })
            .expect("TouchTrigger24.Touch recorded");
        let dm4_started_after_touch = vm.trace[tt24_at..].iter().any(|event| {
            matches!(
                &event.kind,
                xiii_script::TraceKind::StateChange { actor, to, .. }
                    if actor.eq_ignore_ascii_case("DialogueManager4")
                        && to.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("STA_PlayingDialogue"))
            )
        });
        assert!(
            dm4_started_after_touch,
            "the follow-up dialogue 'dialtoits1bis' (DialogueManager4) must start after the 'SpeechJones11T' chain fires"
        );
        // item64f: the HookPoint2 descent is gated by the level's own progression. With the
        // live clock MapInfo.Timer -> Toits01.FirstFrame clears HookPoint2.bInteractive, and
        // only Toits01.SetGoalComplete(99) restores it: XIIIGoalTrigger6 ('transmutation')
        // fires from the 'GrappinPoint' crank XIIIMoverVarTime1's FinishedOpening, and the
        // crank is triggered only by Jones's scene ([189] 'event GrappinPoint') after the
        // player stood at his side ([149] 'wait player 75') and both snipers died
        // (CineTrigger1, CounterTrigger InitialCounterValue 2 -> 'counter_finact1').
        let event_seen = |who: &str, what: &str, arg: &str| {
            vm.trace.iter().any(|event| {
                matches!(
                    &event.kind,
                    xiii_script::TraceKind::Event { target, function, args }
                        if target.eq_ignore_ascii_case(who)
                            && function.ends_with(what)
                            && args.iter().any(|a| a.contains(arg))
                )
            })
        };
        let sniper_kills = vm
            .trace
            .iter()
            .filter(|event| {
                matches!(
                    &event.kind,
                    xiii_script::TraceKind::Event { target, function, args }
                        if target.eq_ignore_ascii_case("CineTrigger1")
                            && function.ends_with("CounterTrigger.Trigger")
                            && args.iter().any(|a| a.contains("XIIIPlayerPawn"))
                )
            })
            .count();
        assert_eq!(
            sniper_kills, 2,
            "both 'Activation_Sc1_Snipers' snipers must die to the route player's fire"
        );
        assert!(
            event_seen(
                "XIIIMoverVarTime1",
                "Mover.TriggerControl.Trigger",
                "CineController2"
            ),
            "Jones's scene must trigger the 'GrappinPoint' crank"
        );
        assert!(
            event_seen(
                "XIIIGoalTrigger6",
                "XIIIGoalTrigger.Trigger",
                "XIIIMoverVarTime1"
            ),
            "the opened crank's 'transmutation' must trigger goal 99's XIIIGoalTrigger6"
        );
        assert!(
            event_seen("Toits0", "Toits01.SetGoalComplete", "99"),
            "goal 99 must complete through the map's own SetGoalComplete"
        );
        let hook_point2 = vm.find_object("HookPoint2").expect("HookPoint2 must exist");
        assert_eq!(
            vm.get_property(hook_point2, "bInteractive"),
            Some(&xiii_script::Value::Bool(true)),
            "goal 99 must re-enable HookPoint2 (Toits01.SetGoalComplete case 99)"
        );
        // The route grapples with the hook the game itself gives (the demo scene's GiveTo at
        // ~58.8); no route-granted second Hook exists.
        let player_hooks = vm
            .objects
            .iter()
            .enumerate()
            .filter(|(i, o)| {
                !o.deleted
                    && vm.set().path(o.class).ends_with(".Hook")
                    && vm.get_property(*i as u32, "Owner")
                        == Some(&xiii_script::Value::Object(Some(
                            xiii_script::ObjRef::Instance(session.player),
                        )))
            })
            .count();
        assert_eq!(
            player_hooks, 1,
            "the player must carry exactly the game-given Hook"
        );
        // Both grapples (the HookPoint0 climb/swing and the HookPoint2 street descent) and the
        // sniper section cost no health. item86: after the descent the level's own
        // DetectionVolume1 brush now fires 'act2b' (item81c), which is the authored trigger for
        // the courtyard soldiers' AttaqueScriptee; that intended combat is excluded from this
        // pin (its measured result is the Health pin above).
        let early_damage: Vec<f64> = vm
            .trace
            .iter()
            .filter(|event| {
                event.time < 172.0
                    && matches!(
                        &event.kind,
                        xiii_script::TraceKind::Event { target, function, .. }
                            if target == &session.player_name && function.ends_with("TakeDamage")
                    )
            })
            .map(|event| event.time)
            .collect();
        assert!(
            early_damage.is_empty(),
            "no player damage during the grapples, snipers and the gated HookPoint2 descent \
             (before the act2b courtyard combat), got {early_damage:?}"
        );
        // item61b: with normal-health soldiers the player survives, so the item48 kill pins are
        // gone: no GameEnded broadcast ever fires (measured: no CineController2 state change
        // after its entry into PlayingSequence; the scene is still running at the cutoff)
        // and the player controller stays in PlayerWalking. The next blocker is pinned after the
        // TouchTrigger10/generator assertions below.
        let cc2 = vm
            .find_object("CineController2")
            .expect("CineController2 must exist");
        assert!(
            vm.is_in_state(cc2, "PlayingSequence"),
            "the scene must still be running (CineController2 in PlayingSequence) when the surviving player ends the route"
        );
        let pc = vm
            .objects
            .iter()
            .enumerate()
            .position(|(_, o)| vm.set().path(o.class).ends_with("XIIIPlayerController"))
            .expect("the player controller must exist");
        assert!(
            vm.is_in_state(pc as u32, "PlayerWalking"),
            "the surviving player controller must end in PlayerWalking (no kill broadcast), got {:?}",
            vm.trace
                .iter()
                .rev()
                .find(|event| matches!(
                    &event.kind,
                    xiii_script::TraceKind::StateChange { actor, .. }
                        if actor.eq_ignore_ascii_case("XIIIPlayerController")
                ))
                .map(|e| match &e.kind {
                    xiii_script::TraceKind::StateChange { from, to, .. } => {
                        format!("{from:?} -> {to:?}")
                    }
                    _ => String::new(),
                })
        );
        // item61b continuation pins (the route's fight + generator-retry phases):
        // 1. The earlier real-clock landing engagement kills BaseSoldier18 before the
        // crossing; retain an exact damage result as well as the death assertion.
        // item69: the pursuing soldier18 fires while dying and the re-timed burst hits
        // measure 56-per-spine-hit (150 -> 94 at317.2 -> 38 at317.4 -> -18), so the
        // corpse value moves from the item68b -41 to the measured -18.
        // item64f: the grapple route's later landing (351.5) reproduces the same three
        // spine hits (150 -> 94 -> 38 -> -18 by 352.3; local/re/item64f/run12.txt).
        // item101: with the ledge rule restored the ending scene advances past
        // counter_finact2b (the courtyard BaseSoldier14 now dies), which retimes the later
        // phase; the measured soldier18 corpse value becomes Health -97 (route run18).
        // item107c: with the pawn bump path (`BaseSoldier5` opens `porte1`, see the health pin
        // below) the landing fight is re-timed again: the corpse value is -18 = 150 - 3 x 56, the
        // minimal three-hit M60 kill (the item64f pattern 150 -> 94 -> 38 -> -18); the extra rounds
        // that made it -97 no longer land after the death.
        let soldier18 = vm
            .find_object("BaseSoldier18")
            .expect("BaseSoldier18 must exist");
        // item107e: re-pinned from -105 to the measured -18. Measured cause: the landing
        // engagement is a closed-loop `engage BaseSoldier18` (linger=10) and the
        // `Session::fire_pressed` weapon-mode fix makes its shots use the host view; measured
        // local/reports/item107e-toits-run7.txt, the fight lands exactly the three M60 spine
        // hits (150 -> 94 -> 38 -> -18) before the soldier dies and the block ends. Same death.
        assert_eq!(
            vm.get_property(soldier18, "Health"),
            Some(&xiii_script::Value::Int(-18)),
            "the landing M60 engagement must reproduce the measured kill (item107e: Health -18, \
             the three-hit M60 kill after the closed-loop fix; item100j: -105)"
        );
        assert!(
            session.actor_is_dead(soldier18),
            "rocket soldier must be killed by route fire"
        );
        // 2. The generator-shot retry (t=360-368.5, from the closest road stand at
        //    (17400,1400), tracking BreakableMover12) lands nothing: the yard stays sealed
        //    (the generator Health 50 pin above). The next blocker, precisely: the yard's
        //    south wall line is sealed by BSP + the two ForeverLocked Tportet3 doors (measured
        //    soup scan: zero clear rays across the whole door band at 5 UU resolution,
        //    `local/re/item61b/los-probe*.txt`); the doors never open (PorteDecors.ForeverLocked
        //    absorbs TakeDamage and only plays a sound on PlayerTrigger); the level's own
        //    shotgun pickup (FusilPompePick1) is inside the sealed yard. So BreakableMover12
        //    can never be destroyed by route input -> 'PorteDebloquee_cine' never fires ->
        //    CineTrigger17 never forwards 'PorteDebloquee' -> TT10 stays disarmed -> goal 0's
        //    chain, the RenfortHelico02 finale, goals 1/2 and the 'finmap' travel are all
        //    unreachable, and the route ends waiting for a travel that cannot be requested.
    }

    /// item87e DIAGNOSTIC (opt-in). The item87d generator-room experiment, moved out of the
    /// main route so it cannot replace that route's positive survival pins. The fixture
    /// (`tests/data/toits01_diag_generator_room.script`) is the main route's input up to
    /// `t=386.20`, then a labelled diagnostic teleport into the generator room. Before item101
    /// the ending scene parked at action[227] `wait event counter_finact2b` and never reached
    /// action[602] `od porte6`, so the room could only be entered by that teleport. item101
    /// (the retail ledge rule) keeps `BaseSoldier14` on the courtyard where the route's fire
    /// kills it, so the scene advances past action[227]; the room's own door may now open in
    /// the unmodified scene, but the fixture still uses the labelled teleport to make the
    /// diagnostic deterministic. The carried rifle destroys `BreakableMover12`
    /// (`Health 50 -> -120`), firing `PorteDebloquee_cine` -> `PorteDebloquee` and arming
    /// `TouchTrigger10`; the game's own armed `Touch` then fires `RenfortHelico02`. item100m:
    /// the fixture is re-driven with the main route's closed-loop commands (the retail
    /// SeePlayer/SightCounter delivery shifted the recorded fights) and the arriving wave is
    /// killed by a standing engagement, so the solo route player survives to the cutoff. This
    /// test pins that measured chain, including the earlier route pins it shares with the main
    /// route. It is a labelled diagnostic, NOT a playable route claim.
    #[test]
    fn opt_in_diag_toits01_generator_room() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Toits01".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Toits01");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let route_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/toits01_diag_generator_room.script");
        let script = script::Script::load(&route_path).expect("load item87e Toits01 diag route");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Toits01",
            &script,
            &resolved.params,
            &scene,
            440.0,
        )
        .expect("run Toits01 diag route");
        let session = &outcome.session;
        let vm = session.vm();
        // The generator is destroyed only by the labelled teleport into the room; the map's own
        // `BreakableMover12.Health` drops below zero and `PorteDebloquee_cine` arms
        // `TouchTrigger10` (Event `RenfortHelico02`). item107e: re-pinned from -120 to the
        // measured -40. Measured cause: the item107e closed-loop tarmac engagements and the
        // `Session::fire_pressed` weapon-mode fix pause the script clock for a different length,
        // so the diagnostic fixture's three recorded `fire`s fall at a different phase of the
        // sniper's refire animation and only one lands (50 - 90 = -40; measured
        // local/reports/item107e-diag.txt). The generator is still destroyed and the arming
        // chain unchanged; only the residue moves.
        let generator = vm
            .find_object("BreakableMover12")
            .expect("the generator BreakableMover12 must exist");
        assert_eq!(
            vm.get_property(generator, "Health"),
            Some(&xiii_script::Value::Int(-40)),
            "diag: the room shot must destroy the generator (item100m measured -120)"
        );
        let tt10 = vm
            .objects
            .iter()
            .enumerate()
            .position(|(i, o)| {
                vm.set().path(o.class).ends_with("TouchTrigger")
                    && vm
                        .get_property(i as u32, "Event")
                        .map(|v| v.to_string().contains("RenfortHelico02"))
                        .unwrap_or(false)
            })
            .expect("TouchTrigger10 (Event RenfortHelico02) must exist on Toits01");
        assert_eq!(
            vm.get_property(tt10 as u32, "bActif"),
            Some(&xiii_script::Value::Bool(true)),
            "diag: TouchTrigger10 must be armed by the destroyed generator"
        );
        let tt10_touched = vm.trace.iter().any(|event| {
            matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, args }
                    if target.eq_ignore_ascii_case("TouchTrigger10")
                        && function.ends_with("TouchTrigger.Touch")
                        && args.iter().any(|a| a.contains("XIIIPlayerPawn"))
            )
        });
        assert!(
            tt10_touched,
            "diag: the armed TouchTrigger10 must fire its own Touch with the player pawn"
        );
        // Goal 0 (`Follow Major Jones`) completes through the armed touch; goals 1/2 and the
        // travel stay unreached (goal 3 completes).
        let objectives = session.objective_states();
        println!(
            "[toits01 diag] objectives={objectives:?} travel={:?} final_map={}",
            outcome.travel, outcome.final_map
        );
        for o in &objectives {
            match o.index {
                0 => assert!(o.completed, "diag: goal 0 must complete: {o:?}"),
                3 => assert!(
                    o.completed,
                    "diag: the survival objective must complete: {o:?}"
                ),
                1 | 2 => assert!(
                    !o.completed,
                    "diag: objective {} must stay incomplete: {o:?}",
                    o.index
                ),
                _ => {}
            }
        }
        // The scene is still parked before the intended generator door (the action-index pin
        // lives in the main route test); the armed touch's `RenfortHelico02` wave arrives at
        // the helipad.
        assert_eq!(outcome.final_map, "Toits01");
        assert!(
            outcome.travel.is_empty(),
            "diag route unexpectedly travelled: {:?}",
            outcome.travel
        );
        let s14 = vm
            .find_object("BaseSoldier14")
            .expect("BaseSoldier14 must exist");
        assert!(
            session.actor_is_dead(s14),
            "diag: BaseSoldier14 must be killed by the route's courtyard fire (item101)"
        );
        // item100m: the pre-re-drive fixture pinned the solo player's death (health -1,
        // GameEndedDeath) because its recorded burst tracked BaseSoldier5/6, which the courtyard
        // phase had already killed; the arriving RenfortHelico02 wave (BaseSoldier8/9, spawned
        // at ~394 s) then shot the player unopposed. The re-driven fixture replaces that burst
        // with a standing closed-loop engagement: measured, the wave is dead by 399.0 s and the
        // player survives to the 440 s cutoff. item107e: re-pinned from 125 to the measured 19.
        // Measured cause: the `Session::fire_pressed` weapon-mode fix makes the closing
        // engagement's shots use the host view, so the courtyard/tarmac phases clear faster and
        // the helipad wave lands more of its return fire before it dies (measured
        // local/reports/item107e-diag2.txt); the wave is still dead and the player still walks.
        let health = session
            .player_health()
            .expect("the player must have a Health property");
        assert_eq!(
            health, 19.0,
            "diag: the re-driven route must end the helipad wave and survive with the measured \
             health 19, got {health}"
        );
        assert_eq!(
            session.player_controller_state(),
            "PlayerWalking",
            "diag: the route player must still be walking at the cutoff"
        );
        println!(
            "[toits01 diag] generator Health=-40, TT10 armed+touched, goal 0 complete; the \
             RenfortHelico02 wave was killed and the route player survived (health {health}, \
             PlayerWalking); BaseSoldier14 was killed by the route (item101)"
        );
    }

    /// item101 opt-in survey (task 4): does any AI pawn end far below its authored position
    /// (fell out of the world) during the level start? Each campaign map runs with no player
    /// input for `XIII_ITEM101_SURVEY_SECS` seconds (default 60); every live `Pawn` actor
    /// further than 2000 UU below its authored `Location.z` is reported and counted. Opt-in:
    /// needs `XIII_GOG_DIR` and `XIII_ITEM101_SURVEY=1`. (The "before the fix" comparison was
    /// produced with a temporary VM gate that is not part of the final tree.)
    #[test]
    fn opt_in_item101_pawn_fall_survey() {
        let Ok(root) = std::env::var("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR for the item101 pawn-fall survey");
            return;
        };
        if std::env::var("XIII_ITEM101_SURVEY").as_deref() != Ok("1") {
            println!("SKIPPED: set XIII_ITEM101_SURVEY=1");
            return;
        }
        let root = std::path::PathBuf::from(root);
        let params = resolve_params(&root).expect("player parameters").params;
        let (maps, _) = survey::discover_campaign_order(&root).expect("campaign order");
        let dur: f32 = std::env::var("XIII_ITEM101_SURVEY_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60.0);
        println!("[fall-survey] {} maps, {dur}s each", maps.len());
        let mut fallen_maps = 0usize;
        for map in &maps {
            let opts = Options {
                map: Some(map.clone()),
                game_dir: Some(root.clone()),
                ..Default::default()
            };
            let scene = match viewer::load_scene(&opts) {
                Ok(s) => s,
                Err(e) => {
                    println!("[fall-survey] {map} IMPORT ERROR {e}");
                    continue;
                }
            };
            // Authored pawn positions, from a separate placement-inspection session.
            let authored: Vec<(String, f32)> = match session::Session::open(&root, map) {
                Ok(probe) => {
                    let vm = probe.vm();
                    (0..vm.objects.len())
                        .filter(|&i| {
                            let o = &vm.objects[i];
                            o.is_actor
                                && !o.deleted
                                && !o.name.starts_with("Default__")
                                && vm.is_a(i as u32, "Pawn")
                        })
                        .filter_map(|i| {
                            vm.vector_prop(i as u32, "Location")
                                .map(|l| (vm.objects[i].name.clone(), l[2]))
                        })
                        .collect()
                }
                Err(_) => Vec::new(),
            };
            let script = script::Script::parse("").expect("empty script");
            let outcome =
                match run_script_with_cinematic_input(&root, map, &script, &params, &scene, dur) {
                    Ok(o) => o,
                    Err(e) => {
                        println!("[fall-survey] {map} RUN ERROR {e}");
                        continue;
                    }
                };
            let vm = outcome.session.vm();
            let mut falls = Vec::new();
            let mut falling = 0usize;
            for i in 0..vm.objects.len() {
                let o = &vm.objects[i];
                if !o.is_actor
                    || o.deleted
                    || o.name.starts_with("Default__")
                    || !vm.is_a(i as u32, "Pawn")
                {
                    continue;
                }
                if vm.get_property(i as u32, "Physics") == Some(&xiii_script::Value::Byte(2)) {
                    falling += 1;
                }
                if let Some(z) = vm.vector_prop(i as u32, "Location").map(|l| l[2])
                    && let Some((_, az)) = authored.iter().find(|(n, _)| n == &o.name)
                    && z < az - 2000.0
                {
                    falls.push(format!("{} authored_z={az:.0} now_z={z:.0}", o.name));
                }
            }
            if falls.is_empty() {
                println!(
                    "[fall-survey] {map}: no pawn >2000 UU below its authored z (falling={falling})"
                );
            } else {
                fallen_maps += 1;
                println!(
                    "[fall-survey] {map}: {} pawn(s) fell out of the world: {falls:?}",
                    falls.len()
                );
            }
        }
        println!(
            "[fall-survey] DONE: {fallen_maps}/{} maps had a pawn far below its authored position",
            maps.len()
        );
    }

    /// Destroys every live `BaseSoldier*` of the session (labelled diagnostic): the Hual02
    /// underfloor and upper floor are held by sighted soldiers who kill the unarmoured player
    /// within seconds under the real level clock (measured, see the item67 report), so the
    /// systems under test (Porte11, the lift, the depot reader) are exercised with the hostile
    /// pawns removed. The `Cine*` scene pawns are KEPT: the depot Carrington scene (goals
    /// 4/5/6) runs through them. The unmodified level is pinned by the route header and the
    /// report, not by this diagnostic.
    fn remove_soldiers(session: &mut session::Session) -> usize {
        let ids: Vec<xiii_script::ObjectId> = (0..session.vm().objects.len() as u32)
            .filter(|&id| {
                let o = &session.vm().objects[id as usize];
                o.is_actor
                    && !o.deleted
                    && !o.name.starts_with("Default__")
                    && o.name.starts_with("BaseSoldier")
                    && session.vm().is_a(id, "BaseSoldier")
            })
            .collect();
        for &id in &ids {
            session.vm_mut().destroy(id).expect("destroy soldier");
        }
        ids.len()
    }

    /// item67: the Hual02 service lift is called and ridden by player input only. The game's own
    /// `LiftButton.PlayerTrigger` -> `LiftManager` states close the doors, `LiftCage` runs its
    /// key (+4632 UU in 8 s) and the player, standing on the cage, is carried up (mover base
    /// carry: `Engine.dll ULevel::MoveActor` 0x1038a770 attached-actor loop) and again down. Without
    /// the carry the cage passed through the player (measured: z stayed -4782.1).
    ///
    /// Labelled diagnostics: the player is placed at the lobby entrance (the underfloor walk to it
    /// is pinned by the route test) and the ambush soldiers are removed (see `remove_soldiers`).
    #[test]
    fn opt_in_hual02_lift_carries_the_player_up_and_down() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Hual02".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Hual02");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let mut runtime =
            open_map_runtime(&game_dir, "Hual02", &scene, &resolved.params).expect("open Hual02");
        let removed = remove_soldiers(&mut runtime.session);
        assert!(removed >= 20, "Hual02 has ~25 soldiers, removed {removed}");
        // yaw 270 = -Y: LiftButton0 is on the cage wall at (-2209, 2092), the player at y=2223.
        let script = script::Script::parse(
            "t=0.20 teleport -2206 2374 -4781
             t=1.00 goto -2209 2330 -4781
             t=4.00 goto -2209 2223 -4781
             t=7.00 yaw 270
             t=7.50 use
             t=40.00 use
             t=75.00 goto -2209 2330 -4781
",
        )
        .unwrap();
        let outcome = run_script_inner(
            &game_dir,
            "Hual02",
            &script,
            &resolved.params,
            &scene,
            80.0,
            false,
            RunnerOptions {
                initial: Some(runtime),
                ..Default::default()
            },
        )
        .expect("run lift ride");
        let z_at = |t: f32| {
            outcome
                .trace
                .iter()
                .min_by(|a, b| (a.1 - t).abs().total_cmp(&(b.1 - t).abs()))
                .map(|s| s.2[2])
                .unwrap()
        };
        let top_z = z_at(36.0);
        let mid_up = outcome
            .trace
            .iter()
            .filter(|s| (7.5..40.0).contains(&s.1))
            .map(|s| s.2[2])
            .fold(f32::MIN, f32::max);
        println!(
            "[lift ride] floor z {:.1} top z {top_z:.1} highest {mid_up:.1} final {:?}",
            z_at(7.0),
            outcome.trace.last().map(|s| s.2)
        );
        // Floor 1 -> floor 2: the cage KeyPos[1].z = +4632 above its -4725 start; the player
        // stands on the cage floor (measured -150.1 = -4782.1 + 4632).
        assert!(
            (z_at(7.0) - -4782.1).abs() < 2.0,
            "starts on floor 1: {}",
            z_at(7.0)
        );
        assert!(
            (top_z - -150.1).abs() < 2.0,
            "the cage must carry the player to floor 2 (z -150.1), got {top_z}"
        );
        assert!(
            mid_up > -4000.0,
            "the player must be carried through the shaft"
        );
        // Floor 2 -> floor 1 by the same internal button: carried back down, not left hanging.
        let vm = outcome.session.vm();
        let manager = vm.find_live_object("LiftManager0").expect("LiftManager0");
        assert_eq!(
            vm.get_property(manager, "CurrentFloor"),
            Some(&xiii_script::Value::Int(1)),
            "the second press returns the cage to floor 1"
        );
        let last = outcome.trace.last().unwrap().2;
        assert!(
            (z_at(70.0) - -4782.1).abs() < 2.0,
            "carried back down to floor 1: z {}",
            z_at(70.0)
        );
        assert!(last[2] < -4700.0, "final z {last:?}");
        let cage = vm.find_live_object("LiftCage0").unwrap();
        assert_eq!(
            vm.get_property(cage, "KeyNum"),
            Some(&xiii_script::Value::Byte(0))
        );
    }

    /// item59, re-authored by item67/item67f/item67g/item85b: Hual02 route by player input (the
    /// vent crawl, the eavesdrop hold, the underfloor with the Porte11 use, the service-lift
    /// ride by the cage's own button, the walk out through the manager-opened live LiftDoor1,
    /// the map's own surface nav chain to the depot reader, the depot reader use, Carrington's
    /// cell) with labelled teleports only for the measured movement gaps (the Locked Porte19
    /// magnetic chain whose only card pickup is sealed in the card-room petitesalle behind the
    /// 112 UU window sills, the ForeverLocked decor-door seal at x~-6198 west of Porte19, the
    /// sealed prison perimeter, the grille pin, the pinned exit stairwell). item67g removed the
    /// surface teleport: the item67f (-2854,2338) dead-end was a wrong turn off the map's nav
    /// chain, not a blocker. item67j measured the card -> reader -> Porte19 chain in
    /// `opt_in_hual02_card_room_reader_chain`; item84 decoded the entry closed (the panes are
    /// `ColType_StaticGlass` `StaticMeshActor`s no consumer shatters - `Actor.ScriptText` enum,
    /// Engine.dll's only ColCategory reader is `execActorInIterationCategory` 0x103e6f00 - and
    /// `APawn::physFalling` 0x103c37b0 never steps or mantles: slide only, `stepUp` is
    /// walking-only at `MAXSTEPHEIGHT` 35), so the sill pin stands and the depot teleport
    /// remains. item85b measured the reader-area ladder shaft (LadderVolume0, live) as a sealed
    /// dev leftover: the climb works but the column is capped/sealed at every probed height by
    /// retail collision, so it offers no route to the card room either.
    ///
    /// The eavesdrop chain now runs - the level's own order (item85b decode): the crawl's
    /// `DetectionVolume21` Touch fires `demarre_bureau` (measured t=20.5), which promotes goal 1
    /// through `XIIIGoalTrigger3` (GoalNumber 91 -> `Hual02.SetGoalComplete` ->
    /// `SetPrimaryGoal(1)`) and advances the conversation scene (Cine10 'Standwell' + Cine11
    /// 'MacCall' in the underfloor office at (-760,120)/(-779,-33); started at level start per
    /// `Cine2.CineInit.Begin` - player controller + `CineController.bInitialized` + the
    /// intro-cartoon wait, `CheckpointNumber` 1 -> `Play(5)` - and waiting at `tabActions[0]
    /// 'wait event demarre_bureau'`). The route holds in the duct and breaks the far grate
    /// (`BreakAbleMover2`, Event `grille_brisee` -> the bInterruptive alert Reactions ->
    /// `turnintosoldier`) only after the scene completes: Cine11's `event blam` completes goal 1
    /// through `XIIIGoalTrigger4` (GoalNumber 1) and fires `objectif92` -> `XIIIGoalTrigger6`
    /// (92) -> `SetPrimaryGoal(2)`; the scene ends with `playerevent XIIISaveGameTrigger1` (the
    /// map's own checkpoint, `CheckpointNumber` 1 -> 2) and both pawns `destroy self`. Goal 2
    /// then completes at the prison landing (`DetectionVolume14`'s `didact6` ->
    /// `XIIIGoalTrigger1`, GoalNumber 2, now validating). Goal 3 is promoted by `Porte154`
    /// finishing opening (`Event 'Carrington_CellDoorOpen'` -> `XIIIGoalTrigger2`, GoalNumber
    /// 93) - NOT by the depot reader, whose `PlayerTrigger` is the decoded PlaySound-only
    /// `XIIITriggers.PlayerTrigger`.
    ///
    /// Labelled diagnostic: the hostile `BaseSoldier*` pawns are removed (`remove_soldiers`,
    /// the `Cine*` scene pawns kept - the conversation needs them). Under the real level clock
    /// the unmodified level's underfloor gauntlet (BaseSoldier25/26/27 + lobby 15/28) kills the
    /// unarmoured player on the underfloor walk (measured, the item67 report); that kill is the
    /// pinned next blocker, not something this diagnostic hides.
    #[test]
    fn opt_in_hual02_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Hual02".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Hual02");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let route_path =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/hual02_route.script");
        let script = script::Script::load(&route_path).expect("load item59 Hual02 route");
        // The spec forbids goal/control bridges; the labelled Beretta grant stands in for the
        // Hual01b campaign loadout (a direct Hual02 start hands the Fists only).
        assert!(
            script.events.iter().all(|event| {
                !matches!(
                    &event.command,
                    script::Command::TakeControl | script::Command::SetGoal(_)
                )
            }),
            "the Hual02 route must not bridge control or goals"
        );
        let mut runtime =
            open_map_runtime(&game_dir, "Hual02", &scene, &resolved.params).expect("open Hual02");
        let removed = remove_soldiers(&mut runtime.session);
        assert!(
            removed >= 20,
            "the Hual02 hostile garrison is ~22 BaseSoldier pawns, removed {removed}"
        );
        // 520 s: the crawl, the eavesdrop hold (the conversation completes ~t=63, item85b), the
        // underfloor, the lift ride (~40 s, item67), the surface nav-chain walk to the depot
        // reader (item67g, ~160 s), the reader use, the cell unlock (~22 s hold), and the exit
        // plateau walk; the script's wait_travel never completes.
        let outcome = run_script_inner(
            &game_dir,
            "Hual02",
            &script,
            &resolved.params,
            &scene,
            520.0,
            false,
            RunnerOptions {
                initial: Some(runtime),
                ..Default::default()
            },
        )
        .expect("run Hual02 route");
        let session = &outcome.session;
        let objectives = session.objective_states();
        println!(
            "[hual02 route] objectives={objectives:?} travel={:?} final_map={}",
            outcome.travel, outcome.final_map
        );
        assert_eq!(outcome.final_map, "Hual02");
        assert!(
            outcome.travel.is_empty(),
            "route unexpectedly travelled: {:?}",
            outcome.travel
        );
        for o in &objectives {
            match o.index {
                // Goals 4/5/6 (the anti-goals) complete through the game's own chains in
                // every measured real-clock run.
                4..=6 => assert!(
                    o.completed,
                    "objective {} must complete through the game's chains: {o:?}",
                    o.index
                ),
                // item85b: goal 3 is promoted when the picked cell door Porte154 finishes
                // opening (Event 'Carrington_CellDoorOpen' -> XIIIGoalTrigger2, GoalNumber 93
                // -> SetPrimaryGoal(3)), not by the depot reader (XIIITriggers.PlayerTrigger is
                // decoded PlaySound-only). The escort never starts: TouchTrigger8 sits on a
                // z=63 gallery with no navigable access and the corridor's Porte151 opens only
                // from Carrington's escort moveseq, which waits forever on the
                // 'soldat_assome' raid event (measured legs 31-37).
                3 => assert!(
                    o.primary && !o.completed,
                    "objective 3 is promoted by the cell door but stays incomplete: {o:?}"
                ),
                // item85b: the conversation now completes (the route holds in the duct past the
                // scene's 'blam' before breaking the 'grille_brisee' grate), so goal 1 is
                // promoted (DV21 'demarre_bureau' -> GoalTrigger3) and completed ('Blam' ->
                // GoalTrigger4), and goal 2 is promoted ('objectif92' -> GoalTrigger6) and then
                // completed by the prison landing (DV14 'didact6' -> GoalTrigger1).
                1 | 2 => assert!(
                    o.primary && o.completed,
                    "objective {} must end promoted and completed through the game's chains: {o:?}",
                    o.index
                ),
                // The level-end trigger touches fire on the exit plateau, but the escort goals
                // never complete, so TestGoalComplete never trips and 'fin2' never completes
                // goal 0.
                0 => assert!(
                    o.primary && !o.completed,
                    "objective 0 must stay promoted and incomplete: {o:?}"
                ),
                _ => {}
            }
        }
        // The VM trace and live state for the chain pins below.
        let vm = session.vm();
        // item85b: the crawl walked into DetectionVolume21 ('demarre_bureau') after the spawn:
        // the volume's own `Detection`-state `Touch` handler runs (its t=0 entries are
        // begin-play overlap resolution), immediately followed by the GoalTrigger3 promotion
        // pinned below. (The session touch ring holds only the last 64 touches and this 520 s
        // route overflows it, so the touch is pinned from the VM trace.)
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.eq_ignore_ascii_case("DetectionVolume21")
                        && function.ends_with("Detection.Touch")
                        && event.time > 5.0
            )),
            "the crawl must touch DetectionVolume21 (the eavesdrop volume)"
        );
        // item85b: the eavesdrop chain, pinned event by event in the VM trace: the promotion
        // (demarre_bureau -> GoalTrigger3), the completion ('blam' -> GoalTrigger4), the goal-2
        // promotion ('objectif92' -> GoalTrigger6), the goal-2 completion ('didact6' ->
        // GoalTrigger1) and the goal-3 promotion (Porte154's opening event -> GoalTrigger2).
        // GoalTrigger3's event IS the crawl's DetectionVolume21 touch (demarre_bureau is that
        // volume's Event).
        for (actor, what) in [
            ("XIIIGoalTrigger3", "demarre_bureau promotion"),
            ("XIIIGoalTrigger4", "blam completion"),
            ("XIIIGoalTrigger6", "objectif92 promotion"),
            ("XIIIGoalTrigger1", "didact6 completion"),
            ("XIIIGoalTrigger2", "cell-door promotion"),
        ] {
            assert!(
                vm.trace.iter().any(|event| matches!(
                    &event.kind,
                    xiii_script::TraceKind::Event { target, function, .. }
                        if target.eq_ignore_ascii_case(actor)
                            && function.ends_with("XIIIGoalTrigger.Trigger")
                )),
                "the game's own chain must run {actor}.Trigger ({what})"
            );
        }
        // The conversation scene completed the level's own way: Cine11's
        // 'playerevent XIIISaveGameTrigger1' checkpoint ran (CheckpointNumber 1 -> 2) and both
        // scene pawns destroyed themselves ('destroy self', the scene's last action).
        let game_info = session.game_info.expect("XIIIGameInfo must exist");
        assert_eq!(
            vm.get_property(game_info, "CheckpointNumber"),
            Some(&xiii_script::Value::Int(2)),
            "the conversation's own checkpoint must run"
        );
        assert!(
            vm.find_object("Cine10").is_none() && vm.find_object("Cine11").is_none(),
            "the conversation pawns must have destroyed themselves at scene end"
        );
        // The game's own chains the route does fire, pinned by the session's touch log.
        let touched: Vec<String> = session
            .touches()
            .into_iter()
            .map(|(_, name)| name)
            .collect();
        // item67 measured: Porte154 is aimed/used, not walked into, so it is NOT in the
        // player touch log (its touch events in the probe belong to the door bumping the
        // Cine triggers, not the pawn). The game's own chains the pawn physically touches:
        for expected in ["DetectionVolume14", "XIIIGoalTrigger0", "XIIIGoalTrigger8"] {
            assert!(
                touched.iter().any(|t| t.eq_ignore_ascii_case(expected)),
                "the route must touch {expected} (the game's own chain): touches={touched:?}"
            );
        }
        // The cell unlock must have gone through the door's own Unlocked path.
        let vm = session.vm();
        let porte154 = vm.find_object("Porte154").expect("Porte154 must exist");
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.eq_ignore_ascii_case("Porte154")
                        && function.ends_with("Trigger")
            )),
            "the unlock must call Porte154.Trigger (the game's own unlock path)"
        );
        // The underfloor Porte11 corridor door must have opened through its own
        // PlayerTrigger (the route's named use, the game's Grab tail).
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.eq_ignore_ascii_case("Porte11")
                        && function.ends_with("PlayerTrigger")
            )),
            "the underfloor Porte11 must open through its own PlayerTrigger"
        );
        // item67: the service lift was ridden through the game's own chain: the cage button's
        // PlayerTrigger ran, the LiftManager states drove the cage to floor 2 and the player
        // walked out on the surface (the teleport-free z=-149 corridor legs after t=252).
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.eq_ignore_ascii_case("LiftButton0")
                        && function.ends_with("PlayerTrigger")
            )),
            "the ride must start at the cage's own LiftButton0.PlayerTrigger"
        );
        let manager = vm
            .find_object("LiftManager0")
            .expect("LiftManager0 must exist");
        assert_eq!(
            vm.get_property(manager, "CurrentFloor"),
            Some(&xiii_script::Value::Int(2)),
            "the route rode the cage to floor 2: {objectives:?}"
        );
        // item67f: Hual02's `Porte4`/`Porte20` (`XIDCine.Porte`, Tags 'porte_bas'/'porte_haut')
        // are serialised with `bDeleteMe=true`, in a trailing block of 54 destroyed exports that
        // also holds the `LiftManager0`'s stale `XIIIMover0` cage reference. No actor in the map
        // fires an event named 'porte_haut'/'porte_bas' (the strings occur only as the live
        // `LiftDoor` Tags), so no `TriggerEvent` can open `Porte20`; and the class's automatic
        // `PostBeginPlay` radius search only matches `XIDCine.LiftDoor`. The engine's
        // `AActor::IsPendingKill` (Engine.dll 0x10304b40, `AActor+0x2e & 1`) reports those exports
        // dead, so the dead `Porte20` is not part of the live level and never blocked the exit.
        assert!(
            vm.find_object("Porte20").is_none() && vm.find_object("Porte4").is_none(),
            "the destroyed Porte4/Porte20 exports must not be live level actors"
        );
        // The live floor-2 lift door is `LiftDoor1` (Tag 'porte_haut'), which the `LiftManager0`'s
        // own outer-door states open; the player walks out through the doorway.
        let lift_door = vm.find_object("LiftDoor1").expect("LiftDoor1 must exist");
        assert_eq!(
            vm.get_property(lift_door, "bOpened"),
            Some(&xiii_script::Value::Bool(true)),
            "the manager must open the live floor-2 LiftDoor1"
        );
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.eq_ignore_ascii_case("LiftDoor1") && function.ends_with("Trigger")
            )),
            "the LiftManager must trigger LiftDoor1 (the game's own outer-door path)"
        );
        // item67g measured walk: with the destroyed `Porte20` no longer part of the level, the
        // player leaves the cage north through the live `LiftDoor1` doorway and follows the
        // map's own ReachSpec/PathNode chain (measured with `xiii-tool nav` + `box-probe`)
        // south from the doorway to y~2436 and west along the y~1790 corridor, around the
        // x=-5120 BSP wall, to the depot reader. The item67f dead-end at (-2854,2338) was a
        // wrong turn off this chain (the wall is real BSP), not a blocker; the surface teleport
        // is removed.
        let walk = outcome
            .trace
            .iter()
            .filter(|s| (252.0..422.0).contains(&s.1));
        let max_y = walk.clone().map(|s| s.2[1]).fold(f32::MIN, f32::max);
        assert!(
            max_y > 2550.0,
            "the walker must leave the cage through the doorway (max corridor y {max_y})"
        );
        let min_y = walk.clone().map(|s| s.2[1]).fold(f32::MAX, f32::min);
        assert!(
            min_y < 900.0,
            "the walker must follow the nav chain all the way to the depot corridor (min y {min_y})"
        );
        // The depot reader was reached by real input: over the surface walk the player gets
        // within a few UU of the reader's volume (MagneticPassTrigger1 at -5259,803), where the
        // aimed use runs.
        let closest = walk
            .clone()
            .map(|s| ((s.2[0] + 5259.0).powi(2) + (s.2[1] - 803.0).powi(2)).sqrt())
            .fold(f32::MAX, f32::min);
        assert!(
            closest < 60.0,
            "the nav-chain walk must reach the depot reader (-5259,803): closest {closest:.1} UU"
        );
        // The depot reader was aimed and used (the game's TargetActor + bCanTrigger -> Grab),
        // but the player carries no matching card, so Grab only ran
        // `MagneticPassTrigger1.PlayerTrigger` (its sound/goal side effects) - not an item
        // activation; without the (sealed) magnetic card Porte19 must still be locked: the next
        // measured blocker, pinned instead of hidden.
        let porte19 = vm.find_object("Porte19").expect("Porte19 must exist");
        assert_eq!(
            vm.get_property(porte19, "KeyNum"),
            Some(&xiii_script::Value::Byte(0)),
            "Porte19 stays locked without the card"
        );
        assert_eq!(
            vm.get_property(porte19, "bOpened"),
            Some(&xiii_script::Value::Bool(false)),
            "Porte19 never opened"
        );
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.eq_ignore_ascii_case("MagneticPassTrigger1")
                        && function.ends_with("PlayerTrigger")
            )),
            "the reader use must run Grab's trigger branch on MagneticPassTrigger1"
        );
        let _ = porte154;
    }

    /// item67j: the Hual02 magnetic-card chain by player input, with two labelled teleport
    /// diagnostics for the measured, pinned movement gaps. The card room is a nav island whose
    /// only corridor-side openings are the two `grande_vitre` windows (`ColCategory` 3 =
    /// `ColType_StaticGlass`, engine.u `Actor.ScriptText`; item84 decoded that no retail
    /// consumer shatters them - the panes are script-less `Engine.StaticMeshActor` exports and
    /// Engine.dll reads `ColCategory` (AActor+0x1a0) only in `execActorInIterationCategory`
    /// 0x103e6f00), whose 112 UU sill (aperture z -112..16) no measured retail primitive crosses
    /// from the flat corridor floor (`APawn::physFalling` 0x103c37b0 answers a wall hit with
    /// `processHitWall` 0x103bed60 and a `TwoWallAdjust` slide, never `stepUp` - that is
    /// walking-only via `moveSmooth` 0x103b7990 at `MAXSTEPHEIGHT` 35; jump 92.8 = JumpZ 420
    /// with `PhysicsVolume.Gravity` (0,0,-950); no sill-mantle script exists in any package), so
    /// the player is placed on the pickup. The west corridor beyond the opened Porte19 ends at the
    /// `ForeverLocked` decor doors `PorteDecors18/19` (`PorteDecors.SetInitialState` ->
    /// `GotoState('ForeverLocked')`; its `PlayerTrigger` only plays the sound), so the player is
    /// placed at the depot-leg nav node the main route already walks to. Everything between the
    /// teleports is the game's own chain: the walk-over pickup puts `Hual02MagneticCard` in the
    /// inventory, the aimed use runs the controller's `Grab` (the game's `TargetActor`
    /// MagneticPassTrigger1 + `bCanTrigger` + `FindPowerUpItemNameForTrigger` matching the card),
    /// and the reader's OutEvent 'Porteentrevs' Triggers the `TriggerControl` Porte19 open. The
    /// walk west then pins at the decor seal x~-6198, which stays the route's next blocker after
    /// the card.
    #[test]
    fn opt_in_hual02_card_room_reader_chain() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Hual02".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Hual02");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let probe_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/hual02_cardroom_reader.script");
        let script = script::Script::load(&probe_path).expect("load card-room reader probe");
        assert!(
            script.events.iter().all(|event| {
                !matches!(
                    &event.command,
                    script::Command::TakeControl | script::Command::SetGoal(_)
                )
            }),
            "the probe must not bridge control or goals"
        );
        let mut runtime =
            open_map_runtime(&game_dir, "Hual02", &scene, &resolved.params).expect("open Hual02");
        let removed = remove_soldiers(&mut runtime.session);
        assert!(
            removed >= 20,
            "the Hual02 hostile garrison is ~22 BaseSoldier pawns, removed {removed}"
        );
        let outcome = run_script_inner(
            &game_dir,
            "Hual02",
            &script,
            &resolved.params,
            &scene,
            70.0,
            false,
            RunnerOptions {
                initial: Some(runtime),
                ..Default::default()
            },
        )
        .expect("run Hual02 card-room reader probe");
        let session = &outcome.session;
        let vm = session.vm();
        // The walk-over pickup put the card in the game's inventory (labelled diagnostic
        // teleport onto Hual02MagneticCardPick4 at (-3474.6,1549.5): the pickup's own Touch ->
        // HandlePickupQuery chain).
        let inventory = session.inventory_items();
        assert!(
            inventory
                .iter()
                .any(|(item, _)| item.eq_ignore_ascii_case("Hual02MagneticCard")),
            "the player must carry Hual02MagneticCard after the pickup walk: {inventory:?}"
        );
        // The aimed use at the reader ran the game's own Grab with the matching item: the
        // reader's OutEvent 'Porteentrevs' Triggered the TriggerControl Porte19 open.
        let porte19 = vm.find_object("Porte19").expect("Porte19 must exist");
        assert_eq!(
            vm.get_property(porte19, "bOpened"),
            Some(&xiii_script::Value::Bool(true)),
            "the reader's OutEvent must open Porte19"
        );
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target.eq_ignore_ascii_case("Porte19") && function.ends_with("Trigger")
            )),
            "Porte19 must open through its own Trigger event (the OutEvent chain)"
        );
        // The walked west pass: through the open Porte19 doorway (x=-5328) down the 'B' corridor
        // to the ForeverLocked decor seal, which pins the walk at x~-6198 (measured, repeated
        // samples; PorteDecors18's PlayerTrigger only plays a sound, so nothing opens it).
        let west = outcome
            .trace
            .iter()
            .filter(|s| (15.0..33.0).contains(&s.1))
            .map(|s| s.2[0])
            .collect::<Vec<_>>();
        let min_x = west.iter().fold(f32::MAX, |acc, &x| f32::min(acc, x));
        assert!(
            west.iter().any(|&x| x < -5350.0),
            "the walk must pass west through the opened Porte19 (x=-5328): min x {min_x}"
        );
        assert!(
            (-6210.0..=-6180.0).contains(&min_x),
            "the walk must pin at the PorteDecors18 seal (~-6198), not before or past it: min x {min_x}"
        );
    }

    /// Hual04c's first route pins the cabin-rope ladder obstruction after the real ride.
    /// Completed authored anti-goals are preserved; escort/helicopter travel is not claimed.
    #[test]
    fn headless_camera_projection_uses_eye_and_rotated_view() {
        let p = headless_projection(
            [100.0, 200.0, 300.0],
            60.0,
            std::f32::consts::FRAC_PI_2,
            0.0,
        );
        let center = p.project([100.0, 300.0, 360.0]).unwrap();
        assert!((center[0] - 640.0).abs() < 0.001);
        assert!((center[1] - 360.0).abs() < 0.001);
        let right = p.project([0.0, 300.0, 360.0]).unwrap();
        assert!((right[0] - 1280.0).abs() < 0.001);
        let above = p.project([100.0, 300.0, 410.0]).unwrap();
        assert!(
            above[1] < center[1],
            "world Z up must project above the crosshair"
        );
        assert!(p.project([f32::NAN, 0.0, 0.0]).is_none());
    }

    #[test]
    fn opt_in_dead_player_native_guard() {
        let Some(root) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR for death guard");
            return;
        };
        let mut s = session::Session::open(&root, "Plage01").expect("death session");
        let player = s.player;
        let pc = s.controller.expect("controller");
        let loc = s.player_location().expect("location");
        let corpse = s
            .vm()
            .find_live_object("BaseSoldier6")
            .expect("corpse regression soldier");
        let corpse_loc = s
            .vm()
            .vector_prop(corpse, "Location")
            .expect("corpse stand");
        s.vm_mut()
            .send_event(
                corpse,
                "TakeDamage",
                vec![
                    xiii_script::Value::Int(10000),
                    xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(player))),
                    xiii_script::Value::Vector(corpse_loc),
                    xiii_script::Value::Vector([0.0; 3]),
                    xiii_script::Value::Object(None),
                ],
            )
            .expect("corpse setup by damage");
        s.vm_mut()
            .send_event(
                player,
                "TakeDamage",
                vec![
                    xiii_script::Value::Int(10000),
                    xiii_script::Value::Object(None),
                    xiii_script::Value::Vector(loc),
                    xiii_script::Value::Vector([0.0; 3]),
                    xiii_script::Value::Object(None),
                ],
            )
            .expect("lethal damage");
        for _ in 0..3 {
            s.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &session::PlayerVMModes::default(),
            );
        }
        for id in [player, pc] {
            let o = &s.vm().objects[id as usize];
            println!(
                "[death guard] {} state={:?} metadata={:?} controller={:?} touching={:?} collision={:?}",
                o.name,
                s.vm().state_name(id),
                o.state.and_then(|r| s.vm().set().object(r)),
                s.vm().get_property(id, "Controller"),
                s.vm().get_property(id, "Touching"),
                s.vm().get_property(id, "bCollideActors")
            );
        }
        assert!(cinematics::input_suppressed(&s));
        assert_eq!(s.fire(1.0, 0.0), session::FireOutcome::Suppressed);
        // The native corpse can fall a few UU; compare against a zero-input death,
        // rather than replacing that physics by a frozen-location approximation.
        let mut control = session::Session::open(&root, "Plage01").expect("control death session");
        let control_player = control.player;
        let control_corpse = control
            .vm()
            .find_live_object("BaseSoldier6")
            .expect("control soldier");
        control
            .vm_mut()
            .send_event(
                control_corpse,
                "TakeDamage",
                vec![
                    xiii_script::Value::Int(10000),
                    xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(control_player))),
                    xiii_script::Value::Vector(corpse_loc),
                    xiii_script::Value::Vector([0.0; 3]),
                    xiii_script::Value::Object(None),
                ],
            )
            .expect("control corpse");
        control
            .vm_mut()
            .send_event(
                control_player,
                "TakeDamage",
                vec![
                    xiii_script::Value::Int(10000),
                    xiii_script::Value::Object(None),
                    xiii_script::Value::Vector(loc),
                    xiii_script::Value::Vector([0.0; 3]),
                    xiii_script::Value::Object(None),
                ],
            )
            .expect("control lethal damage");
        for _ in 0..3 {
            control.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &session::PlayerVMModes::default(),
            );
        }
        for _ in 0..600 {
            s.step(
                1.0 / 60.0,
                corpse_loc,
                1.0,
                [472.0, 0.0, 0.0],
                &session::PlayerVMModes::default(),
            );
            control.step(
                1.0 / 60.0,
                loc,
                0.0,
                [0.0; 3],
                &session::PlayerVMModes::default(),
            );
        }
        assert_eq!(
            s.player_location(),
            control.player_location(),
            "repeated input must not move the dead pawn relative to native corpse physics"
        );
        assert_eq!(
            s.player_physics(),
            control.player_physics(),
            "host must not replace death physics"
        );
        assert_eq!(
            s.inventory_snapshot().expect("dead inventory"),
            control.inventory_snapshot().expect("control inventory"),
            "dead host input must not auto-loot the searchable corpse"
        );
        assert!(!s.vm().trace.iter().any(|e|matches!(&e.kind, xiii_script::TraceKind::Event{function,..} if function.ends_with(".SearchPawn"))), "dead pawn must not acquire corpse inventory through host motion");
    }

    /// item97b: carry the measured Hual04a endpoint loadout into Hual04c through the game's own
    /// `ExportTravel`/`AcceptInventory` path (no grants, no health writes), clear the encounter
    /// by input, and extend Carrington's escort as far as it goes. Negative control for the
    /// direct-entry route: the same soldiers are fought with the campaign's carried rifle.
    #[test]
    fn opt_in_hual04c_travel_loadout_route() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR for Hual04a endpoint travel loadout");
            return;
        };
        let params = resolve_params(&game_dir).expect("player parameters").params;
        let source_scene = viewer::load_scene(&Options {
            game_dir: Some(game_dir.clone()),
            map: Some("Hual04a".into()),
            ..Default::default()
        })
        .expect("Hual04a scene");
        let source_input =
            script::Script::parse(include_str!("../../tests/data/hual04a_route.script"))
                .expect("source input");
        // item107c: the Hual04a route now ends with the map's own travel request; stop there so the
        // source session is still Hual04a's.
        let mut source = run_script_inner(
            &game_dir,
            "Hual04a",
            &source_input,
            &params,
            &source_scene,
            620.0,
            true,
            RunnerOptions {
                stop_on_travel: true,
                ..Default::default()
            },
        )
        .expect("measured Hual04a endpoint");
        println!(
            "[travel-loadout source] health={:?} inventory={:?} travel={:?}",
            source.session.player_health(),
            source
                .session
                .inventory_snapshot()
                .expect("source inventory"),
            source.travel
        );
        // Labelled endpoint import: Hual04a's authored travel is blocked at Carrington's
        // refused tp23. Use the production ExportTravel/AcceptInventory path, with no grants.
        let next = travel::open_next_session(
            &mut source.session,
            &game_dir,
            &travel::TravelPlan::parse("Hual04c.unr", 0, true).expect("measured destination"),
        )
        .expect("native travel import");
        println!(
            "[travel-loadout accepted] health={:?} inventory={:?}",
            next.player_health(),
            next.inventory_snapshot().expect("accepted inventory")
        );
        for (name, class) in next.inventory_items() {
            if let Some(id) = next.vm().find_live_object(&name) {
                println!(
                    "[travel-loadout group] {class} group={:?} state={:?} ammo={:?}",
                    next.vm().get_property(id, "InventoryGroup"),
                    next.vm().state_name(id),
                    next.vm().get_property(id, "AmmoType")
                );
            }
        }
        let scene = viewer::load_scene(&Options {
            game_dir: Some(game_dir.clone()),
            map: Some("Hual04c".into()),
            ..Default::default()
        })
        .expect("destination scene");
        let initial = build_map_runtime(&game_dir, "Hual04c", &scene, &params, next)
            .expect("carried runtime");
        let prefix = include_str!("../../tests/data/hual04c_route.script")
            .replace("t=124.90 wait_travel", "")
            .replace("t=113.20 switch_weapon 14", "")
            .replace(
                "t=69.00 next_item",
                "t=68.00 next_item\nt=69.00 next_item\nt=70.00 next_item",
            )
            .replace("t=100.00 goto -678 746", "t=75.10 forward 0")
            .replace("t=100.00", "t=75.10")
            .replace("t=113.", "t=83.")
            .replace("t=116.00", "t=86.00")
            .replace("t=117.00 goto -1752 596", "t=87.00 goto -2000 1100")
            .replace("t=120.00", "t=90.00");
        let input = script::Script::parse(&format!(
            "{prefix}\n{}",
            include_str!("../../tests/data/hual04c_travel_combat.script")
        ))
        .expect("carried combat input");
        let mut outcome = run_script_inner(
            &game_dir,
            "Hual04c",
            &input,
            &params,
            &scene,
            292.0,
            true,
            RunnerOptions {
                initial: Some(initial),
                stop_on_travel: true,
                ..Default::default()
            },
        )
        .expect("carried route");
        let cine = outcome
            .session
            .vm()
            .find_live_object("Cine0")
            .expect("living escort");
        let pc = outcome.session.controller.expect("player controller");
        let los = outcome
            .session
            .vm_mut()
            .send_event(
                pc,
                "LineOfSightTo",
                vec![xiii_script::Value::Object(Some(
                    xiii_script::ObjRef::Instance(cine),
                ))],
            )
            .expect("measure authored visibility gate");
        println!(
            "[escort visibility] LOS={los:?} LastRenderTime={:?} time={:.3} hidden={:?}",
            outcome.session.vm().get_property(cine, "LastRenderTime"),
            outcome.session.vm_time(),
            outcome.session.vm().get_property(cine, "bHidden")
        );
        let session = &outcome.session;
        println!(
            "[travel-loadout end] health={:?} position={:?} inventory={:?} travel={:?} goals={:?}",
            session.player_health(),
            session.player_location(),
            session.inventory_snapshot().expect("final inventory"),
            outcome.travel,
            session.objective_states()
        );
        println!(
            "[travel-loadout hands] one_hand={:?} mode={:?} weapon={:?}",
            session
                .vm()
                .get_property(session.player, "bHaveOnlyOneHandFree"),
            session
                .controller
                .and_then(|pc| session.vm().get_property(pc, "bWeaponMode")),
            session.player_weapon()
        );
        survey::print_details(&survey::extract("Hual04c", &outcome));
        for e in &session.vm().trace {
            if let xiii_script::TraceKind::Event {
                target,
                function,
                args,
            } = &e.kind
                && function.ends_with(".TakeDamage")
                && target == &session.player_name
            {
                println!(
                    "[carried damage] {:.3} {target} {function} {args:?}",
                    e.time
                );
            }
            if let xiii_script::TraceKind::Native {
                path, args, result, ..
            } = &e.kind
                && path.ends_with(".WeaponChange")
            {
                println!(
                    "[carried weaponchange] {:.3} {path} {args:?} => {result}",
                    e.time
                );
            }
        }
        for name in [
            "BaseSoldier28",
            "BaseSoldier29",
            "BaseSoldier30",
            "BaseSoldier2",
            "BaseSoldier4",
            "BaseSoldier15",
            "BaseSoldier31",
            "BaseSoldier32",
            "BaseSoldier33",
            "BaseSoldier1",
            "BaseSoldier0",
            "BaseSoldier5",
            "BaseSoldier10",
            "BaseSoldier12",
            "BaseSoldier34",
            "BaseSoldier35",
            "BaseSoldier36",
            "BaseSoldier37",
            "CineController2",
            "Cine0",
        ] {
            if let Some(id) = session.vm().find_object(name) {
                println!(
                    "[travel-loadout actor] {name} state={:?} health={:?} position={:?} action={:?} paused={:?}",
                    session.vm().state_name(id),
                    session.vm().get_property(id, "Health"),
                    session.vm().get_property(id, "Location"),
                    session.vm().get_property(id, "ScriptedActionIndex"),
                    session.vm().get_property(id, "flagsPaused")
                );
            }
        }
        if let Some(cine) = session.vm().find_live_object("Cine0") {
            if let Some(xiii_script::Value::Array(actions)) =
                session.vm().get_property(cine, "tabActions")
            {
                for (i, action) in actions.iter().enumerate().skip(20) {
                    println!("[escort action] {i}: {action:?}");
                }
            }
            if let Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(c)))) =
                session.vm().get_property(cine, "CineController")
            {
                println!(
                    "[escort live] {} action={:?} paused={:?} tag={:?}",
                    session.vm().objects[*c as usize].name,
                    session.vm().get_property(*c, "ScriptedActionIndex"),
                    session.vm().get_property(*c, "flagsPaused"),
                    session.vm().get_property(*c, "Tag")
                );
            }
        }
        assert!(
            session.first_error().is_none(),
            "{:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(
            session.player_health().is_some_and(|h| h > 0.0),
            "carried player must survive"
        );
        // item97b positive clear pin: every first-encounter soldier died to player input. A
        // `bDestroyWhenDead` soldier leaves the actor array once its death clip ends, so a
        // missing actor and a negative-health actor are both a clear; a live positive actor is
        // the failure this pins (it would mean the first encounter was left standing).
        for name in ["BaseSoldier28", "BaseSoldier29", "BaseSoldier30"] {
            if let Some(id) = session.vm().find_live_object(name) {
                assert!(
                    session.actor_health(id).is_some_and(|h| h <= 0.0),
                    "{name} must be cleared by input"
                );
            }
        }
        // item97b positive resume pin: soldier28's death delivered `move_move_02` and Carrington's
        // action27 `wait event move_move_02` resumed. The interpreter only advances its action
        // index after that event gate clears, so a large index proves the resume by input.
        let escort_actor = session
            .vm()
            .find_live_object("Cine0")
            .expect("living escort actor");
        let escort = match session.vm().get_property(escort_actor, "CineController") {
            Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(c)))) => *c,
            other => panic!("Cine0.CineController: {other:?}"),
        };
        assert_eq!(
            session.vm().objects[escort as usize].name,
            "CineController2"
        );
        let action_index = match session.vm().get_property(escort, "ScriptedActionIndex") {
            Some(xiii_script::Value::Int(i)) => *i,
            other => panic!("escort ScriptedActionIndex: {other:?}"),
        };
        assert!(
            action_index > 27,
            "Carrington's action27 wait for move_move_02 must resume and advance the escort: index {action_index}"
        );
        // item97d positive assault-clear pin: the two live `Attackattheendofthetrack`
        // `vasypepechuismort` emitters (BaseSoldier34, Beretta; BaseSoldier35, Bazook) both died
        // by input, so `CineTrigger5` (counter 2) fired `vasypepechuismorttwice` and `CineTrigger4`
        // fired `vasypepechuismortthird`. A `bDestroyWhenDead` soldier leaves the actor array once
        // its death clip ends, so a missing actor and a negative-health actor are both a clear.
        for name in ["BaseSoldier34", "BaseSoldier35"] {
            if let Some(id) = session.vm().find_live_object(name) {
                assert!(
                    session.actor_health(id).is_some_and(|h| h <= 0.0),
                    "{name} must be cleared by input"
                );
            }
        }
        // item97e: after the assault the escort runs its action89 `movseqb … m23 m25 m26 m27 m28
        // m29 m30` leg and then waits at action92 `wait player 1200`. The carried player followed
        // that leg by ordinary input (it reaches m29 at (2218,6565), 553 UU from m30 (2760,6458)),
        // so the gate cleared and the interpreter advanced past action92.
        assert!(
            action_index > 92,
            "the player must follow the escort past the action92 `wait player 1200` gate: index \
             {action_index}"
        );
        // item97e positive pin: the escort's action93 `ev Bastardsinyourback` spawns the
        // `Bastardsinyourback` ambush (BaseSoldier36/37, whose death emits `tupeuxyaller`) at the
        // m30 wait point. BaseSoldier36 died by input; its death is one of the two the
        // `CineTrigger3` (counter 2) -> `CineTrigger2` chain needs for `tupeuxyallerthird`.
        if let Some(id) = session.vm().find_live_object("BaseSoldier36") {
            assert!(
                session.actor_health(id).is_some_and(|h| h <= 0.0),
                "the first Bastardsinyourback ambusher (BaseSoldier36) must be cleared by input"
            );
        }
        // item97e: the mission must NOT be over. Replaces item97d's pin of `GameEnded` (which
        // fired only because the un-followed player fell behind `Hual04c.CheckPlayerDistance`);
        // with the player following, the escort is alive and still interpreting.
        assert_eq!(
            session.vm().state_name(escort).as_deref(),
            Some("PlayingSequence"),
            "the escort interpreter must still be running: {:?}",
            session.vm().state_name(escort)
        );
        let player_state = session.player_controller_state();
        assert!(
            !player_state.contains("GameEnded"),
            "the mission must not have ended: player controller {player_state}"
        );
        // item97e precise next-blocker pin: the escort now waits at action107
        // `wait event tupeuxyallerthird`, i.e. on a second `tupeuxyaller` death from the action93
        // ambush. BaseSoldier37 (the other live `Bastardsinyourback` ambusher) is still standing;
        // the low-health carried player can clear only the first ambusher here (it heals from the
        // first corpse's FullMedKit), so the route stops at this gate with the player alive rather
        // than exposing it to the second ambusher. It is the measured next blocker.
        if let Some(id) = session.vm().find_live_object("BaseSoldier37") {
            assert!(
                session.actor_health(id).is_some_and(|h| h > 0.0),
                "the second Bastardsinyourback ambusher is the pinned next blocker and must be \
                 standing at this measurement point"
            );
        }
        let current_action = match session.vm().get_property(escort_actor, "tabActions") {
            Some(xiii_script::Value::Array(actions)) => {
                actions.get(action_index as usize - 1).cloned()
            }
            other => panic!("Cine0.tabActions: {other:?}"),
        };
        assert_eq!(
            current_action,
            Some(xiii_script::Value::Str(
                "wait event tupeuxyallerthird".to_owned()
            )),
            "the escort's measured next blocker must be the `tupeuxyallerthird` kill-event wait"
        );
        assert!(
            outcome.travel.is_empty(),
            "the blocked escort must not travel"
        );
        let goals = session.objective_states();
        for index in [0, 2, 3] {
            assert!(
                !goals[index].completed,
                "unreached objective {index}: {goals:?}"
            );
        }
    }

    #[test]
    fn opt_in_hual04c_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Hual04c".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Hual04c");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text = include_str!("../../tests/data/hual04c_route.script");
        assert!(!text.lines().any(|line| {
            let command = line.split('#').next().unwrap_or("");
            ["teleport", "weapon ", "item ", "set_goal", "take_control"]
                .iter()
                .any(|forbidden| {
                    command
                        .split_whitespace()
                        .nth(1)
                        .is_some_and(|word| word == forbidden.trim())
                })
        }));
        let script = script::Script::parse(text).expect("parse Hual04c route");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Hual04c",
            &script,
            &resolved.params,
            &scene,
            125.0,
        )
        .expect("run Hual04c route");
        let session = &outcome.session;
        let vm = session.vm();
        println!(
            "[hual04c route] error={:?} objectives={:?} travel={:?} position={:?} health={:?}",
            session.first_error(),
            session.objective_states(),
            outcome.travel,
            session.player_location(),
            session.player_health()
        );
        survey::print_details(&survey::extract("Hual04c", &outcome));
        // These are placed live soldiers, not generated enemies or the map's pending-kill
        // copies. Soldier28's death emits move_move -> CineTrigger10 -> move_move_02;
        // soldiers29/30 instead feed the later five-death voielibre counter.
        //
        // item104: with the retail `calcVelocity` friction on steered walking (item100b) the
        // route ends while BaseSoldier30 takes `DTShotGunned` hits from the other soldiers
        // (`BaseSoldier4` at t=123.22 and 123.67, `BaseSoldier29` at t=124.02, measured with a
        // `TakeDamage` trace; that these are shots aimed at the idle player is a hypothesis):
        // friendly fire takes it from 150 to 44. With the friction term skipped (a temporary env
        // gate, measured) it stays at 150 at t=124 at the same position (-2236.7,121.4), and
        // disabling only the ledge rule does not change it. It is still alive and in
        // `AttaqueScriptee`; the other two are untouched at 150.
        // item100h: native SightCounter/probe-gated perception shifts this crossfire.
        // Measured on the merged tree: only BaseSoldier29 hits, at124.817s, raw21
        // DTShotGunned at(-2268,135,-1979); the 1.25 damage scale removes26 HP.
        // The old two BaseSoldier4 hits and earlier BaseSoldier29 hit no longer land.
        // Preserve an exact living-encounter pin: 150 - trunc(21*1.25) = 124.
        for (name, expected_health) in [
            ("BaseSoldier28", 150),
            ("BaseSoldier29", 150),
            ("BaseSoldier30", 124),
        ] {
            let soldier = vm
                .find_live_object(name)
                .expect("live first-encounter soldier");
            assert_eq!(
                vm.get_property(soldier, "Health"),
                Some(&xiii_script::Value::Int(expected_health)),
                "unresolved encounter soldier {name}"
            );
            assert_eq!(
                vm.get_property(soldier, "bIsDead"),
                Some(&xiii_script::Value::Bool(false)),
                "{name} must remain alive at the prefix blocker"
            );
            let Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(controller)))) =
                vm.get_property(soldier, "Controller")
            else {
                panic!("{name} must retain its live AI controller");
            };
            assert_eq!(
                vm.state_name(*controller).as_deref(),
                Some("AttaqueScriptee"),
                "authored first attack must remain active for {name}"
            );
            println!(
                "[hual04c encounter] {name} location={:?} health={expected_health} AI=AttaqueScriptee",
                vm.get_property(soldier, "Location")
            );
        }
        let counter = vm
            .find_live_object("CineTrigger7")
            .expect("later combat counter");
        assert_eq!(
            vm.get_property(counter, "Count"),
            Some(&xiii_script::Value::Int(5)),
            "no first-canyon soldier death has yet fed voielibre"
        );
        let buffer = vm
            .find_live_object("CineTrigger10")
            .expect("first combat event buffer");
        assert_eq!(vm.state_name(buffer).as_deref(), Some("BufferTrigger"));
        assert!(
            session.first_error().is_none(),
            "{:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(session.blocked.is_empty(), "{:?}", session.blocked);
        assert!(
            session.player_health().is_some_and(|h| h > 0.0),
            "player must survive"
        );
        assert!(
            outcome.travel.is_empty(),
            "the blocked route must not travel"
        );
        let goals = session.objective_states();
        assert_eq!(goals.len(), 7);
        for index in [0, 2, 3] {
            assert!(
                !goals[index].completed,
                "unreached objective {index}: {goals:?}"
            );
        }
        assert!(goals[0].primary);
        for index in [1, 4, 5, 6] {
            assert!(
                goals[index].anti_goal && goals[index].completed,
                "authored anti-goal {index} must hold: {goals:?}"
            );
        }
        let hud = vm.find_object("XIIIBaseHud").expect("player HUD");
        assert_eq!(
            vm.get_property(hud, "HudCartoonSFX"),
            Some(&xiii_script::Value::Bool(false)),
            "movement's early mission release must not stop the HUD slide-out"
        );
        let car = vm.find_object("Telepherique0").expect("authored cable car");
        assert_eq!(
            vm.state_name(car).as_deref(),
            Some("TheFall"),
            "the HUD's own EndCartoonEffect event must start the ride"
        );
        let carrington = vm.find_object("Cine0").expect("ground Carrington");
        assert_eq!(
            vm.get_property(carrington, "CurrentScript"),
            Some(&xiii_script::Value::Int(0)),
            "the ground escort must start through TouchTrigger4"
        );
        let escort = vm
            .find_object("CineController2")
            .expect("Carrington interpreter");
        assert_eq!(
            vm.get_property(escort, "ScriptedActionIndex"),
            Some(&xiii_script::Value::Int(27)),
            "first encounter wait event move_move_02"
        );
        assert_eq!(
            vm.get_property(escort, "flagsPaused"),
            Some(&xiii_script::Value::Int(2)),
            "retail event wait bit"
        );
        let weapon = session
            .player_weapon()
            .expect("direct-entry default melee weapon");
        assert!(
            vm.is_a(weapon, "Fists"),
            "group14 selection cannot create the missing campaign rifle"
        );
        let position = session.player_location().expect("player position");
        for (actual, expected) in position.into_iter().zip([-1772.34, 617.889, -1971.271]) {
            assert!(
                (actual - expected).abs() < 0.5,
                "ground escort/combat pin moved: {position:?}"
            );
        }
        let pawns = session.player_pawn_actors();
        assert_eq!(pawns.len(), 1, "one live player pawn");
        let pawn = pawns[0].0;
        assert_eq!(
            vm.get_property(pawn, "Physics"),
            Some(&xiii_script::Value::Byte(1)),
            "the player must land and walk to the ground escort"
        );
        let mi = session.map_info().expect("Hual04c MapInfo");
        assert_eq!(
            vm.get_property(mi, "NextMapLevelWithUnr"),
            Some(&xiii_script::Value::Str("kello01a.unr".to_owned()))
        );
        let hook = vm.find_object("Hook").expect("game-supplied grapple");
        assert_eq!(
            vm.get_property(hook, "bHooked"),
            Some(&xiii_script::Value::Bool(false)),
            "use releases the rope near the canyon floor"
        );
        let locked_name = vm
            .trace
            .iter()
            .find_map(|event| match &event.kind {
                xiii_script::TraceKind::StateChange { actor, to, .. }
                    if actor.starts_with("HookProjectile") && to.as_deref() == Some("Locked") =>
                {
                    Some(actor)
                }
                _ => None,
            })
            .expect("locked projectile trace");
        // The escape event destroys the used anchor and release reaps its projectile.
        // Pin their actual Touch and successful native placement rather than live objects.
        assert!(vm.trace.iter().any(|event| matches!(&event.kind, xiii_script::TraceKind::Event { target, function, args } if target == locked_name && function == "XIIIProjectile.Touch" && args.first().is_some_and(|arg| arg == "HookPoint1"))), "the hook must contact the authored escape anchor");
        let anchor_arg = "vect(-688.65295,60.903763,283.35825)".to_owned();
        assert!(vm.trace.iter().any(|event| matches!(&event.kind, xiii_script::TraceKind::Native { this, path, args, result, .. } if this == locked_name && path == "Actor.SetLocation" && args.first() == Some(&anchor_arg) && result == "true")), "attachment is to the authored HookPoint1");
        let releases: Vec<_> = vm
            .trace
            .iter()
            .filter_map(|event| match &event.kind {
                xiii_script::TraceKind::Event {
                    target,
                    function,
                    args,
                    ..
                } if (target == locked_name && function == "XIIIProjectile.Touch")
                    || function == "Hook.HookInUse.Release" =>
                {
                    Some(format!("{function} {args:?}"))
                }
                xiii_script::TraceKind::Log(text)
                    if text.starts_with("Releasing hook because Projectile hit wall") =>
                {
                    Some(text.clone())
                }
                _ => None,
            })
            .collect();
        assert!(
            !releases
                .iter()
                .any(|text| text.contains("BreakAbleMover44")),
            "mesh-external snow-plaque Touch must not release the attached hook: {releases:?}"
        );
        println!(
            "[hual04c next blocker] HookPoint1 lock/descent/release and ground escort reached; Carrington action27 waits for move_move_02 (first combat clear). Direct-entry rifle selection leaves Fists; campaign ranged loadout/first encounter unresolved, goals0/2/3 and kello01a travel unreached. inventory={:?}",
            session.inventory_items()
        );
    }

    /// Corpus regression for the decoded Cine2 event protocol. Registration must not pause;
    /// an unmatched early event is restored when a waiter changes Tag, including while paused.
    #[test]
    fn opt_in_cine2_event_registration_and_early_restore() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to run the Cine2 event regression");
            return;
        };
        let mut session = session::Session::open(&game_dir, "PRock01a").expect("open corpus");
        let vm = session.vm_mut();
        let class = vm.find_loaded_object("xidcine.CineController2").unwrap();
        let pawn_class = vm.find_loaded_object("xidcine.Cine2").unwrap();
        let sender = vm.spawn(class, "EventTestSender").unwrap();
        let receiver = vm.spawn(class, "EventTestReceiver").unwrap();
        let pawn = vm.spawn(pawn_class, "EventTestPawn").unwrap();
        vm.set_property(
            receiver,
            "MyPawn",
            0,
            Value::Object(Some(xiii_script::ObjRef::Instance(pawn))),
        );
        vm.set_property(pawn, "CurrentTabActionIndex", 0, Value::Int(0));
        vm.set_property(
            pawn,
            "tabActions",
            0,
            Value::Array(vec![
                Value::Str("' resume".into()),
                Value::Str("rem resumed".into()),
            ]),
        );
        for ctrl in [sender, receiver] {
            vm.set_active(ctrl, true);
            vm.goto_state(ctrl, "PlayingSequence", None).unwrap();
            for i in 0..12 {
                vm.set_property(ctrl, "nOnJump", i, Value::Int(-1));
            }
        }
        vm.send_event(
            receiver,
            "Interpret",
            vec![Value::Str("on event Item100hJump resume".into())],
        )
        .unwrap();
        assert_eq!(
            vm.get_property(receiver, "flagsPaused"),
            Some(&Value::Int(0))
        );
        assert_eq!(
            vm.get_property(receiver, "ScriptedActionIndex"),
            Some(&Value::Int(1))
        );
        // Receiver is in a different latent action when the unmatched event is raised.
        vm.set_property(receiver, "flagsPaused", 0, Value::Int(128));
        vm.send_event(
            sender,
            "TriggerEvent",
            vec![
                Value::Name("Item100hEarly".into()),
                Value::Object(None),
                Value::Object(None),
            ],
        )
        .unwrap();
        assert_eq!(
            vm.get_property(sender, "EventNamesTab"),
            Some(&Value::Name("Item100hEarly".into()))
        );
        vm.send_event(
            receiver,
            "Interpret",
            vec![Value::Str("wait event Item100hEarly".into())],
        )
        .unwrap();
        assert_eq!(
            vm.get_property(receiver, "flagsPaused"),
            Some(&Value::Int(0)),
            "early event must release the wait"
        );
        // Restore does not consume a record; deadlines affect replacement, not delivery.
        vm.send_event(
            receiver,
            "Interpret",
            vec![Value::Str("wait event Item100hEarly".into())],
        )
        .unwrap();
        assert_eq!(
            vm.get_property(receiver, "flagsPaused"),
            Some(&Value::Int(0))
        );
        vm.send_event(
            receiver,
            "Interpret",
            vec![Value::Str("wait event Item100hMissing".into())],
        )
        .unwrap();
        assert_eq!(
            vm.get_property(receiver, "flagsPaused"),
            Some(&Value::Int(2)),
            "unmatched wait must really block"
        );
    }

    /// PRock01a route (item100b): escape the Plain Rock prison by player input from the level's
    /// own PlayerStart. The route waits through the authored cell release (the escort cine opens
    /// `Porte13` at ~104.6 s), leaves the cell, then trails the lead guard `Cine3` at 180 UU while
    /// `XIDCine.EscortManager` walks the guards west through the corridor and down the stairs. The
    /// route survives the escort, approaches Cine5 to release the laundry guards, knocks out
    /// both, searches the key holder, and opens Porte9. TouchTrigger16 inside the supply
    /// cupboard completes objective 0. Returning toward the escort meets the authored
    /// TriggerToggle laundry closure (Porte3); objective 1/travel remain unreached. No teleport,
    /// goal bridge, grant or control override.
    #[test]
    fn opt_in_prock01a_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("PRock01a".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import PRock01a");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let route_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/data/prock01a_route.script");
        let script = script::Script::load(&route_path).expect("load PRock01a route");
        assert!(!script.events.iter().any(|e| matches!(
            &e.command,
            script::Command::Teleport(_)
                | script::Command::SetGoal(_)
                | script::Command::TakeControl
                | script::Command::Weapon(_)
                | script::Command::Item(_)
        )));
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "PRock01a",
            &script,
            &resolved.params,
            &scene,
            400.0,
        )
        .expect("run PRock01a route");
        let session = &outcome.session;
        let vm = session.vm();
        println!(
            "[prock01a route] objectives={:?} travel={:?} position={:?} health={:?}",
            session.objective_states(),
            outcome.travel,
            session.player_location(),
            session.player_health()
        );
        println!(
            "[prock01a route] error={:?} suspended={:?} blocked={:?}",
            session.first_error(),
            session.suspended,
            session.blocked
        );
        let baton_blows: Vec<_> = vm
            .trace
            .iter()
            .filter_map(|event| match &event.kind {
                xiii_script::TraceKind::Event {
                    target,
                    function,
                    args,
                } if target == "XIIIPlayerPawn"
                    && function.ends_with("TakeDamage")
                    && args.last().is_some_and(|d| d == "xiii.DTFisted") =>
                {
                    Some(format!("[{:.2}s] {args:?}", event.time))
                }
                _ => None,
            })
            .collect();
        assert!(
            session.first_error().is_none(),
            "{:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(session.blocked.is_empty(), "{:?}", session.blocked);
        let objectives = session.objective_states();
        assert_eq!(objectives.len(), 2, "PRock01a has two authored objectives");
        assert!(
            objectives[0].primary,
            "touching the authored CineTrigger1 must run the escort cine's `event objectif91` \
             -> `SetPrimaryGoal(0)`: {objectives:?}"
        );
        assert!(
            objectives[0].completed && objectives[1].primary,
            "crossing Porte9 into TouchTrigger16 must complete objective 0 and select 1: {objectives:?}"
        );
        assert!(
            !objectives[1].completed,
            "objective 1 must not be claimed complete: {objectives:?}"
        );
        assert!(
            objectives.iter().all(|o| !o.anti_goal),
            "PRock01a has no anti-goals: {objectives:?}"
        );
        assert!(
            outcome.travel.is_empty(),
            "the blocked route must not travel"
        );
        // The escort manager ran to its end state (its own `EndOfEscort` trigger destroys it); the
        // lead guard finished at the bottom of the stairs.
        let reached_end = vm.trace.iter().any(|event| {
            matches!(
                &event.kind,
                xiii_script::TraceKind::StateChange { actor, to, .. }
                    if actor == "EscortManager0" && to.as_deref() == Some("STA_EndOfEscort")
            )
        });
        assert!(
            reached_end,
            "the escort must run to STA_EndOfEscort (player survived the corridor and stairs)"
        );
        let cine3 = vm.find_object("Cine3").expect("escort lead guard");
        let cine3_x = vm.vector_prop(cine3, "Location").expect("guard location")[0];
        assert!(
            cine3_x < 3200.0,
            "the escort must lead west and down the stairs: Cine3 x {cine3_x}"
        );
        // The player is beaten by the escort's own scripted baton blows (XIDCine.EscortManager
        // `FirstKeeperBehavior` -> `PlayerPawn.TakeDamage(..., DTFisted)`); a careful route takes
        // few of them and survives.
        println!(
            "[prock01a baton blows] {} DTFisted TakeDamage event(s)",
            baton_blows.len()
        );
        let min_health = outcome
            .map_min_health
            .iter()
            .find(|(m, _)| m == "PRock01a")
            .map(|(_, h)| *h)
            .expect("the run must record PRock01a's minimum player Health");
        let final_pos = session.player_location().unwrap_or([0.0; 3]);
        println!(
            "[prock01a route] measured minimum player Health along PRock01a: {min_health}; final \
             position ({:.0},{:.0},{:.0}); next blocker on the return path: Porte3 \
             (verrouillage_porte, TriggerToggle)",
            final_pos[0], final_pos[1], final_pos[2]
        );
        assert!(
            min_health > 0.0,
            "the route must survive the escort, min health {min_health}"
        );
        assert!(
            outcome.trace.iter().any(|(_, _, p, ..)| p[1] < -3200.0),
            "the player must physically cross Porte9 into the cupboard"
        );
        assert!(
            final_pos[1] > -700.0 && final_pos[1] < -536.0,
            "after completing objective 0, the return attempt stops south of Porte3: {final_pos:?}"
        );
        let door = vm.find_object("Porte9").expect("Porte9");
        assert_eq!(vm.state_name(door).as_deref(), Some("PlayerTriggerToggle"));
        assert!(
            session.carried_key_for(door).is_some(),
            "the corpse key must be carried"
        );
        assert!(session.touches().iter().any(|(_, a)| a == "TouchTrigger16"));
        for name in ["Cine6", "Cine7"] {
            let guard = vm.find_object(name).expect("laundry guard");
            assert_eq!(
                vm.get_property(guard, "bIsDead"),
                Some(&Value::Bool(true)),
                "{name} must be knocked out"
            );
            assert_eq!(
                vm.get_property(guard, "bHidden"),
                Some(&Value::Bool(false)),
                "{name} must become visible"
            );
        }
        let closure = vm.find_object("Porte3").expect("laundry closure");
        assert_eq!(vm.state_name(closure).as_deref(), Some("TriggerToggle"));
    }

    /// Hual04a (item86 re-time): with automatic AI bursts, fight the canyon soldiers before they
    /// close, heal on the base medkits, read the earned card, survive the `Cine1` LastWave,
    /// install the fuse and activate the cable-car lever.
    #[test]
    fn opt_in_hual04a_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Hual04a".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Hual04a");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text = include_str!("../../tests/data/hual04a_route.script");
        assert!(!text.lines().any(|line| {
            let command = line.split('#').next().unwrap_or("");
            command.contains("set_goal")
                || command.contains("take_control")
                || command.contains("teleport")
        }));
        let script = script::Script::parse(text).expect("parse Hual04a route");
        // item107c: the route now ends with the map's own travel to Hual04c; stop the run at the
        // request so the assertions below read the Hual04a session.
        let outcome = run_script_inner(
            &game_dir,
            "Hual04a",
            &script,
            &resolved.params,
            &scene,
            620.0,
            true,
            RunnerOptions {
                stop_on_travel: true,
                ..Default::default()
            },
        )
        .expect("run Hual04a route");
        let session = &outcome.session;
        let vm = session.vm();
        println!(
            "[hual04a loadout] health={:?} inventory={:?}",
            session.player_health(),
            session
                .inventory_snapshot()
                .expect("strict measured endpoint inventory")
        );
        for name in [
            "StunningSkill",
            "bHaveStunningSkill",
            "Skills",
            "Armor",
            "Armour",
        ] {
            println!(
                "[hual04a skill] {name}={:?}",
                vm.get_property(session.player, name)
            );
        }
        let mi = session.map_info().expect("Hual04a MapInfo");
        let error = session.first_error().map(ToString::to_string);
        println!(
            "[hual04a route] error={error:?} objectives={:?} eNbVivants={:?} travel={:?} position={:?}",
            session.objective_states(),
            vm.get_property(mi, "eNbVivants"),
            outcome.travel,
            session.player_location()
        );
        for name in [
            "BaseSoldier1",
            "BaseSoldier5",
            "BaseSoldier10",
            "BaseSoldier12",
            "BaseSoldier0",
            "BaseSoldier4",
        ] {
            // A killed soldier's corpse may finish Dying and be destroyed before the route
            // ends; the dead count is asserted through `eNbVivants` below.
            let Some(id) = vm.find_object(name) else {
                println!("[hual04a soldier] {name} destroyed");
                continue;
            };
            println!(
                "[hual04a soldier] {name} health={:?} location={:?} dead={:?}",
                vm.get_property(id, "Health"),
                vm.get_property(id, "Location"),
                vm.get_property(id, "bIsDead")
            );
        }
        assert!(error.is_none(), "unexpected blocker: {error:?}");
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        // item95: with `ULevel::FindSpot` ported, the post-lever cine's `tp 23` places Carrington
        // on the platform and the cine advances past `wait player 128`, spawning the second wave
        // (`Wave01`/`Wave02`). The route kills the charging shotgun soldier `BaseSoldier7`
        // (head-band fire from t=520.6). item95d: the collision changes since item95 (item91's
        // mesh/mover sweeps) push the lane clear only at ~522.5 s, so the original 0.2 s fire
        // cadence left the kill one round late (the dying soldier landed its shotgun burst); the
        // route now fires every 0.1 s and kills it by 523.4, before it fires, surviving at 10 hp.
        // The other soldier `BaseSoldier6` holds position in its controller's `Tenir` state
        // at `(-18,-710,102)` and never engages, so its `RushFinal` event never fires and the cine
        // stays at `wait event RushFinalCine` (see the pinned blocker below). This is the honest
        // next blocker, not a completed level.
        assert!(
            session.player_health().is_some_and(|h| h > 0.0),
            "player must survive"
        );
        let goals = session.objective_states();
        // The map authors six `Objectif` entries. Completing goal 4 (the lever) runs
        // `Hual04a.SetGoalComplete(94)`, which calls `SetPrimaryGoal(6)`; `Objectif` is a dynamic
        // array and the UE2 dynamic-array write past the end grows it (item90
        // `execDynArrayElement`), so a seventh zeroed (empty, primary) entry appears. Measured on
        // this route: 7 entries, index 6 empty.
        assert_eq!(goals.len(), 7);
        // The anti-goal is authored completed and must hold on this route.
        assert!(
            goals[1].anti_goal && goals[1].primary && goals[1].completed,
            "the primary anti-goal must stay completed at spawn: {:?}",
            goals[1]
        );
        let carrington = vm.find_object("Cine1").expect("Carrington's authored pawn");
        // item94: after the lever the player walks back through `Porte12` (960,720,128) and around
        // the station's north side down to the boarding platform below the station floor (the same
        // path a collision-world BFS of the real walker finds; the direct line south is walled).
        let player_loc = session.player_location();
        println!(
            "[hual04a item94] player={player_loc:?} carrington={:?} telepherique={:?}",
            vm.get_property(carrington, "Location"),
            vm.find_object("Telepherique0").map(|t| vm.state_name(t)),
        );
        // The post-lever cine: `on event TeleOn label00` (tabActions[39]) jumps Carrington's
        // sequence to tabActions[52]; action 54 `tp 23` calls
        // `CineController2.TeleportTo(PositionInfo23)` -> `Actor.SetLocation`. The cine then waits
        // at action 61 `wait player 128` (action_index 62, flagsPaused bit 1).
        let (cine_action_index, cine_action) = match vm.get_property(carrington, "CineController") {
            Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(c)))) => {
                let c = *c;
                let idx = match vm.get_property(c, "ScriptedActionIndex") {
                    Some(xiii_script::Value::Int(i)) => *i,
                    _ => -1,
                };
                let list = match vm.get_property(carrington, "tabActions") {
                    Some(xiii_script::Value::Array(a)) => a.clone(),
                    _ => Vec::new(),
                };
                let cur = list.get(idx.saturating_sub(1) as usize).map(|v| match v {
                    xiii_script::Value::Str(s) | xiii_script::Value::Name(s) => s.clone(),
                    o => format!("{o:?}"),
                });
                (idx, cur)
            }
            _ => (-1, None),
        };
        println!("[hual04a item94] cine action_index={cine_action_index} current={cine_action:?}");
        let _p = player_loc.expect("player location");
        // item107c: the route no longer ends on the platform: after the second wave it walks to
        // `DetectionVolume10` and the map travels. The old end-position pin therefore reads the
        // trace: the walked route must have stood on the boarding platform (PositionInfo23).
        assert!(
            outcome.trace.iter().any(|(_, _, p, _)| {
                (p[0] - 945.8).abs() < 60.0 && (p[1] - 393.75).abs() < 60.0 && p[2] < -50.0
            }),
            "item94: the walked route must reach the boarding platform at PositionInfo23 (945.8,393.8,-105)"
        );
        // item95: the ported `ULevel::FindSpot` adjusts the cine's `tp 23` destination into a free
        // spot (`z: -105 -> -90`), so Carrington reaches `PositionInfo23`, the cine advances past
        // `wait player 128` (index 62), fires `Event Wave01 Wave02` and parks at
        // `wait event RushFinalCine` (index 72), which needs both second-wave generators to fire
        // `RushFinal`. `xiii-tool` disasm: `CineController2.TeleportTo` @120418,
        // `ULevel::FarMoveActor` 0x1038a440 calls `ULevel::FindSpot` 0x1038a080.
        // item107c: BaseSoldier6 now reaches the player (see the goal 5 pins below), so the cine
        // does not park at `wait event RushFinalCine` (index 72) any more: it runs past it.
        assert!(
            cine_action_index > 72,
            "item107c: the post-lever cine must advance past `wait event RushFinalCine` once both second-wave soldiers are dead: {cine_action_index} {cine_action:?}"
        );
        let cl = match vm.get_property(carrington, "Location") {
            Some(xiii_script::Value::Vector(v)) => *v,
            other => panic!("Cine1 location {other:?}"),
        };
        // The `tp 23` placement put Carrington at `PositionInfo23` (found spot `(945.8,393.8,-90)`,
        // measured by the item95 world-placement probe); the cine's later movement carries him
        // away again, so the observable proof is that he left `PositionInfo18`
        // `(-1529.6,-399.4,87.3)`, which the `wait player 128` gate above also requires.
        assert!(
            (cl[0] - -1529.57).abs() > 100.0 || (cl[1] - -399.44).abs() > 100.0,
            "item95: the `tp 23` placement must move Carrington off PositionInfo18: {cl:?}"
        );
        // Retail `AActor::IsOverlapping` (Engine.dll @0x10352bd0) uses the brush primitive for a
        // volume brush, so `Degage` (DetectionVolume4) and `dernierpoint` (DetectionVolume5) now
        // fire on the walked route. The old class-cylinder test missed both (DV4's cylinder is
        // ~867 UU off the path; DV5's cylinder top z=158 is below the canyon floor), so the map
        // previously fell into `FinDeMission` at the fuse. `Degage` -> `Cine1.Play(2)` now starts
        // Carrington's sequence.
        let departure = vm
            .find_object("DetectionVolume4")
            .expect("Carrington start volume");
        let last_point = vm
            .find_object("DetectionVolume5")
            .expect("dernierpoint volume");
        assert_eq!(
            vm.get_property(departure, "bActivated"),
            Some(&xiii_script::Value::Bool(true)),
            "DetectionVolume4 must fire 'Degage' through its brush primitive"
        );
        assert_eq!(
            vm.get_property(last_point, "bActivated"),
            Some(&xiii_script::Value::Bool(true)),
            "DetectionVolume5 must fire 'dernierpoint' through its brush primitive"
        );
        assert_eq!(vm.state_name(carrington).as_deref(), Some("CineInit"));
        assert_eq!(
            vm.get_property(carrington, "CurrentScript"),
            Some(&xiii_script::Value::Int(0)),
            "the delivered Degage cue started Cine1.Play(2)"
        );
        // item92: with the electrical-box door `Porte21` opened and the view aimed at the exposed
        // fuse socket `XIIIGoalTrigger19` (`track`), the game's own `ReturnTrace` targets the
        // trigger, `bCanTrigger` is true and the carried fuse's `EventItem.UseMe` triggers goal 3.
        // The `XIIIMover1` lever then completes goal 4 (its `PlayerTriggerToggle` open fires the
        // map's `SetGoalComplete(94)`). Goals 3/4 are claimed; goal 5 / the Hual04c travel are the
        // route's next leg (recorded below, see the report).
        assert_eq!(
            vm.get_property(mi, "eNbVivants"),
            Some(&xiii_script::Value::Int(1)),
            "the re-timed route completes the six-kill death-event chain (all six soldiers dead)"
        );
        assert!(
            goals[2].completed,
            "goal 2 (find the fuse) must complete on this route: {goals:?}"
        );
        assert!(
            goals[3].completed,
            "goal 3 (install the fuse) must complete on this route: {goals:?}"
        );
        assert!(
            goals[4].completed,
            "goal 4 (activate the cable-car) must complete on this route: {goals:?}"
        );
        // item107c: goal 5 and the Hual04c travel are reached. With the retail `APawn::NotifyBump`
        // path (Engine.dll 0x10387d80: `Controller.NotifyBump` first) `BaseSoldier6` presses against
        // `porte10`, `IAController.NotifyBump` opens it and the soldier attacks the platform; the
        // route shoots it (standing `engage_on`), its `RushFinal` releases `wait event RushFinalCine`
        // (tabActions[71], `CineTrigger1` counter 2), the cine runs to `event LaFinDeLaMap`
        // (tabActions[95]), which triggers `DetectionVolume10` (Event `finmap`); the route walks into
        // that volume, goal 5 completes and the map requests Hual04c (measured vm_time 571.6 s).
        assert!(
            goals[5].completed,
            "item107c: goal 5 (rejoin Carrington) must complete on this route: {goals:?}"
        );
        assert_eq!(
            outcome.travel.len(),
            1,
            "item107c: the route ends with the map's own travel: {:?}",
            outcome.travel
        );
        assert_eq!(outcome.travel[0].from, "Hual04a");
        assert_eq!(outcome.travel[0].to, "Hual04c");
        // The LastWave generators each spawned one reinforcement; all three are dead (killed by
        // input). The post-lever second wave spawns two more soldiers via `GenNMI6`/`GenNMI7`
        // (`Bases_Event`/soldier `Event` `RushFinal`): `BaseSoldier7` is dead (killed by input on
        // this route) and `BaseSoldier6` is still alive (holding). The generators may be cleaned
        // up after their wave dies, so count the spawned soldiers by their `GenEnnemi`
        // back-reference instead.
        let mut last_wave_dead = 0;
        let mut second_wave_dead = 0;
        let mut second_wave_alive = 0;
        for (i, o) in vm.objects.iter().enumerate() {
            if !o.is_actor || o.deleted || o.name.starts_with("Default__") {
                continue;
            }
            let id = i as xiii_script::ObjectId;
            if !vm.is_a(id, "basesoldier") {
                continue;
            }
            let gen_id = match vm.get_property(id, "GenEnnemi") {
                Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(g)))) => *g,
                _ => continue,
            };
            let gen_name = vm.objects[gen_id as usize].name.clone();
            let dead = vm.get_property(id, "bIsDead") == Some(&xiii_script::Value::Bool(true));
            // The post-lever second wave is spawned by `GenNMI6`/`GenNMI7`; the `LastWave`
            // reinforcements by the authored `GenNMI1`/`gennmi2`/`gennmi3` (`SoldiersEnRenforts`).
            if matches!(gen_name.as_str(), "GenNMI6" | "GenNMI7") {
                if dead {
                    second_wave_dead += 1;
                } else {
                    second_wave_alive += 1;
                }
            } else if dead {
                last_wave_dead += 1;
            }
        }
        assert_eq!(
            last_wave_dead, 3,
            "the three Cine1 LastWave reinforcements must have spawned and died"
        );
        // item107c: both second-wave soldiers die by input: `BaseSoldier7` (the charging shotgun
        // soldier) and `BaseSoldier6`, which now comes down its attack path, opens `porte10`
        // through the bump path and is shot by the standing engagement.
        assert_eq!(
            second_wave_dead, 2,
            "item107c: both second-wave soldiers (BaseSoldier7, BaseSoldier6) must be dead"
        );
        assert_eq!(
            second_wave_alive, 0,
            "item107c: no second-wave soldier may be left alive"
        );
        // The second wave (`Wave01`/`Wave02`) belongs to Carrington's post-lever cine (action 63
        // `Event Wave01 Wave02`), which the route now reaches because the `tp 23` placement
        // succeeded. Each generator spawned one soldier; the generator whose soldier died is
        // cleaned up (`GenNMI6` destroyed), the other still holds.
        for name in ["GenNMI6", "GenNMI7"] {
            if let Some(id) = vm.find_object(name) {
                assert!(
                    matches!(
                        vm.get_property(id, "SpawnActor"),
                        Some(xiii_script::Value::Object(Some(_)))
                    ),
                    "{name}: the second wave must have spawned its soldier"
                );
            }
        }
        // item101d positive pin: the ledge rule now lets BaseSoldier6's authored attack path
        // (network 222: AttackPoint13 -> AttackPoint12 -> AttackPoint0 on the ground) hand off to
        // a drop. It descends from the station's upper level (spawn 871,-590,462) to the ground
        // and holds at (-18,-710,102) in `Tenir` (measured); it is no longer frozen at the roof
        // edge (615,-926,462). Reaching the ground is exactly what the ledge semantics must let a
        // lower-node path move do; it still cannot fire `RushFinal` because it stops behind the
        // station wall (the item102 navigation/door divergence, independent of the ledge rule).
        // item107c: it now opens `porte10` (`IAController.NotifyBump`), attacks and dies to the
        // route's fire; the position pins below describe where its corpse lies.
        let bs6 = vm
            .find_object("BaseSoldier6")
            .expect("second-wave BaseSoldier6");
        let bs6_loc = match vm.get_property(bs6, "Location") {
            Some(xiii_script::Value::Vector(p)) => *p,
            other => panic!("BaseSoldier6 Location not a vector: {other:?}"),
        };
        assert!(
            session.actor_is_dead(bs6),
            "item107c: BaseSoldier6 must have been shot dead by the route"
        );
        assert!(
            bs6_loc[2] < 300.0,
            "BaseSoldier6 must have descended its attack path off the roof (roof z=462): {bs6_loc:?}"
        );
        // item107c: the corpse lies north of `porte10` (y = -710), i.e. the soldier passed the door it
        // opened and came on toward the platform (measured corpse at (53,268,102)).
        assert!(
            bs6_loc[0].abs() < 600.0 && bs6_loc[1] > -700.0,
            "BaseSoldier6 must have passed the opened porte10 toward the platform: {bs6_loc:?}"
        );
        // The cable-car lever use completed goal 4 (its `PlayerTriggerToggle` open fired the
        // map's `SetGoalComplete(94)`). Record the lever state for the report.
        let lever = vm.find_object("XIIIMover1").expect("cable-car lever");
        println!(
            "[hual04a lever] XIIIMover1 KeyNum={:?} state={:?}",
            vm.get_property(lever, "KeyNum"),
            vm.state_name(lever)
        );
        assert_eq!(
            vm.get_property(mi, "NextMapLevelWithUnr"),
            Some(&xiii_script::Value::Str("Hual04c.unr".to_owned()))
        );
        println!(
            "[hual04a end] item107c: the post-lever cine passes `wait player 128`, fires `Event Wave01 Wave02` and parks at \
             `wait event RushFinalCine` until both second-wave soldiers have fired `RushFinal`. The route kills \
             `BaseSoldier7` and, once `APawn::NotifyBump` lets `BaseSoldier6` open `porte10`, `BaseSoldier6` too; \
             the cine then runs `wait player 200` and `event LaFinDeLaMap`, the route walks into \
             `DetectionVolume10` (Event `finmap`), goal 5 completes and the map requests Hual04c."
        );
    }

    /// item99: the first Kello01b route driven entirely by retail input. The PlayerStart sits on
    /// the snow mountain's north flank facing ENE (yaw 3496); the script holds that facing east
    /// along the flank, turns south and drops the east face of the massif to the chalet snow
    /// field, opens the front door `Porte0` with the retail Grab key (`use Porte0`, which runs the
    /// door's own `XIIIPorte.PlayerTriggerToggle.PlayerTrigger`), re-approaches the doorway from
    /// the open leaf's hinge side and enters; `TouchTrigger2` (`Event='XIII_dans_chalet'`) then
    /// starts the Cine0 cutscene (`CineController21` -> `PlayingSequence`).
    ///
    /// The route is honest about what it reaches: the player survives, no objective is completed
    /// and there is no travel. item99b decoded and fixed the flash chain: `fin_transition` is
    /// emitted by the level's `GenFlash` (`Level.FlashManager`), which the shipped Kello01b data
    /// never triggers from script (Cine0/Cine1's `playerevent flash` flash prologue sits behind
    /// `goto deroulement_normal`). Retail HXAudio observes its global integer variable `Flash`;
    /// the bank program's writes and playback timing remain undecoded. The Partial cue-name
    /// model infers stages from `Flash1`/`Flash2`/`Flash3`/`FlashsEnd` with a next-VM-tick delay.
    /// The engine now (a) executes the class-default
    /// lvalue write `XIIIPlayerPawn.default.bCollideWorld` in `MapInfo.SaveInventoryForFlash`,
    /// (b) skips `transient` struct members when decoding the raw `structFlash`, and (c) drives
    /// the FlashManager through that explicitly modelled cue path. `fin_transition` fires and the
    /// flashback cine Cine1 starts.
    ///
    /// item99d: the `wait warning` blocker is fixed generically. `CineController2`'s `wait warning`
    /// sets `flagsPaused` bit 4; the warning comes from
    /// `DialogueManager.STA_PlayingDialogue.EndOfLine`, which walks the line's
    /// `SpeakersToWarnAtTheEndOfThisLine` and calls `CineWarn`. The line never spoke because
    /// `DialogueManager1` (`dial_flash`) was destroyed by `MustBeStop` at `StartDialogue`: its
    /// `Speakers[0].Pawn` names the map's pending-kill `Cine2` export (`bDeleteMe=true`), which the
    /// engine removes and whose references it nulls. `Vm::load_level` now performs that generic
    /// reference replacement (unit-tested). The whole cutscene runs (Cine0/Cine1 reach
    /// `destroy self` at ~226 s, `event objectif99` promotes goal 0).
    ///
    /// item99e: the post-cutscene firefight is re-driven by input and survived with a margin. The
    /// item99d claim that the route "loots the `FusilPompePick0` shotgun" was wrong: that pickup (and
    /// `MiniUziPick0`/`MagnumPick0`) sits on a lower floor at z~1930, below the fully-enclosed
    /// goal-trigger room floor at z~2091, so the player never touched it and fought `BaseSoldier2`
    /// unarmed (`Fists`), which shotgunned it to death. The route now grabs the reachable
    /// west-floor `M60Pick1` through the game's own interaction Grab, selects it with the retail
    /// `next_weapon` key, returns to the chalet wardrobe for cover and kills the close
    /// `BaseSoldier2`, then crosses the goal triggers and survives. Next blocker: goal 0
    /// completes only on the level's own `objectif0` event, so goal 0/1 and the `PRock01a` travel
    /// stay unreached.
    ///
    /// item99f decoded the emitter chain and measured the blocker. `objectif0` has exactly one
    /// emitter: `TouchTrigger0` (`Event='objectif0'` at `(-2262,-886,2127)`), a player-touch
    /// trigger in the annex/exterior zone south of the chalet. It also reaches `XIIIGoalTrigger2`
    /// (GoalNumber 0 -> `CauseGoal` -> goal 0), `TriggerSound29` and `gennmi23`/`gennmi27`. The
    /// cutscene's `event wave1` opens the annex door `Porte1` (Tag='wave1'), but the passage to
    /// the trigger is gated by a second door `Porte3` (Tag='veryend') and the `BreakableMover0`
    /// wall (Tag='veryend', Health=1, `bWorldGeometry=true`). `veryend` comes only from the late
    /// level script (`XIIIDispatcher1` Tag='fin_dial_artilleurs' <- `DialogueManager11` <-
    /// `CineTrigger2` Tag='End'), and 'End' is fired by clearing the wave chain (gennmi10's
    /// soldier) or the M60-armed chopper `NastyHelico2` (Health=1000, `DeadEvent='End'`,
    /// `InvisibleUntilTriggered` until 'wave401'). Goal 1's completion event 'roquette' is fired
    /// by `XIDPawn.XIIIDispatcher0` (Tag='pre_roquette'), triggered by `TouchTrigger6`
    /// (`Event='pre_roquette'` at `(3303,-3417,1175)`), and completes `XIIIGoalTrigger1`
    /// (GoalNumber 1, `TimeBeforeTrigger` 15).
    ///
    /// item99g completes the level by input. `veryend` needs `CineTrigger2` (Tag `End`,
    /// `InitialCounterValue` 4, not 1 as item99f wrote) to count four `End` events: the soldiers of
    /// `gennmi10` (wave1), `gennmi25` (wave602), `gennmi20` (wave603) and the chopper
    /// `NastyHelico2` (`DeadEvent`), which appears on `wave401` and dies to the player's M60
    /// (`HelicoDeco.TakeDamage`, Health 1000). The generators spawn their soldier only when the
    /// spawn point is hidden from, or behind, the player (`gennmi.genese.Timer`), which the route
    /// handles by turning away. Then `Porte3` opens, `BreakableMover0` breaks, `TouchTrigger0`
    /// completes goal 0, the three backup soldiers are killed, and `TouchTrigger6` completes goal 1
    /// 15 s later; the map's own `DoTravel` requests `PRock01a` (the run stops at that request).
    /// The route is a replay: the fight lines are recorded input commands (see the script header).
    #[test]
    fn opt_in_kello01b_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Kello01b".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Kello01b");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text = include_str!("../../tests/data/kello01b_route.script");
        assert!(!text.lines().any(|line| {
            let command = line.split('#').next().unwrap_or("");
            ["teleport", "weapon ", "item ", "set_goal", "take_control"]
                .iter()
                .any(|forbidden| command.contains(forbidden))
        }));
        let script = script::Script::parse(text).expect("parse Kello01b route");
        // item99g: the route now ends with the map's own `DoTravel` -> `PRock01a`. Stop the run at
        // the travel request so the assertions below read the Kello01b session, not the carried one.
        let outcome = run_script_inner(
            &game_dir,
            "Kello01b",
            &script,
            &resolved.params,
            &scene,
            800.0,
            true,
            RunnerOptions {
                stop_on_travel: true,
                ..Default::default()
            },
        )
        .expect("run Kello01b route");
        let session = &outcome.session;
        let vm = session.vm();
        let events_of = |target: &str, function_suffix: &str| -> Vec<(f64, Vec<String>)> {
            vm.trace
                .iter()
                .filter_map(|e| match &e.kind {
                    xiii_script::TraceKind::Event {
                        target: t,
                        function,
                        args,
                    } if t == target && function.ends_with(function_suffix) => {
                        Some((e.time, args.clone()))
                    }
                    _ => None,
                })
                .collect()
        };
        // The session keeps only the last 64 player touches, so a touch is also looked up in the
        // full VM event trace (`TouchTrigger.Touch` / `Actor.Touch` with the player pawn as arg).
        let touched = |name: &str| -> bool {
            session.touches().iter().any(|(_, n)| n == name)
                || events_of(name, ".Touch")
                    .iter()
                    .any(|(_, args)| args.first().is_some_and(|a| a == "XIIIPlayerPawn"))
        };
        println!(
            "[kello01b route] error={:?} objectives={:?} travel={:?} position={:?} health={:?} touches={:?}",
            session.first_error(),
            session.objective_states(),
            outcome.travel,
            session.player_location(),
            session.player_health(),
            session.touches(),
        );
        survey::print_details(&survey::extract("Kello01b", &outcome));
        assert!(
            session.first_error().is_none(),
            "{:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(session.blocked.is_empty(), "{:?}", session.blocked);
        assert!(
            session.player_health().is_some_and(|h| h > 0.0),
            "player must survive: {:?}",
            session.player_health()
        );
        // item99g: alive and not ended: no GameEnded/death state took the controller.
        assert!(
            !session.player_input_ended(),
            "the player must keep control (no GameEnded): {}",
            session.player_controller_state()
        );
        // The route is a walk-and-fight route (the start path has no big drop, the chalet fights
        // and the backup soldiers cost health), so the player dipped below full health on this map
        // but never died: the per-map minimum Health is positive and below 150. (The travel request
        // itself restores the carried Health, so the end-of-run value is not the pin.)
        let min_health = outcome
            .map_min_health
            .iter()
            .find(|(map, _)| map == "Kello01b")
            .map(|(_, h)| *h);
        println!("[kello01b item99g] minimum Health on Kello01b: {min_health:?}");
        assert!(
            min_health.is_some_and(|h| h > 0.0 && h < 150.0),
            "the fights must cost some health, never all of it: {min_health:?}"
        );
        let goals = session.objective_states();
        assert_eq!(goals.len(), 2, "the map authors two objectives: {goals:?}");
        // item99g: goal 0 (cover Kim's escape) used to be pinned incomplete (item99f: the
        // `veryend` gate was unreachable). Measured cause of the change: the wave chain, the chopper
        // and the backup soldiers are now cleared by input, `veryend` opens the annex and the
        // player touches `TouchTrigger0`, whose `objectif0` completes goal 0 through
        // `XIIIGoalTrigger2`; `TouchTrigger6` -> `roquette` completes goal 1 after its 15 s timer.
        assert!(
            goals[0].completed,
            "goal 0 (cover Kim's escape) must complete through `objectif0`: {goals:?}"
        );
        assert!(
            goals[1].completed,
            "goal 1 (find Jones and Carrington again) must complete through `roquette`: {goals:?}"
        );
        // The map's own `TestGoalComplete` -> `DoTravel` requests the next map once both goals
        // are complete; the old `travel.is_empty()` pin is reversed for that measured cause.
        assert_eq!(
            outcome.travel.len(),
            1,
            "one travel hop: {:?}",
            outcome.travel
        );
        assert_eq!(outcome.travel[0].from, "Kello01b");
        assert_eq!(outcome.travel[0].to, "PRock01a");
        assert_eq!(outcome.travel[0].url, "PRock01a.unr");
        assert!(
            touched("TouchTrigger2"),
            "the player must reach TouchTrigger2 (XIII_dans_chalet): {:?}",
            session.touches()
        );
        // item99d: the DialogueManager line-end warning now releases Cine1's `wait warning`, so the
        // whole chalet cutscene runs. `DialogueManager1` (tag `dial_flash`) is the flashback line
        // whose `SpeakersToWarnAtTheEndOfThisLine` used to be unreachable; its line now speaks.
        assert!(
            session
                .dialogues
                .iter()
                .any(|(_, d)| d.actor.eq_ignore_ascii_case("DialogueManager1")),
            "the flashback line (`DialogueManager1`, `dial_flash`) must speak: {:?}",
            session.dialogues
        );
        // The flashback cine (`CineController22`) is gone: it ran to `destroy self` when the
        // warning released `wait warning`, and Cine0 (`CineController21`) followed past
        // `fin_flash` to `destroy self` (measured at t~226.4), which is what promotes goal 0.
        assert!(
            vm.find_object("CineController22").is_none(),
            "the flashback cine must have completed and destroyed itself"
        );
        assert!(
            goals[0].primary,
            "the cutscene's `event objectif99` must promote goal 0 to primary: {goals:?}"
        );
        // item99e: the route arms the player before the firefight. The chalet's `FusilPompePick0`
        // shotgun, `MiniUziPick0` and `MagnumPick0` sit on a lower floor (`z~1930`) under the
        // fully-enclosed goal-trigger room floor (`z~2091`) and are unreachable; the reachable
        // weapon is the west-floor `M60Pick1` (`z~2076`), grabbed through the game's own
        // interaction Grab (`use M60Pick1`) and selected with the retail `next_weapon` key.
        // item99g: the run now stops at the map's own travel request, and `DoTravel`'s level-exit
        // bookkeeping has already stripped the carried weapons from the pawn by then (measured at
        // the request: `Weapon == None`, inventory = left hand, skills, documents; Health is also
        // back at its carried value), so the item99e "active weapon is the M60" pin reads the
        // event trace instead: the M60 pickup (`Inventory.PickupFunction` on an `m60*` weapon) and
        // the shots fired with it (`XIIIWeapon.NormalFire.AnimEnd`, one per `fire` input line).
        let m60_events = |function_suffix: &str| -> usize {
            vm.trace
                .iter()
                .filter(|e| {
                    matches!(
                        &e.kind,
                        xiii_script::TraceKind::Event { target, function, .. }
                            if target.starts_with("m60") && function.ends_with(function_suffix)
                    )
                })
                .count()
        };
        assert!(
            m60_events("Inventory.PickupFunction") >= 1,
            "the route must loot the reachable M60 (M60Pick1)"
        );
        let m60_shots = m60_events("NormalFire.AnimEnd");
        assert!(
            m60_shots >= 200,
            "the route must fire the looted M60 for the waves and the chopper: {m60_shots}"
        );
        // Positive pins: the player crosses the chalet floor onto the map's own goal triggers
        // (`XIIIGoalTrigger2` tag `objectif0`, `XIIIGoalTrigger1` tag `roquette`).
        for trigger in ["XIIIGoalTrigger2", "XIIIGoalTrigger1"] {
            assert!(
                touched(trigger),
                "the route must reach the goal trigger {trigger}: {:?}",
                session.touches()
            );
        }
        // item99f: the emitter chain. `objectif0` (the event goal 0 completes on) has exactly one
        // emitter in the map: `TouchTrigger0` (Event='objectif0' at (-2262,-886,2127)), a
        // player-touch trigger in the annex/outside zone south of the chalet. The event also
        // reaches `XIIIGoalTrigger2` (GoalNumber 0 -> `CauseGoal`), `TriggerSound29` (the
        // objective music) and the reinforcement generators `gennmi23`/`gennmi27`.
        let touch0 = vm
            .find_object("TouchTrigger0")
            .expect("the sole objectif0 emitter");
        assert_eq!(
            vm.get_property(touch0, "Event"),
            Some(&Value::Name("objectif0".to_owned())),
            "TouchTrigger0 must be the objectif0 emitter"
        );
        assert_eq!(
            vm.get_property(touch0, "bActif"),
            Some(&Value::Bool(true)),
            "the emitter must be armed (its PostBeginPlay sets bActif)"
        );
        // item99g: the old pin "the route must not reach TouchTrigger0" is reversed: the annex
        // gate is open now (measured cause below), and the touch is what completes goal 0.
        assert!(
            touched("TouchTrigger0"),
            "the route must reach the objectif0 emitter: {:?}",
            session.touches()
        );
        assert!(
            touched("TouchTrigger6"),
            "the route must reach the pre_roquette emitter: {:?}",
            session.touches()
        );
        // The cutscene's own `event wave1` (Cine0.tabActions[77]) opened the annex door `Porte1`
        // (Tag='wave1') through `XIIIPorte.PlayerTriggerToggle.Trigger` -> `PlayerTrigger`.
        let porte1 = vm.find_object("Porte1").expect("annex door");
        assert_eq!(
            vm.get_property(porte1, "bOpened"),
            Some(&Value::Bool(true)),
            "the cutscene's `event wave1` must open Porte1"
        );
        // The passage south to `TouchTrigger0` was gated behind the second annex door `Porte3`
        // (Tag='veryend') and the `BreakableMover0` wall (Tag='veryend', Health=1,
        // `bWorldGeometry=true`). `veryend` is fired only by the late level script
        // (`XIIIDispatcher1`, Tag='fin_dial_artilleurs'), which needs the `End` counter (below).
        // The wall is `bWorldGeometry`, and `XIIIBulletsAmmo.ProcessTraceHit` only spawns impacts
        // for such actors (it never calls `TakeDamage`), so fire cannot break it: the authored
        // condition is the event. item99g: the gates are now open (reversed pins, measured).
        let porte3 = vm.find_object("Porte3").expect("annex south door");
        assert_eq!(
            vm.get_property(porte3, "bOpened"),
            Some(&Value::Bool(true)),
            "Porte3 must have opened on `veryend`"
        );
        assert_eq!(
            vm.get_property(porte3, "bClosed"),
            Some(&Value::Bool(false)),
            "Porte3 must no longer be closed after `veryend`"
        );
        // `BreakableMover.Break` destroys the mover (measured: it is gone from the live objects once
        // `veryend` fires), so the exported object is looked up including deleted ones.
        let wall_id = vm
            .objects
            .iter()
            .position(|o| o.name == "BreakableMover0")
            .expect("south wall") as xiii_script::ObjectId;
        assert!(
            vm.objects[wall_id as usize].deleted
                || vm.get_property(wall_id, "bBroken") == Some(&Value::Bool(true)),
            "the veryend wall must be broken (and destroyed) after `veryend`"
        );
        assert!(
            vm.find_live_object("BreakableMover0").is_none(),
            "the broken wall must no longer be a live collision mover"
        );
        assert_eq!(
            vm.get_property(wall_id, "bWorldGeometry"),
            Some(&Value::Bool(true)),
            "the wall is world geometry (bullet-proof; it breaks only on `veryend`)"
        );
        // item99g: the `End` counter. `CineTrigger2` (Tag='End', InitialCounterValue=4; the item99f
        // report's "1" was wrong) fires `avant_veryend` on the fourth `End`: the three wave
        // generators' soldiers (`gennmi10` wave1, `gennmi25` wave602, `gennmi20` wave603) and the
        // chopper `NastyHelico2.DeadEvent`. Each is a `CounterTrigger.Trigger` event in the trace.
        let counts = events_of("CineTrigger2", "CounterTrigger.Trigger");
        assert_eq!(
            counts.len(),
            4,
            "four `End` events must reach CineTrigger2: {counts:?}"
        );
        for (_, args) in &counts[..3] {
            assert!(
                args[0].starts_with("BaseSoldier"),
                "the first three `End`s come from the wave soldiers: {counts:?}"
            );
        }
        assert_eq!(
            counts[3].1[0], "NastyHelico2",
            "the fourth `End` is the chopper's DeadEvent: {counts:?}"
        );
        // The chopper appears on `wave401` (a wave soldier's death triggers it while
        // `InvisibleUntilTriggered`), takes the player's M60 damage (Health 1000) and is gone.
        let appear = events_of("NastyHelico2", "InvisibleUntilTriggered.Trigger");
        assert_eq!(appear.len(), 1, "NastyHelico2 appears once: {appear:?}");
        assert!(
            appear[0].0 < counts[3].0,
            "the chopper must appear before it dies: {appear:?} {counts:?}"
        );
        let heli_damage: i64 = events_of("NastyHelico2", "HelicoDeco.TakeDamage")
            .iter()
            .filter(|(_, args)| args.get(1).is_some_and(|a| a == "XIIIPlayerPawn"))
            .filter_map(|(_, args)| args[0].parse::<i64>().ok())
            .sum();
        assert!(
            heli_damage >= 1000,
            "the player's fire must deal the chopper's 1000 Health: {heli_damage}"
        );
        assert!(
            vm.find_live_object("NastyHelico2").is_none(),
            "the destroyed chopper must be gone"
        );
        // The chain after the fourth `End`: `DialogueManager11` (avant_veryend) -> `XIIIDispatcher1`
        // (fin_dial_artilleurs) -> `veryend` after its 3 s delay.
        assert!(
            !events_of("DialogueManager11", "DialogueManager.Trigger").is_empty(),
            "avant_veryend must trigger the dialogue manager"
        );
        assert!(
            !events_of("XIIIDispatcher1", "XIIIDispatcher.Trigger").is_empty(),
            "fin_dial_artilleurs must run the veryend dispatcher"
        );
        // Goal 1's chain: TouchTrigger6 (`pre_roquette`) -> XIIIDispatcher0 -> `roquette`.
        assert!(
            !events_of("XIIIDispatcher0", "XIIIDispatcher.Trigger").is_empty(),
            "pre_roquette must run XIIIDispatcher0"
        );
        println!(
            "[kello01b item99g] End counter events {counts:?}; chopper appeared {appear:?}, player \
             damage to it {heli_damage}; objectif0 emitter TouchTrigger0 (Event='objectif0', \
             (-2262,-886,2127)) touched after `veryend` opened Porte3 and broke BreakableMover0 \
             (bWorldGeometry, so only the event breaks it); receivers XIIIGoalTrigger2 (goal 0), \
             TriggerSound29 and the backup generators gennmi23/gennmi27; goal 1 via TouchTrigger6 \
             (Event='pre_roquette' at (3303,-3417,1175)) -> XIIIDispatcher0 -> 'roquette' -> \
             XIIIGoalTrigger1 (GoalNumber 1, TimeBeforeTrigger 15) -> DoTravel."
        );
        assert_eq!(
            vm.get_property(
                session.map_info().expect("Kello01b MapInfo"),
                "NextMapLevelWithUnr"
            ),
            Some(&Value::Str("PRock01a.unr".to_owned()))
        );
    }

    /// Kello01a (the map after Hual04c): start fresh at PlayerStart0 with the map-entry
    /// loadout and drive the level by input along the authored valley loop, then north toward
    /// the dry band, stopping at the authored invisible BSP sheet.
    ///
    /// item98 walked south-west straight from the start into the start bowl's rim bank
    /// (~50 degrees, steeper than the walker's `MINFLOORZ=0.7`) and pinned that. item98c planned
    /// a path across lake Keelownee from the map's own data; its route drowned. item98d showed
    /// why: the two lake `Gameplay.WaterVolume`s (`WaterVolume0`/`WaterVolume1`) are
    /// `bPainCausing=true, DamagePerSec=60, DamageType=DTDrowned`, so retail water is lethal on
    /// touch, and the ice floes do not span the crossing. item98e re-mapped the floes and the
    /// dry north band: the floe gaps (up to ~1200 UU at the route's latitude) exceed the ~417 UU
    /// retail running jump, so the dry band is the path a retail player takes. This route leaves
    /// the valley at (~-500,-3850) and climbs north to (~-4200,-776), where the walker is
    /// stopped by the authored invisible solid BSP sheet (`Model207`, triangle
    /// (-3664,-1440,-1344)-(-5280,896,3776)-(-5280,896,-1344), spanning x[-5280,-3664]
    /// y[-1440,896] z[-1344,3776]). The objective `Find the chalet` and the Kello01b travel stay
    /// unreached. See `local/reports/item98e-kello01a-crossing.md`.
    #[test]
    fn opt_in_kello01a_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("Kello01a".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Kello01a");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text = include_str!("../../tests/data/kello01a_route.script");
        // The route is input-only: forbid the host bridges and a `weapon <Class>` grant. A
        // `select_weapon`/`switch_weapon` number-key selection is ordinary input (the carried
        // magnum/shotgun from the map-entry loadout), so match the command word, not a substring.
        assert!(!text.lines().any(|line| {
            let command = line.split('#').next().unwrap_or("").trim();
            let word = command.split_whitespace().nth(1);
            ["teleport", "weapon", "item", "set_goal", "take_control"]
                .iter()
                .any(|forbidden| word == Some(*forbidden))
        }));
        let script = script::Script::parse(text).expect("parse Kello01a route");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Kello01a",
            &script,
            &resolved.params,
            &scene,
            100.0,
        )
        .expect("run Kello01a route");
        let session = &outcome.session;
        let vm = session.vm();
        println!(
            "[kello01a route] error={:?} objectives={:?} travel={:?} position={:?} health={:?}",
            session.first_error(),
            session.objective_states(),
            outcome.travel,
            session.player_location(),
            session.player_health()
        );
        survey::print_details(&survey::extract("Kello01a", &outcome));
        println!(
            "[kello01a loadout] weapon={:?} inventory={:?}",
            session
                .player_weapon()
                .map(|w| vm.objects[w as usize].name.clone()),
            session
                .inventory_items()
                .iter()
                .map(|(n, c)| format!("{n}[{c}]"))
                .collect::<Vec<_>>()
        );
        let player_name = session.player_name.clone();
        let mut dmg_by: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();
        let mut dmg_detail: Vec<String> = Vec::new();
        for e in &vm.trace {
            if let xiii_script::TraceKind::Event {
                target,
                function,
                args,
            } = &e.kind
                && function.ends_with("TakeDamage")
                && target.eq_ignore_ascii_case(&player_name)
            {
                *dmg_by
                    .entry(args.get(1).cloned().unwrap_or_default())
                    .or_default() += 1;
                dmg_detail.push(format!(
                    "{}dmg/{}/{}",
                    args.first().cloned().unwrap_or_default(),
                    args.get(1).cloned().unwrap_or_default(),
                    args.get(4).cloned().unwrap_or_default()
                ));
            }
        }
        println!("[kello01a damage] by instigator: {dmg_by:?}");
        println!("[kello01a damage] detail: {dmg_detail:?}");
        assert!(
            session.first_error().is_none(),
            "{:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(session.blocked.is_empty(), "{:?}", session.blocked);
        let goals = session.objective_states();
        assert_eq!(goals.len(), 1, "Kello01a authors one objective: {goals:?}");
        assert!(
            goals[0].primary,
            "the single objective is primary: {goals:?}"
        );
        assert!(
            goals[0].text.eq_ignore_ascii_case("Find the chalet"),
            "unexpected objective text: {:?}",
            goals[0].text
        );
        let mi = session.map_info().expect("Kello01a MapInfo");
        assert_eq!(
            vm.get_property(mi, "NextMapLevelWithUnr"),
            Some(&xiii_script::Value::Str("Kello01b.unr".to_owned()))
        );
        let pawns = session.player_pawn_actors();
        assert_eq!(pawns.len(), 1, "one live player pawn");
        // item98e positive pins. The route leaves the start bowl, follows the authored valley
        // loop south-west, then climbs north from the valley at (~-500,-3850) to the band's
        // south face, where the authored invisible BSP sheet stops it. item98d stopped at the
        // lake's east shore (~(-7996,-1751)); this route reaches ~(-4200,-776).
        let position = session.player_location().expect("player position");
        println!(
            "[kello01a item98e] completed={} travel={:?} position={:?} health={:?}",
            goals[0].completed,
            outcome.travel,
            position,
            session.player_health()
        );
        assert!(
            session.player_health().is_some_and(|h| h > 0.0),
            "the route must stop before any fatal point and the player must survive: {:?}",
            session.player_health()
        );
        assert!(
            (-4300.0..-4100.0).contains(&position[0]) && (-850.0..-650.0).contains(&position[1]),
            "item98e: the route must reach the dry band's south face at ~(-4200,-776) where the \
             invisible BSP sheet blocks it (item98d stopped at the lake shore ~(-7996,-1751)): \
             {position:?}"
        );
        assert!(
            position[2] > -400.0,
            "the band approach is dry (no lake water): {position:?}"
        );
        assert!(
            !goals[0].completed,
            "the objective must not complete on this route: {goals:?}"
        );
        assert!(
            outcome.travel.is_empty(),
            "the Kello01b travel must not be reached on this route: {:?}",
            outcome.travel
        );
        // item98e pinned blocker: the authored invisible solid BSP sheet (`Model207`,
        // `PolyFlags=0x1`) that spans x[-5280,-3664] y[-1440,896] z[-1344,3776]. The lake itself
        // is also impassable: `WaterVolume0`/`WaterVolume1` are `bPainCausing=true,
        // DamagePerSec=60, DamageType=DTDrowned`, and the ice floe gaps (up to ~1200 UU at the
        // route's latitude) exceed the ~417 UU retail running jump. See the report for the
        // measured evidence.
    }

    /// Input replay of the whole Kello01a route: PlayerStart, the start-bowl and ambush fights,
    /// the lake ice crossing, the far shore and west cliffs, the chalet soldiers, `Find the
    /// chalet` (`Trigger0` -> `XIIIGoalTrigger0`) and the map's own `DoTravel` to Kello01b
    /// (item98j, extending item98f/item98i).
    ///
    /// The route is one self-contained input script (`kello01a_chalet_route.script`): `goto`/`jump`
    /// waypoints on the walkable floes (the open-water channels between the `GR_banquise_01`/`02`
    /// floes are jumped, not walked) and `select_weapon`/`track`/`fire`/`reload` for the soldiers.
    /// Magnum rounds (12) and the shared shotgun/rifle shells (13) are the only ammunition on the
    /// route, so every shot is placed on a visible soldier. The run continues a few seconds into
    /// Kello01b so that the end state is the new map's walking player, not the Kello01a
    /// `GameEndedSuccess` hand-over. The earlier north-band route and every one of its assertions
    /// remain a separate regression above.
    #[test]
    fn opt_in_kello01a_chalet_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to run the Kello01a chalet route");
            return;
        };
        let opts = Options {
            map: Some("Kello01a".into()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import Kello01a");
        let resolved = resolve_params(&game_dir).expect("player defaults");
        let text = include_str!("../../tests/data/kello01a_chalet_route.script");
        assert!(
            !text.lines().any(|line| {
                let command = line.split('#').next().unwrap_or("").trim();
                [
                    "teleport",
                    "place",
                    "weapon",
                    "item",
                    "set_goal",
                    "take_control",
                    "assume_control",
                ]
                .contains(&command.split_whitespace().nth(1).unwrap_or(""))
            }),
            "route must contain only input commands"
        );
        let script = script::Script::parse(text).expect("chalet route");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "Kello01a",
            &script,
            &resolved.params,
            &scene,
            script.last_time() + 3.0,
        )
        .expect("input replay");
        let session = &outcome.session;
        println!(
            "[kello01a item98j] map={} objectives={:?} travel={:?} position={:?} health={:?} min_health={:?} state={}",
            outcome.final_map,
            outcome.map_objectives,
            outcome.travel,
            session.player_location(),
            session.player_health(),
            outcome.map_min_health,
            session.player_controller_state()
        );
        let player_name = session.player_name.clone();
        let (_, kello_trace) = outcome
            .map_traces
            .iter()
            .find(|(map, _)| map == "Kello01a")
            .expect("Kello01a trace");
        let mut dmg_detail: Vec<String> = Vec::new();
        let mut player_hits: Vec<(String, f64)> = Vec::new();
        for e in kello_trace {
            let xiii_script::TraceKind::Event {
                target,
                function,
                args,
            } = &e.kind
            else {
                continue;
            };
            if !function.ends_with("TakeDamage") {
                continue;
            }
            if target.eq_ignore_ascii_case(&player_name) {
                dmg_detail.push(format!(
                    "{:.2}s {}dmg/{}/{}",
                    e.time,
                    args.first().cloned().unwrap_or_default(),
                    args.get(1).cloned().unwrap_or_default(),
                    args.get(4).cloned().unwrap_or_default()
                ));
            } else if args
                .get(1)
                .is_some_and(|a| a.eq_ignore_ascii_case(&player_name))
            {
                player_hits.push((target.clone(), e.time));
            }
        }
        println!("[kello01a item98j damage] {dmg_detail:?}");
        println!("[kello01a item98j] player-dealt TakeDamage events: {player_hits:?}");
        assert!(
            session.first_error().is_none(),
            "{:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(session.blocked.is_empty(), "{:?}", session.blocked);
        // Kello01a's own objective state at the travel request: the single primary objective,
        // now completed by the map's own goal trigger (no host bridge: only the input script ran).
        let (map, goals) = outcome
            .map_objectives
            .first()
            .expect("Kello01a objective states");
        assert_eq!(map, "Kello01a");
        assert_eq!(goals.len(), 1);
        assert!(goals[0].primary && goals[0].text == "Find the chalet");
        assert!(
            goals[0].completed,
            "`Find the chalet` must be completed by the map's own goal trigger: {goals:?}"
        );
        // item98j positive pin: the map's own DoTravel requests Kello01b, exactly once.
        assert_eq!(
            outcome.travel.len(),
            1,
            "exactly one travel hop: {:?}",
            outcome.travel
        );
        assert_eq!(outcome.travel[0].from, "Kello01a");
        assert_eq!(outcome.travel[0].to, "Kello01b");
        assert_eq!(outcome.travel[0].url, "Kello01b.unr");
        assert_eq!(outcome.final_map, "Kello01b");
        // The player must be alive on both maps and the game not ended at the end.
        assert!(
            outcome.map_min_health.len() >= 2
                && outcome.map_min_health.iter().all(|(_, h)| *h > 0.0),
            "min health on every map must stay positive: {:?}",
            outcome.map_min_health
        );
        assert!(
            session.player_health().is_some_and(|h| h > 0.0),
            "player must survive the entire route"
        );
        assert!(
            !session.player_input_ended(),
            "the player must not be dead or in GameEnded at the end: state={}",
            session.player_controller_state()
        );
        assert_ne!(
            session.player_controller_state(),
            "GameEnded",
            "no pin may end the game"
        );
        // The three ambush soldiers (BaseSoldier/BaseSoldier1/BaseSoldier2) are killed by the
        // player's own rounds (item98i measured 0 hits with the route that kept running), and the
        // chalet soldiers by the carried rifle/shotgun.
        for soldier in ["BaseSoldier", "BaseSoldier1", "BaseSoldier2"] {
            assert!(
                player_hits
                    .iter()
                    .any(|(t, time)| t == soldier && *time > 60.0),
                "the player must hit {soldier} on the ambush: {player_hits:?}"
            );
        }
    }

    #[test]
    fn opt_in_prock04a_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("PRock04a".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import PRock04a");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text = include_str!("../../tests/data/prock04a_route.script");
        let script = script::Script::parse(text).expect("parse PRock04a route");
        assert!(
            script.events.iter().all(|event| !matches!(
                event.command,
                script::Command::Teleport(_)
                    | script::Command::Weapon(_)
                    | script::Command::Item(_)
                    | script::Command::SetGoal(_)
                    | script::Command::TakeControl
            )),
            "the route must use ordinary input only"
        );
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "PRock04a",
            &script,
            &resolved.params,
            &scene,
            74.0,
        )
        .expect("run PRock04a route");
        let session = &outcome.session;
        let vm = session.vm();
        println!(
            "[prock04a] error={:?} goals={:?} travel={:?} position={:?} health={:?} inventory={:?}",
            session.first_error(),
            session.objective_states(),
            outcome.travel,
            session.player_location(),
            session.player_health(),
            session.inventory_items()
        );
        survey::print_details(&survey::extract("PRock04a", &outcome));
        assert!(
            session.first_error().is_none(),
            "{:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(session.blocked.is_empty(), "{:?}", session.blocked);
        assert!(!session.player_input_ended(), "stop before a fatal point");
        // The route stops on the open canyon floor by the medical crate, alive. item100h: 134,
        // was 122. With the retail `ShowSelf` SightCounter countdown (Engine.dll 0x1036df40) the
        // second exchange lands one 12-hp hit (64.0 s) instead of two (60.5 s, 65.5 s on the
        // previous perception); the medkit at ~68 s then heals to 134 instead of 122 (measured).
        assert_eq!(
            session.player_health(),
            Some(134.0),
            "surviving measured stop"
        );
        assert!(
            outcome.map_min_health.iter().all(|(_, h)| *h > 0.0),
            "min health on the map must stay positive: {:?}",
            outcome.map_min_health
        );
        let near = |position: [f32; 3], expected: [f32; 3]| {
            position
                .iter()
                .zip(expected)
                .all(|(a, b)| (a - b).abs() < 2.0)
        };
        let start = outcome.trace.first().expect("spawn sample").2;
        assert!(
            near(start, [-6238.5015, -4465.876, 678.5102]),
            "normal spawn: {start:?}"
        );
        // Measured landing centres, with the same 2-UU pin as the other PRock route.
        for landing in [[-5259.8, -3623.4, 1347.7], [848.6, -3891.2, 1545.9]] {
            assert!(
                outcome.trace.iter().any(|(_, _, p, _)| near(*p, landing)),
                "grapple landing {landing:?} must be reached"
            );
        }
        for anchor in ["HookPoint10", "HookPoint2"] {
            assert!(vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { function, args, .. }
                    if function == "XIIIProjectile.Touch" && args.first().is_some_and(|arg| arg == anchor)
            )), "the player's hook must contact {anchor}");
        }
        // The first canyon soldier is defeated: the game runs its authored death
        // presentation on `BaseSoldier3` (the pawn is removed by the retail corpse path,
        // so it cannot be looked up at the end).
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target == "BaseSoldier3" && function == "BaseSoldier.PlayDying"
            )),
            "the first soldier must be defeated"
        );
        let inventory = session.inventory_items();
        for class in ["xiii.kalash", "xiii.magnum"] {
            assert!(
                inventory
                    .iter()
                    .any(|(_, carried)| carried.eq_ignore_ascii_case(class)),
                "looted {class} must remain carried: {inventory:?}"
            );
        }
        for pickup in ["MedPick10", "MedPick2"] {
            assert!(
                vm.find_live_object(pickup).is_none(),
                "{pickup} must be collected"
            );
        }
        assert_eq!(vm.trace.iter().filter(|event| matches!(
            &event.kind,
            xiii_script::TraceKind::Event { function, .. } if function.ends_with(".UseMeQuick")
        )).count(), 2, "both carried medical kits must be used through QuickHeal");
        let nurse = vm.find_object("Cine1").expect("nurse");
        assert!(
            !session.actor_is_dead(nurse),
            "preserve the nurse anti-goal"
        );
        let goals = session.objective_states();
        assert_eq!(goals.len(), 2);
        assert!(goals[0].primary && !goals[0].anti_goal && !goals[0].completed);
        assert_eq!(goals[0].text, "Find the mine entrance to rejoin Jones.");
        assert!(goals[1].primary && goals[1].anti_goal && goals[1].completed);
        assert!(outcome.travel.is_empty(), "PRock04b travel remains blocked");
        let map_info = vm
            .find_object("PRock04a1")
            .expect("map info / travel owner");
        assert_eq!(
            vm.get_property(map_info, "NextMapLevelWithUnr"),
            Some(&Value::Str("PRock04b".to_owned()))
        );
        let finish = vm
            .find_live_object("XIIIGoalTrigger0")
            .expect("unreached Fini goal trigger");
        assert_eq!(
            vm.get_property(finish, "Tag"),
            Some(&Value::Name("Fini".to_owned()))
        );
        // The player stands on the flat canyon floor beside the medical crate, alive and
        // still in control: the prefix's furthest stable point. The terrain north of here
        // is a collision trap and the floor east is a sniper field (see the report).
        let position = session.player_location().expect("player location");
        assert!(
            near(position, [1248.5105, -2546.0454, 1380.5625]),
            "crate floor stop: {position:?}"
        );
        assert_ne!(
            session.player_physics(),
            Some(2),
            "the stop must not be a falling/stuck state"
        );
    }

    /// PRock04a tyrol ride (item108b): the map crosses the canyon with the tyrolines
    /// (`XIIITyrolNavPoint*`/`TyrolTrigger*`); `TyrolTrigger.PlayerTrigger` ->
    /// `XIIIPlayerController.GoTyroling` -> the `Tyroling` state's `PlayerMove`/`ProcessMove`
    /// (`Pawn.Move`, `PHYS_None`, `fTyrolSpeed` 600).
    ///
    /// **Mechanism test, not a route.** The map's first tyrol trigger (`TyrolTrigger0` at
    /// (3168,-4099,2138)) sits on the far rim of the start pit with no measured walkable
    /// approach (see `local/reports/item108-prock04a.md`: the second-grapple landing shelf
    /// (z~1471) is separated from the nav shelf (z~1700-2150) by a ~490 UU cliff at x=800,
    /// and the closed-loop walker drifts into the pit). This test therefore bootstraps the
    /// player next to the trigger with the diagnostic `teleport` and rides it with the ordinary
    /// `use` input; the ride itself is the game's own chain, not a host bridge. A foot route to
    /// the trigger is unproven, so the test asserts only the mechanism.
    #[test]
    fn opt_in_prock04a_tyrol_mechanism() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("PRock04a".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import PRock04a");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text =
            "t=0.05 teleport 3168 -4099 2140\nt=0.6 use TyrolTrigger0\nt=0.8 use TyrolTrigger0\n";
        let script = script::Script::parse(text).expect("parse tyrol probe");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "PRock04a",
            &script,
            &resolved.params,
            &scene,
            20.0,
        )
        .expect("run tyrol probe");
        let session = &outcome.session;
        let vm = session.vm();
        for (name, function) in [
            ("TyrolTrigger0", "TyrolTrigger.PlayerTrigger"),
            ("XIIIPlayerController", "XIIIPlayerController.GoTyroling"),
        ] {
            assert!(
                vm.trace.iter().any(|event| matches!(
                    &event.kind,
                    xiii_script::TraceKind::Event { target, function: f, .. }
                        if target == name && f == function
                )),
                "the tyrol must run {name}.{function}"
            );
        }
        assert!(
            !session.player_input_ended(),
            "the ride must not end the game"
        );
        // Measured: the ride carries the pawn from NavPoint0 (3166,-4110,2366) to
        // NavPoint1 (7176,-3311,2720) at fTyrolSpeed and drops it on the far rim.
        let position = session.player_location().expect("player location");
        let near_end = position
            .iter()
            .zip([7217.7, -3290.9, 2457.8])
            .all(|(a, b)| (a - b).abs() < 40.0);
        assert!(
            position[0] > 7000.0 && near_end,
            "the tyrol must reach the far NavPoint: {position:?}"
        );
        assert_eq!(
            session.player_controller_state(),
            "PlayerWalking",
            "the tyrol state must hand back to walking at the far end"
        );
    }

    #[test]
    fn opt_in_prock03_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("PRock03".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import PRock03");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text = include_str!("../../tests/data/prock03_route.script");
        assert!(!text.lines().any(|line| {
            let command = line.split('#').next().unwrap_or("").trim();
            let word = command.split_whitespace().nth(1);
            ["teleport", "weapon", "item", "set_goal", "take_control"]
                .iter()
                .any(|forbidden| word == Some(*forbidden))
        }));
        let script = script::Script::parse(text).expect("parse PRock03 route");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "PRock03",
            &script,
            &resolved.params,
            &scene,
            60.0,
        )
        .expect("run PRock03 route");
        let session = &outcome.session;
        let vm = session.vm();
        println!(
            "[prock03 route] error={:?} objectives={:?} travel={:?} position={:?} health={:?}",
            session.first_error(),
            session.objective_states(),
            outcome.travel,
            session.player_location(),
            session.player_health()
        );
        println!(
            "[prock03 loadout] weapon={:?} inventory={:?}",
            session
                .player_weapon()
                .map(|w| vm.objects[w as usize].name.clone()),
            session
                .inventory_items()
                .iter()
                .map(|(n, c)| format!("{n}[{c}]"))
                .collect::<Vec<_>>()
        );
        survey::print_details(&survey::extract("PRock03", &outcome));
        assert!(
            session.first_error().is_none(),
            "unexpected blocker: {:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(session.blocked.is_empty(), "{:?}", session.blocked);
        assert!(
            session.player_health().is_some_and(|h| h > 0.0),
            "player must survive"
        );
        let goals = session.objective_states();
        assert_eq!(
            goals.len(),
            3,
            "the map authors three objectives: {goals:?}"
        );
        assert!(
            goals[0].primary && !goals[0].anti_goal && !goals[0].completed,
            "goal 0 (find a way out of the asylum) is the primary open objective: {goals:?}"
        );
        assert!(
            !goals[1].primary && !goals[1].completed,
            "goal 1 (use the hostage) stays open and is not primary on this route: {goals:?}"
        );
        assert!(
            goals[2].primary && goals[2].anti_goal && goals[2].completed,
            "the authored anti-goal (do not kill the medical staff) must still hold: {goals:?}"
        );
        assert!(
            outcome.travel.is_empty(),
            "the blocked route must not travel: {:?}",
            outcome.travel
        );
        // The route reached the west cell-block checkpoint and cannot open it. The measured
        // stopping point is pinned exactly (no tolerance-widening).
        let position = session.player_location().expect("player location");
        assert!(
            (position[0] - -3821.96).abs() < 2.0
                && (position[1] - 1720.03).abs() < 2.0
                && (position[2] - 75.0).abs() < 2.0,
            "the route must stop at the porte_sec checkpoint: {position:?}"
        );
        // Pin the blocker from the game's own state, not from a host bridge:
        // `Porte29` is the `porte_sec` security door in its `TriggerControl` state (only a `Trigger`
        // event moves it), and the authored opener `MagneticPassTrigger0` needs the pawn to carry a
        // `pass` item (`FindPowerUpItemNameForTrigger`, Tag `pass`) that this map never gives out.
        let security = vm.find_object("Porte29").expect("porte_sec security door");
        assert_eq!(
            vm.state_name(security).as_deref(),
            Some("TriggerControl"),
            "the security door must be in its event-gated TriggerControl state"
        );
        let pass = vm
            .find_object("MagneticPassTrigger0")
            .expect("authored magnetic-pass checkpoint opener");
        assert_eq!(
            vm.get_property(pass, "Tag"),
            Some(&Value::Name("pass".to_owned())),
            "the checkpoint opener reads the pawn's `pass` item"
        );
        assert!(
            session
                .inventory_items()
                .iter()
                .all(|(name, _)| !name.eq_ignore_ascii_case("PRock03MagneticCard")),
            "the pawn must carry no magnetic card (the map gives none)"
        );
        // The card and therefore the checkpoint are behind the level's guard/key chain: the
        // only nav route to the card crosses `Porte30` (`UnlockItemCode "clefgarde"`). The
        // detection volume at the checkpoint (`DetectionVolume7`, `Event 'gardien_lostpass'`)
        // starts the guard `Cine2`'s authored sequence, which walks its waypoint chain and runs
        // action `opendoor porte30` (tabActions[12]). With the retailed `ULevel::FarMoveActor`
        // placement (no actor-encroachment rejection) that cine teleport now succeeds, so the
        // door is open at the end of this walking route. Pinned from the game's own state.
        let porte30 = vm.find_object("Porte30").expect("clefgarde door");
        assert!(
            !vm.is_in_state(porte30, "Locked"),
            "the guard cine's `opendoor porte30` must have unlocked the door by the checkpoint: \
             {:?}",
            vm.state_name(porte30)
        );
        assert!(
            vm.is_in_state(porte30, "PlayerTriggerToggle"),
            "the cine leaves Porte30 in its open `PlayerTriggerToggle` state: {:?}",
            vm.state_name(porte30)
        );
        assert_eq!(
            vm.get_property(porte30, "bOpened"),
            Some(&Value::Bool(true)),
            "the cine's `open door` leaves Porte30 open"
        );
        assert_eq!(
            vm.get_property(porte30, "UnlockItemCode"),
            Some(&Value::Str("clefgarde".to_owned())),
            "Porte30's unlock code is the clefgarde key"
        );
        let guard = vm
            .find_object("Cine2")
            .expect("gardien_lostpass key holder");
        assert_eq!(
            vm.get_property(guard, "Event"),
            Some(&Value::Name("garde_porte".to_owned())),
            "the guard's death Event (XIIIPawn.Died -> TriggerEvent) is garde_porte"
        );
        let opens_door = match vm.get_property(guard, "tabActions") {
            Some(Value::Array(entries)) => entries.iter().any(|v| match v {
                Value::Str(s) | Value::Name(s) => s.eq_ignore_ascii_case("opendoor porte30"),
                _ => false,
            }),
            _ => false,
        };
        assert!(
            opens_door,
            "Cine2's authored action list must contain the 'opendoor porte30' opener"
        );
        // The controller advanced past action index 12 (the opener): after executing action 12
        // `Interpret` increments `ScriptedActionIndex`, so a value > 12 proves the cine reached
        // and ran `opendoor porte30`.
        let cine_action_index = match vm.get_property(guard, "CineController") {
            Some(Value::Object(Some(xiii_script::ObjRef::Instance(c)))) => {
                match vm.get_property(*c, "ScriptedActionIndex") {
                    Some(Value::Int(i)) => *i,
                    _ => -1,
                }
            }
            _ => -1,
        };
        assert!(
            cine_action_index > 12,
            "the guard cine must advance past the `opendoor porte30` action (index 12): \
             {cine_action_index}"
        );
        // The guard left its authored start (-3360.56,3311.56) for the card-side waypoints.
        let guard_loc = vm.vector_prop(guard, "Location").expect("guard location");
        assert!(
            (guard_loc[1] - 3311.56).abs() > 200.0,
            "the guard cine must carry the guard along its waypoint chain: {guard_loc:?}"
        );
        println!(
            "[prock03 next blocker] the west cell-block corridor and its four ordinary doors \
             (Porte33/35/40/42) are reached by input; the checkpoint DetectionVolume now starts \
             the guard cine, which runs `opendoor porte30` and parks the guard at PositionInfo22 \
             (cine action index {cine_action_index}). `Porte29` (porte_sec, TriggerControl) is \
             still the event-gated checkpoint: `use` runs only the bare `XIIIMover.PlayerTrigger` \
             no-op because `MagneticPassTrigger0` needs a `pass` item the map never grants \
             (`MagneticPassHolder` is None). The magnetic card behind the now-open `Porte30` and \
             the Prock04a travel stay unreached; health={:?}.",
            session.player_health()
        );
    }

    /// item103d acceptance: the PRock03 guard-key -> magnetic-card -> `porte_sec` chain driven
    /// entirely by player input, with **no teleports** (the previous labelled diagnostics are
    /// removed). The natural west-corridor route reaches the `porte_sec` checkpoint, which touches
    /// the guard detection volume `DetectionVolume7` (`Event 'gardien_lostpass'`); the guard
    /// `Cine2` cine then runs its action 12 `opendoor porte30`, so the player walks north through
    /// the now-open `Porte30` to the magnetic card `Hual02MagneticCardPick0`, walks over it
    /// (`Hual02MagneticCard`, Event `pass`) and returns to the checkpoint. Because the cine already
    /// opened `Porte30`, `XIDMaps.PRock03.Trigger` never grants the `clefgarde` key (its grant is
    /// gated on `DoorToOpen` still being `Locked`); the key is the alternative path, not a retail
    /// requirement. The carried card, read on `MagneticPassTrigger0` through
    /// `XIIIPawn.FindPowerUpItemNameForTrigger`, opens the `TriggerControl` checkpoint `Porte29`.
    /// The route then continues toward the objectives (hostage `Johansson3` / `CellKey`) and the
    /// `Prock04a` travel as far as the map allows.
    ///
    /// The `Porte30` leaf is a `PlayerTriggerToggle` mover: walking through it toggles it shut, so
    /// the return leg re-opens it with the player's own `use` before stepping through.
    #[test]
    fn opt_in_prock03_guard_key_card_chain() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("PRock03".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import PRock03");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text = include_str!("../../tests/data/prock03_key_chain.script");
        assert!(
            !text.lines().any(|line| {
                let command = line.split('#').next().unwrap_or("").trim();
                let word = command.split_whitespace().nth(1);
                [
                    "teleport",
                    "place",
                    "weapon",
                    "item",
                    "set_goal",
                    "take_control",
                ]
                .iter()
                .any(|forbidden| word == Some(*forbidden))
            }),
            "item103d: the on-foot chain must be input-only, with no teleports or host bridges"
        );
        let script = script::Script::parse(text).expect("parse PRock03 key-chain probe");
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "PRock03",
            &script,
            &resolved.params,
            &scene,
            215.0,
        )
        .expect("run PRock03 key-chain probe");
        let session = &outcome.session;
        let vm = session.vm();
        println!(
            "[prock03 key] error={:?} objectives={:?} travel={:?} position={:?} health={:?}",
            session.first_error(),
            session.objective_states(),
            outcome.travel,
            session.player_location(),
            session.player_health()
        );
        println!(
            "[prock03 key] inventory={:?}",
            session
                .inventory_items()
                .iter()
                .map(|(n, c)| format!("{n}[{c}]"))
                .collect::<Vec<_>>()
        );
        assert!(
            session.first_error().is_none(),
            "unexpected blocker: {:?}",
            session.first_error()
        );
        assert!(
            session.player_health().is_some_and(|h| h > 0.0),
            "the player must survive the key chain"
        );
        // --- item103d on-foot positive pins -------------------------------------------------
        // The guard cine opened Porte30 (`opendoor porte30`); it is not the player's key use.
        // (`bOpened` is left by the player's own later `use`/walk-through, so the pins are the
        // state leaving `Locked` plus the cine action index, below.)
        let porte30 = vm.find_object("Porte30").expect("Porte30");
        assert!(
            !vm.is_in_state(porte30, "Locked"),
            "the guard cine's `opendoor porte30` must leave Porte30 not Locked: {:?}",
            vm.state_name(porte30)
        );
        // The cine reached and ran action 12 (`opendoor porte30`). item100k: with the retail
        // `AController::ShowSelf` delivery (Engine.dll 0x1036df40 walks `Level.ControllerList`
        // and delivers `SeePlayer` to every controller with negative `SightCounter` and an
        // enabled `SeePlayer` probe; there is no class filter) the guard `Cine2` -- a
        // `BaseSoldier` -- also receives `SeePlayer`. Its authored `Cine2.CineInit.SeeXIII`
        // reaction (`Play` -> `CineController.StartSequence`) then restarts the cine with the
        // `tabActions2` list (`bevisible`, `destroy touchtrigger2`, ...), so the final
        // `ScriptedActionIndex` is that reaction's 5 actions rather than the opener's. Measured
        // (`local/re/item100j/guardidx.log`): the index reaches 12 -> 13 at 49.22 s (action 12
        // `opendoor porte30`), 15/16 at 52.8 s and 17..19 at 67.7 s, then the 68.9 s SeePlayer
        // resets it to tab 2 and ends at 5. The opener still ran, so pin the guard's own
        // `PlayerTrigger` on the door first and then the measured post-reaction index.
        let guard_triggered_door = vm.trace.iter().any(|event| {
            matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, args }
                    if target.eq_ignore_ascii_case("Porte30")
                        && function.ends_with("PlayerTrigger")
                        && args.iter().any(|a| a == "Cine2")
            )
        });
        assert!(
            guard_triggered_door,
            "the guard `Cine2`'s action 12 `opendoor porte30` must fire the door's own \
             PlayerTrigger"
        );
        let guard = vm
            .find_object("Cine2")
            .expect("gardien_lostpass key holder");
        let cine_action_index = match vm.get_property(guard, "CineController") {
            Some(Value::Object(Some(xiii_script::ObjRef::Instance(c)))) => {
                match vm.get_property(*c, "ScriptedActionIndex") {
                    Some(Value::Int(i)) => *i,
                    _ => -1,
                }
            }
            _ => -1,
        };
        assert_eq!(
            cine_action_index, 5,
            "item100k: the retail SeePlayer delivery runs the guard's SeeXIII reaction, which \
             restarts the cine with `tabActions2` (5 actions), so ScriptedActionIndex is 5 even \
             though action 12 `opendoor porte30` already ran (proven by the Porte30 state and the \
             guard's own PlayerTrigger above)"
        );
        // `XIDMaps.PRock03.Trigger` grants the `clefgarde` key only while `DoorToOpen` (Porte30)
        // is still `Locked`. The cine opened it, so the grant was skipped and the player carries
        // no `clefgarde` key on the on-foot path; the key is the alternative path, not a retail
        // requirement.
        let carried_key = (0..vm.objects.len() as xiii_script::ObjectId).find(|&i| {
            let o = &vm.objects[i as usize];
            !o.deleted
                && vm.is_a(i, "keys")
                && matches!(
                    vm.get_property(i, "Owner"),
                    Some(Value::Object(Some(xiii_script::ObjRef::Instance(owner))))
                        if *owner == session.player
                )
        });
        assert!(
            carried_key.is_none(),
            "the on-foot path must skip the clefgarde grant: the player must carry no key"
        );
        // The walk-over card pickup put the `pass` item into the game's inventory.
        assert!(
            session
                .inventory_items()
                .iter()
                .any(|(item, _)| item.eq_ignore_ascii_case("Hual02MagneticCard")),
            "the player must carry Hual02MagneticCard after the pickup"
        );
        // The aimed reader use matched the card and its OutEvent opened the TriggerControl
        // checkpoint (`Mover.TriggerControl.Open`).
        let security = vm.find_object("Porte29").expect("porte_sec security door");
        assert_eq!(
            vm.get_property(security, "bOpened"),
            Some(&Value::Bool(true)),
            "the porte_sec checkpoint must open through MagneticPassTrigger0's OutEvent"
        );
        // item103d Task 3: the player is still alive and the game has not ended (no death or
        // `GameEnded` pin).
        assert!(
            !session.player_input_ended(),
            "the game must not end (death or GameEnded) on the on-foot chain"
        );
        // The objectives keep their authored states over the chain (the anti-goal still holds).
        let goals = session.objective_states();
        assert_eq!(goals.len(), 3, "three authored objectives: {goals:?}");
        assert!(
            goals[0].primary && !goals[0].completed,
            "goal 0 stays the open primary objective: {goals:?}"
        );
        assert!(!goals[1].completed, "goal 1 stays open: {goals:?}");
        assert!(
            goals[2].primary && goals[2].anti_goal && goals[2].completed,
            "the anti-goal (do not kill the medical staff) must still hold: {goals:?}"
        );
        assert!(
            outcome.travel.is_empty(),
            "the map does not travel on this route: {:?}",
            outcome.travel
        );
        // item103e Task 3: the continuation now crosses the `prsas02` airlock. The carried card is
        // read on the aimed `MagneticPassTrigger3` (OutEvent `Door2`, opening the `Door2` movers
        // Porte27/Porte26); the player then crosses the open gap onto the east side and follows
        // the canonical nav corridor south along x~-2600 through the `PorteAlarme2` alarm door
        // `Porte11` (opened with the player's own `use`).
        // item107d: with the retail `ULevel::MoveActor` bump path the patrolling ward soldier
        // `BaseSoldier6` opens the ward doors it presses against (`IAController.NotifyBump` ->
        // `XIIIPorte.PlayerTrigger`) and the central-south ward is awake by the time the player
        // arrives; measured, the player then takes a rifle hit at x~-2060 and bleeds 12 hp/s down
        // to 6 hp at the old stop (-1684,-277). The route therefore stops at the last measured
        // 150-hp waypoint `goto -2159 204` (measured stop (-2138.8,204.2)), east of the alarm
        // doors and short of the deeper ward. The hostage cell / `Johansson3` (behind the
        // `NMI_Otage10`-gated Porte15/Porte16) and the `Prock04a` travel stay unreached on this
        // route; the player is alive and the game not ended (asserted above). The measured stop is
        // pinned exactly (no tolerance-widening).
        let position = session.player_location().expect("player location");
        assert!(
            (position[0] - -2138.8).abs() < 2.0
                && (position[1] - 204.2).abs() < 2.0
                && (position[2] - 75.0).abs() < 2.0,
            "the route must stop at the last measured safe waypoint (-2138.8,204.2): {position:?}"
        );
        // Positive pins for the advance past the old prsas02 pin: the player crossed the
        // airlock's `Door2` movers and the `PorteAlarme2` alarm doors (Porte11 opened by the
        // player's own `use`, Porte12 by the alert cine) and stands east of the alarm doors.
        assert!(
            position[0] > -2200.0,
            "the route must cross the airlock and alarm doors: {position:?}"
        );
        let porte27 = vm.find_object("Porte27").expect("airlock south leaf");
        let porte26 = vm.find_object("Porte26").expect("airlock north leaf");
        assert!(
            !vm.is_in_state(porte27, "Locked") && !vm.is_in_state(porte26, "Locked"),
            "the card reader's `Door2` OutEvent must leave both airlock leaves out of Locked: \
             {:?} {:?}",
            vm.state_name(porte27),
            vm.state_name(porte26)
        );
        // The player takes no hostile damage before the measured stop; this is the "stop before a
        // fatal point" pin (item107d: the awakened ward fires down the corridor past x~-2060, so
        // the stop moved east of it; measured 150 hp).
        assert!(
            session.player_health().is_some_and(|h| h >= 140.0),
            "the route must stop before the hostile ward: {:?}",
            session.player_health()
        );
        println!(
            "[prock03 key] on-foot chain complete: cine action index {cine_action_index}, Porte30 \
             not Locked (no key grant), card carried, porte_sec opened; the route crossed the \
             airlock and the `PorteAlarme2` alarm doors and stops at the last 150-hp waypoint \
             -2138.8,204.2 (item107d: the door-bump path wakes the ward, which fires past x~-2060). \
             The `NMI_Otage10`-gated hostage cell and the Prock04a travel stay unreached. \
             Porte27 state={:?}. health={:?}.",
            vm.state_name(porte27),
            session.player_health()
        );
    }

    /// PRock04b first input route (item109): normal standalone start -> the start-cave
    /// planks -> the yard pass -> TyrolTrigger0 -> the far mine-roof shelf.
    ///
    /// Map facts measured for the route: one primary objective
    /// (`Jones has hidden an airplane away. Find it.`), completed by the canyon
    /// `objectif0` goal trigger (`TouchTrigger1` at 11195,187,-552 fires
    /// `TriggerEvent('objectif0')` -> `XIIIGoalTrigger6`); `MapInfo.NextMapLevelWithUnr`
    /// is `spads01.unr`. The `Fini` goal trigger at the start cave is not on that event
    /// path. The route uses ordinary input only (fists on the plank movers, crouch under
    /// the last plank, the named tyroline interaction).
    ///
    /// Measured stop (see `local/reports/item109-prock04b.md`): the tyroline release puts
    /// the pawn on the shelf above the mine gallery at (18437.7,-3298.9,1359.9), 150 HP,
    /// PlayerWalking. The next stretch is not survived by this standalone save: the slope
    /// carries the pawn west off the shelf, the fall takes 114 HP, and the canyon camp then
    /// kills the unarmed player. The route therefore stops before that point and pins the
    /// incomplete objective, the absent travel and the surviving position.
    #[test]
    fn opt_in_prock04b_route_objectives_and_travel() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let opts = Options {
            map: Some("PRock04b".to_owned()),
            game_dir: Some(game_dir.clone()),
            ..Default::default()
        };
        let scene = viewer::load_scene(&opts).expect("import PRock04b");
        let resolved = resolve_params(&game_dir).expect("resolve player parameters");
        let text = include_str!("../../tests/data/prock04b_route.script");
        let script = script::Script::parse(text).expect("parse PRock04b route");
        assert!(
            script.events.iter().all(|event| !matches!(
                event.command,
                script::Command::Teleport(_)
                    | script::Command::Weapon(_)
                    | script::Command::Item(_)
                    | script::Command::SetGoal(_)
                    | script::Command::TakeControl
            )),
            "the route must use ordinary input only"
        );
        let outcome = run_script_with_cinematic_input(
            &game_dir,
            "PRock04b",
            &script,
            &resolved.params,
            &scene,
            76.0,
        )
        .expect("run PRock04b route");
        let session = &outcome.session;
        let vm = session.vm();
        println!(
            "[prock04b] error={:?} goals={:?} travel={:?} position={:?} health={:?}",
            session.first_error(),
            session.objective_states(),
            outcome.travel,
            session.player_location(),
            session.player_health()
        );
        survey::print_details(&survey::extract("PRock04b", &outcome));
        assert!(
            session.first_error().is_none(),
            "{:?}",
            session.first_error()
        );
        assert!(session.suspended.is_empty(), "{:?}", session.suspended);
        assert!(session.blocked.is_empty(), "{:?}", session.blocked);
        assert!(!session.player_input_ended(), "stop before a fatal point");
        assert_ne!(session.player_controller_state(), "GameEnded");
        assert_eq!(
            session.player_health(),
            Some(150.0),
            "the route must reach the tyroline landing unharmed"
        );
        let near = |position: [f32; 3], expected: [f32; 3], tolerance: f32| {
            position
                .iter()
                .zip(expected)
                .all(|(a, b)| (a - b).abs() < tolerance)
        };
        let start = outcome.trace.first().expect("spawn sample").2;
        assert!(
            near(start, [14794.5, -4703.5, 2411.0], 2.0),
            "normal spawn: {start:?}"
        );
        // The start-cave plank wall and the yard plank wall are destroyed by the player's
        // own fists; the route walks through the openings they leave.
        for mover in [
            "BreakAbleMover260",
            "BreakAbleMover258",
            "BreakAbleMover254",
            "BreakAbleMover255",
            "BreakAbleMover19",
            "BreakAbleMover16",
        ] {
            assert!(
                vm.find_live_object(mover).is_none(),
                "{mover} must be destroyed by the route: {:?}",
                vm.find_live_object(mover)
            );
        }
        assert!(
            session
                .touches()
                .iter()
                .any(|(_, actor)| actor == "TyrolTrigger0"),
            "the route must reach TyrolTrigger0: {:?}",
            session.touches()
        );
        assert!(
            vm.trace.iter().any(|event| matches!(
                &event.kind,
                xiii_script::TraceKind::Event { target, function, .. }
                    if target == "XIIIPlayerController" && function.ends_with("GoTyroling")
            )),
            "the tyroline must run GoTyroling"
        );
        // Measured far-end landing (the tyroline release overshoots the NavPoint by a tick or
        // two; 40 UU is the same tolerance as the PRock04a tyrol mechanism test).
        let position = session.player_location().expect("player location");
        assert!(
            near(position, [18437.7, -3298.9, 1359.9], 40.0),
            "the tyroline must reach the mine-roof shelf: {position:?}"
        );
        assert_ne!(
            session.player_physics(),
            Some(2),
            "the stop must not be a falling state"
        );
        assert_eq!(
            session.player_controller_state(),
            "PlayerWalking",
            "the tyroline must hand back to walking"
        );
        // The objectives/travel pin. `XIIIGoalTrigger6` (Tag `objectif0`) is fired only by
        // `TouchTrigger1` at the canyon floor; the route stops far above it, so goal 0 is
        // still primary and incomplete and the map's DoTravel never runs.
        let goals = session.objective_states();
        assert_eq!(goals.len(), 1);
        assert!(goals[0].primary && !goals[0].anti_goal);
        assert_eq!(goals[0].text, "Jones has hidden an airplane away. Find it.");
        assert!(
            !goals[0].completed,
            "the canyon goal stays unreached: {goals:?}"
        );
        assert!(outcome.travel.is_empty(), "PRock04b travel remains blocked");
        let map_info = vm
            .find_object("PRock04b0")
            .expect("map info / travel owner");
        assert_eq!(
            vm.get_property(map_info, "NextMapLevelWithUnr"),
            Some(&Value::Str("spads01.unr".to_owned()))
        );
        let fini = vm
            .find_object("XIIIGoalTrigger14")
            .expect("start-cave Fini goal trigger");
        assert_eq!(
            vm.get_property(fini, "Tag"),
            Some(&Value::Name("Fini".to_owned())),
            "the start-cave Fini trigger is not the canyon objective"
        );
    }
}
