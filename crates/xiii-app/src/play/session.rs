//! Script-VM session for `--play`: the bridge between the Bevy movement simulation and the
//! `xiii-script` interpreter.
//!
//! **Ownership (reviewer decision).** The Bevy movement simulation owns the player pawn's
//! `Location`/`Velocity`/`Rotation`; each fixed step the host writes them into the VM before the
//! VM ticks (see [`Session::step`]). The VM owns script state and every other actor's properties;
//! actors the VM moves are reported in [`Session::moved`] so the renderer can follow them one way.
//!
//! The whole level is loaded and **every** map actor is marked active, then the level-start
//! lifecycle runs tolerantly ([`xiii_world::runtime::begin_play_all`]): an actor whose code hits
//! an unimplemented native is suspended and counted, and the rest keeps running. This is what
//! makes a play window survive the still-partial native layer.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::Read;
use std::path::Path;
use std::rc::Rc;
use std::time::Instant;

use xiii_package::Limits;
use xiii_script::vm::MoverState;
use xiii_script::{
    DialogueEvent, ObjRef, ObjectId, PresentationEvent, SaveCheckpointEvent, ScriptSet,
    TravelRequest, Value, Vm, VmError, VmLimits,
};
use xiii_world::runtime::{self, ProviderSpec};

use crate::collision;

/// Fallback player pawn class when the GameInfo default cannot be read (never a silent swap: the
/// reason is recorded in [`Session::blocked`]).
const PLAYER_PAWN_FALLBACK: &str = "XIII.XIIIPlayerPawn";

/// Unreal rotator units per full turn (UE1/UE2).
const ROTATOR_UNITS_PER_TURN: f32 = 65536.0;

/// One actor the VM moved this step: display name and the Unreal-space position delta (UU).
pub type MovedActor = (String, [f32; 3]);

/// A running script VM over one map.
pub struct Session {
    vm: Vm<'static>,
    /// The player pawn whose physics state the movement simulation owns.
    pub player: ObjectId,
    /// Player pawn display name (VM `Instance.name`).
    pub player_name: String,
    /// Player controller, from the script path or the explicit bootstrap.
    pub controller: Option<ObjectId>,
    /// Spawned GameInfo, when begin-play reached it.
    pub game_info: Option<ObjectId>,
    /// Human-readable description of how the player pawn was created.
    pub bootstrap_note: String,
    /// 1 when the script login chain created the player pawn, 0 otherwise.
    pub login_script: u32,
    /// 1 when the explicit harness bootstrap created the player pawn, 0 otherwise.
    pub login_bootstrap: u32,
    /// Script path attempts that failed before the bootstrap (native + stack).
    pub blocked: Vec<String>,
    /// Suspended actor names (unimplemented native or other code failure).
    pub suspended: Vec<String>,
    /// Every distinct suspension as `(actor, error)`, in first-seen order (bounded). More useful
    /// than `first_error` alone: several actors can fail for different reasons in one run.
    pub failures: Vec<(String, String)>,
    /// First failure, formatted with its script stack.
    pub first_error: Option<String>,
    /// Last synced VM `Location` per object id (dense, for the one-way render sync). `None` until
    /// an actor is first seen.
    last_synced: Vec<Option<[f32; 3]>>,
    /// Presentation events, most recent last (bounded).
    pub events: VecDeque<(f64, PresentationEvent)>,
    /// `PlayStrVoice` dialogue events, most recent last (bounded). `dialogue_total` is the
    /// cumulative count so a consumer can detect new entries after the bounded window wraps.
    pub dialogues: VecDeque<(f64, DialogueEvent)>,
    /// Cumulative number of dialogue events emitted.
    pub dialogue_total: u64,
    /// `Actor.SaveAtCheckpoint` requests, most recent last (bounded). The VM records these and
    /// writes nothing to the installation; `save_total` is the cumulative count.
    pub saves: VecDeque<(f64, SaveCheckpointEvent)>,
    /// Cumulative number of checkpoint-save requests emitted.
    pub save_total: u64,
    /// `Touch` events involving the player, most recent last (bounded).
    pub touches: VecDeque<(f64, String)>,
    player_touching: Vec<ObjectId>,
    /// Actors the VM moved this step (cleared at the start of [`Session::step`]).
    pub moved: Vec<MovedActor>,
    /// Level-travel request observed by the VM (item15), if any. Consumed by
    /// [`Session::take_travel_request`]; the host owns the map reload.
    pub travel: Option<TravelRequest>,
    /// `XIIIDispatcher0`, if the map has one (the trigger chain's end state).
    pub dispatcher: Option<ObjectId>,
    /// Active language code of the install's localisation files (`int`, `frt`, ...).
    pub localization_language: String,
    /// Number of `localized` class-default values filled from the `.int` files while building
    /// the level's class layouts.
    pub localized_overrides: u64,
    /// item18: number of Bink clips found next to the installation whose header was read.
    pub video_clips: usize,
    /// item18: number of those clips whose frames/fps gave a finite duration.
    pub video_timed: usize,
    /// Host per-bone hit-zone provider installed on the VM (item14b). The shared table is
    /// refreshed each fixed step from every live pawn's decoded skeleton and its VM animation.
    pub hit_zones: xiii_world::hitbox::PosedHitZones,
    /// One posed mesh per live pawn with a decoded `SkeletalMesh` (shared per mesh path).
    hitbox_meshes: Vec<(ObjectId, Rc<xiii_world::hitbox::HitBoxMesh>)>,
    /// Hit boxes dropped at decode because their bone index is outside the skeleton.
    pub hitbox_dropped: usize,
    /// Pawn meshes that failed to decode for hit boxes: `(actor, mesh, error)`.
    pub hitbox_errors: Vec<String>,
    /// AI perception events dispatched by the host sight bridge (item14b): `(time, controller,
    /// event)`, oldest first (bounded). `SeePlayer`/`EnemyNotVisible` only.
    pub perception_log: VecDeque<(f64, String, String)>,
    /// Fixed steps run.
    pub tick_count: u64,
    /// Set once the end-game has stopped the active cutscene controllers (item15 bridge): the
    /// decoded `CineController2.Interpret` keeps `GotoState('NoControl')`-ing the player, which
    /// would clobber `XIIIPlayerController.GameEndedSuccess` if the intro were still running when
    /// the level ends.
    cine_stopped: bool,
    /// Minimal Canvas used only by the headless route to drive the engine's render-phase script
    /// gates. The windowed renderer owns its own Canvas through `hud::setup`.
    render_canvas: Option<ObjectId>,
    /// Pawn pose owned by a script cutscene this step; the host movement sim copies it back after
    /// `step` so it resumes from the script's final pose rather than snapping to its stale pose.
    script_pawn_sync: Option<([f32; 3], f32, [f32; 3])>,
}

/// Host-owned player movement fields published to the VM pawn each fixed tick (item7b).
///
/// The simulation owns the values; the VM reads them in the pawn's own code (`Landed`,
/// `TakeFallingDamage`, states). `landed_velocity_z` is `Some` on the tick the player touches
/// down, so `Pawn.Landed(HitNormal)` sees the impact velocity in `Pawn.Velocity.Z`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerVMModes {
    /// `bIsCrouched` / `bWantsToCrouch`.
    pub crouched: bool,
    /// `bUnderWater`.
    pub in_water: bool,
    /// UE2 `EPhysics` byte (`PHYS_Walking`, `PHYS_Falling`, `PHYS_Swimming`, `PHYS_Ladder`).
    pub physics: u8,
    /// Downward velocity (Unreal units/s) at the landing, when the player landed this tick.
    pub landed_velocity_z: Option<f32>,
    /// Floor normal (Unreal axes) for the `Landed(HitNormal)` argument.
    pub floor_normal: [f32; 3],
}

impl Default for PlayerVMModes {
    fn default() -> Self {
        Self {
            crouched: false,
            in_water: false,
            physics: crate::play::sim::PHYS_WALKING,
            landed_velocity_z: None,
            floor_normal: [0.0, 0.0, 1.0],
        }
    }
}

/// Outcome of a host use action on a mover/door.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UseOutcome {
    /// The named actor is not a live mover.
    NotAMover,
    /// A locked door with no available key (its own `Locked` behavior ran).
    Locked,
    /// A locked door was unlocked with a key (its state left `Locked`).
    Unlocked,
    /// An (unlocked) door was triggered to open/close.
    Triggered,
    /// A dead pawn in reach was searched (the game's own `PlayerController.SearchPawn`), which
    /// transfers its inventory to the player.
    CorpseSearched,
    /// The script raised on the transition.
    Error(String),
}

/// item18: one `MapInfo.Objectif[]` entry (the `XIIIGoals` struct) read from the live VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObjectiveState {
    /// Objective index (`Objectif[i]`).
    pub index: usize,
    /// `GoalText` (localised display text).
    pub text: String,
    /// `bCompleted`.
    pub completed: bool,
    /// `bPrimary` (the objective is currently shown/validated).
    pub primary: bool,
    /// `bAntiGoal` (completing it fails the mission).
    pub anti_goal: bool,
}

/// Outcome of a host fire action (item14).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FireOutcome {
    /// The player's weapon ran its own `Fire` path (trace/projectile/damage by script).
    Fired,
    /// The player pawn has no `Weapon`: nothing to fire.
    NoWeapon,
    /// The script raised on the fire path (the error carries the VM stack).
    Error(String),
}

impl Session {
    /// Loads the map's script set, builds and installs the real map providers, activates every
    /// map actor, runs the level start tolerantly and creates the player pawn (script path when
    /// it works, explicit harness bootstrap otherwise).
    pub fn open(game_dir: &Path, map: &str) -> Result<Session, String> {
        Self::open_inner(game_dir, map, None)
    }

    /// Opens a map as a checkpoint resume, asking its level-start scripts to see the game's
    /// `StartSpotEvent=LOAD` condition before any placed actor's startup callbacks execute.
    pub fn open_checkpoint(game_dir: &Path, map: &str) -> Result<Session, String> {
        Self::open_inner(game_dir, map, Some("LOAD"))
    }

    fn open_inner(
        game_dir: &Path,
        map: &str,
        start_event: Option<&str>,
    ) -> Result<Session, String> {
        let (set, map_idx) = runtime::load_with_map(game_dir, map)?;
        let set: &'static ScriptSet = Box::leak(Box::new(set));
        let mut vm = Vm::new(set, VmLimits::default());

        // item18: the VM has no filesystem access, so the host reads each Bink clip's header
        // (frames / fps) and registers the real duration. `VideoPlayer.GetStatus` then times the
        // level-end clip instead of reporting it finished immediately. Only the `--play` session
        // registers durations, so the `--menu` path keeps its instant `GetStatus`.
        let (video_clips, video_timed) = register_video_durations(&mut vm, game_dir);

        // Install the install's localisation files before the first class layout is built, so
        // `localized` class defaults (for example `Plage01CahuteKeyPick.PickupMessage`) are
        // filled from the active-language `.int` as the game's classes load them.
        let localization_language = runtime::configure_localization(&mut vm, game_dir)?;
        // Decoded `USize`/`VSize` for textures in non-script packages (the HUD widgets read the
        // HUD's `FondMsg` texture size while drawing).
        runtime::configure_external_objects(&mut vm, game_dir);

        let anim_log = Rc::new(RefCell::new(Vec::new()));
        let providers = runtime::build_map_providers(
            game_dir,
            map,
            &ProviderSpec {
                physics: true,
                animation: true,
                navigation: true,
            },
            &anim_log,
        )?;
        providers.install(&mut vm, |_k: xiii_script::TraceKind| {});

        let actors = vm
            .load_level(map_idx, &Limits::default())
            .map_err(|e| e.to_string())?;
        for &id in &actors {
            vm.set_active(id, true);
        }

        let default_game = runtime::default_game_from_ini(game_dir)
            .ok_or_else(|| "Default.ini has no [Engine.Engine] DefaultGame".to_owned())?;
        let game_class = runtime::resolve_class_path(set, &default_game).ok_or_else(|| {
            format!("DefaultGame {default_game} did not resolve to a loaded class")
        })?;

        // The runtime owns the single-player URL; `LevelInfo.GetLocalURL` and
        // `GameInfo.InitGame`/`Login` read it before any actor begins play. No decoded script
        // parses a `?load=` option (`XIIIGameInfo.InitGame` 0x0006..0x004D reads
        // Name/Class/Team; `Engine.GameInfo.InitGame` reads Difficulty/GameSpeed/AccessControl/
        // AdminPassword/GameRules), so the retail `?load=9` option of
        // `PlayerController.QuickLoad` is consumed by the engine's native load path — the port
        // presents the resume through the post-login `StartSpotEvent` set below instead.
        runtime::configure_local_url(&mut vm, game_dir, map);

        let begin = runtime::begin_play_all(&mut vm, &actors, game_class);
        let mut blocked = begin
            .suspended
            .iter()
            .map(|(a, e)| format!("{a}: {e}"))
            .collect::<Vec<_>>();
        let suspended = begin
            .suspended
            .iter()
            .map(|(a, _)| a.clone())
            .collect::<Vec<_>>();
        let game_info = begin.game_info;

        let default_pawn =
            collision::class_own_string_default(set, &default_game, "DefaultPlayerClassName")
                .ok()
                .flatten()
                .unwrap_or_else(|| {
                    blocked.push(format!(
                    "{default_game}.DefaultPlayerClassName missing; using {PLAYER_PAWN_FALLBACK}"
                ));
                    PLAYER_PAWN_FALLBACK.to_owned()
                });
        let player_class = runtime::resolve_class_path(set, &default_pawn)
            .ok_or_else(|| format!("player class {default_pawn} not loaded"))?;

        let (start_loc, start_rot) = find_player_start(&vm).unwrap_or(([0.0; 3], [0; 3]));

        // --- script path: GameInfo.Login -> PostLogin -> RestartPlayer ---------------------
        let mut player: Option<ObjectId> = None;
        let mut controller: Option<ObjectId> = None;
        let mut login_script = 0u32;
        let mut login_bootstrap = 0u32;
        let script_note: String;
        if let Some(gi) = game_info {
            // UE2 `ULevel::SpawnPlayActor`: `GameInfo.Login(Portal, Options, Error)` with the
            // map URL's options, then `GameInfo.PostLogin(NewPlayer)`. `XIIIGameInfo.Login`
            // itself calls `RestartPlayer` when `Level.bLonePlayer` (the solo campaign); the
            // explicit `RestartPlayer` below is only the non-solo/delayed fallback.
            let out = vec![
                Value::Str(String::new()),
                Value::Str(vm.url_options().to_owned()),
                Value::Str(String::new()),
            ];
            match vm.send_event(gi, "Login", out) {
                Ok(Some(Value::Object(Some(ObjRef::Instance(pc)))))
                    if vm.objects.get(pc as usize).is_some_and(|o| !o.deleted) =>
                {
                    controller = Some(pc);
                    let arg = Value::Object(Some(ObjRef::Instance(pc)));
                    if let Err(e) = vm.send_event(gi, "PostLogin", vec![arg]) {
                        blocked.push(format!("GameInfo.PostLogin: {e}"));
                    }
                    player = instance_prop(&vm, pc, "Pawn");
                    if player.is_none() {
                        let arg = Value::Object(Some(ObjRef::Instance(pc)));
                        match vm.send_event(gi, "RestartPlayer", vec![arg]) {
                            Ok(_) => player = instance_prop(&vm, pc, "Pawn"),
                            Err(e) => blocked.push(format!("GameInfo.RestartPlayer: {e}")),
                        }
                    }
                    if player.is_some() {
                        login_script = 1;
                    }
                }
                Ok(other) => blocked.push(format!("GameInfo.Login returned {other:?}")),
                Err(e) => blocked.push(format!("GameInfo.Login: {e}")),
            }
        } else {
            blocked.push("no GameInfo was spawned (InitGame failed)".to_owned());
        }

        let player_name;
        if let Some(p) = player {
            player_name = vm.objects[p as usize].name.clone();
            script_note = format!(
                "player pawn {player_name} created by the script login chain \
                 (GameInfo.Login/RestartPlayer; PostLogin attempted, login_script={login_script})"
            );
            // Keep any controller the script path produced (Login returned one in most cases).
            if controller.is_none() {
                controller = instance_prop(&vm, p, "Controller");
            }
        } else {
            // Explicit bootstrap: spawn the pawn and its controller at the PlayerStart. This is
            // labelled as a harness bootstrap everywhere it is reported.
            let p = vm
                .spawn(player_class, "XIIIPlayerPawn(play)")
                .map_err(|e| e.to_string())?;
            vm.set_property(p, "Location", 0, Value::Vector(start_loc));
            vm.set_property(p, "Rotation", 0, Value::Rotator(start_rot));
            vm.set_active(p, true);
            let ctrl_class = collision::class_layout_of(set, &default_pawn, None)
                .ok()
                .and_then(|l| collision::layout_class(&l, "ControllerClass"))
                .or_else(|| {
                    collision::class_own_string_default(
                        set,
                        &default_game,
                        "PlayerControllerClassName",
                    )
                    .ok()
                    .flatten()
                    .and_then(|path| runtime::resolve_class_path(set, &path))
                });
            if let Some(cc) = ctrl_class {
                let c = vm
                    .spawn(cc, "XIIIPlayerController(play)")
                    .map_err(|e| e.to_string())?;
                vm.set_property(c, "Pawn", 0, Value::Object(Some(ObjRef::Instance(p))));
                vm.set_property(p, "Controller", 0, Value::Object(Some(ObjRef::Instance(c))));
                vm.set_active(c, true);
                controller = Some(c);
            }
            player_name = vm.objects[p as usize].name.clone();
            player = Some(p);
            login_bootstrap = 1;
            let why = if blocked.is_empty() {
                "Login/RestartPlayer returned no pawn".to_owned()
            } else {
                blocked.join("; ")
            };
            script_note = format!(
                "harness bootstrap: spawned {default_pawn} + controller at the PlayerStart \
                 (script path blocked: {why})"
            );
        }
        let player = player.expect("player created above");
        if let Some(event) = start_event {
            // The engine's checkpoint-load path sets GameInfo.StartSpotEvent after the login
            // chain (XIII's own `RestartPlayer` copies `StartSpot.Event` into it first —
            // bytecode 0x037A). `Plage00.FirstFrame`'s guard at 0x0013 then skips re-applying
            // the wounded intro Health when it reads "LOAD". Set it here, once, after spawn.
            vm.set_start_spot_event(event);
        }

        let dispatcher = vm.find_object("XIIIDispatcher0");

        // Baseline for the one-way render sync and the initial touch state. Dense by object id,
        // so the per-tick sync is a vec index rather than a hash-map lookup per actor.
        let mut last_synced: Vec<Option<[f32; 3]>> = vec![None; vm.objects.len()];
        for (i, o) in vm.objects.iter().enumerate() {
            if o.is_actor
                && !o.deleted
                && let Some(l) = vm.location_prop(i as ObjectId)
            {
                last_synced[i] = Some(l);
            }
        }

        // item14b: decode the hit boxes of every live pawn's `SkeletalMesh` (once per mesh path)
        // and install the host hit-zone provider. This is host-side and works headless, so both
        // `--play` and `--play-script` classify bullet hits against the posed body, not the
        // collision cylinder. A mesh that fails to decode is reported; the affected actor keeps
        // the cylinder fallback (never a silent success).
        let (hitbox_meshes, hitbox_dropped, hitbox_errors, hit_zones) =
            build_hit_boxes(&vm, game_dir);
        vm.set_hit_zones(Box::new(hit_zones.clone()));

        let mut session = Session {
            vm,
            player,
            player_name,
            controller,
            game_info,
            bootstrap_note: script_note,
            login_script,
            login_bootstrap,
            blocked,
            suspended,
            failures: Vec::new(),
            first_error: None,
            last_synced,
            events: VecDeque::new(),
            dialogues: VecDeque::new(),
            dialogue_total: 0,
            saves: VecDeque::new(),
            save_total: 0,
            touches: VecDeque::new(),
            player_touching: Vec::new(),
            moved: Vec::new(),
            travel: None,
            dispatcher,
            localization_language,
            localized_overrides: 0,
            video_clips,
            video_timed,
            hit_zones,
            hitbox_meshes,
            hitbox_dropped,
            hitbox_errors,
            perception_log: VecDeque::new(),
            tick_count: 0,
            cine_stopped: false,
            render_canvas: None,
            script_pawn_sync: None,
        };
        session.localized_overrides = session.vm.localized_overrides;
        session.suspended.dedup();
        session.drain_events();
        session.update_touches();
        session.update_hit_boxes();
        Ok(session)
    }

    /// Rebuilds the shared posed hit-box table from every cached pawn's VM animation state. The
    /// provider reads it during `Actor.Trace`; the boxes are the decoded per-bone volumes
    /// (`xiii_world::hitbox`) at the same pose the renderer draws.
    pub fn update_hit_boxes(&self) {
        let handle = self.hit_zones.handle();
        let mut table = handle.borrow_mut();
        table.clear();
        for (id, mesh) in &self.hitbox_meshes {
            let anim = self.vm.actor_animation(*id);
            let clip = anim.as_ref().and_then(|a| {
                let ch = a
                    .channels
                    .iter()
                    .filter(|c| c.active)
                    .min_by_key(|c| c.channel)
                    .or_else(|| a.channels.iter().min_by_key(|c| c.channel))?;
                let set = mesh.anims.as_ref()?;
                let clip = set.clip(&ch.sequence)?;
                Some((clip, ch.frame, ch.looping))
            });
            let loc = self.vm.vector_prop(*id, "Location").unwrap_or([0.0; 3]);
            let rot = self.vm.rotation_prop(*id).unwrap_or([0; 3]);
            let boxes = xiii_world::hitbox::world_boxes(mesh, loc, rot, clip);
            table.insert(*id, boxes);
        }
    }

    /// Pawn hit-box summary line for the startup report: how many pawns have a posed skeleton,
    /// how many boxes, and any decode failures.
    pub fn hitbox_summary(&self) -> String {
        let mut boxes = 0usize;
        for (_, m) in &self.hitbox_meshes {
            boxes += m.boxes.len();
        }
        format!(
            "{} pawn(s) with decoded hit boxes, {} box(es), {} dropped (bone out of range){}",
            self.hitbox_meshes.len(),
            boxes,
            self.hitbox_dropped,
            if self.hitbox_errors.is_empty() {
                String::new()
            } else {
                format!("; failures: {}", self.hitbox_errors.join(", "))
            }
        )
    }

    /// One fixed step, in the documented order: write the player pawn state (owned by the
    /// movement simulation) into the VM, refresh its touches, tick the VM tolerantly, drain the
    /// presentation events and record the actors the VM moved.
    ///
    /// `modes` carries the movement-mode fields the host owns (crouch/water/physics and, on the
    /// landing tick, the impact velocity). When the player landed, the pawn's own
    /// `Landed(HitNormal)` event runs through the VM (`XIIIPawn.Landed` ->
    /// `TakeFallingDamage` -> `TakeDamage`), so falling damage is computed by the game's code.
    pub fn step(
        &mut self,
        dt: f32,
        location: [f32; 3],
        yaw: f32,
        velocity: [f32; 3],
        modes: &PlayerVMModes,
    ) {
        self.moved.clear();
        self.script_pawn_sync = None;
        let script_owned_before = self.script_owns_player_pawn();
        // A cutscene can defer a return-valued inventory callback while it temporarily removes
        // the player pawn from the VM tick set. Once the game's controller state returns to
        // PlayerWalking, the pawn is again a live participant: pickup Touch -> GiveTo ->
        // AddInventory must execute on it rather than being deferred as an out-of-scope event.
        if self
            .controller
            .is_some_and(|pc| self.vm.is_in_state(pc, "PlayerWalking"))
            && !self.vm.objects[self.player as usize].active
        {
            self.vm.set_active(self.player, true);
            println!(
                "[play] t={:.3}s restored local player pawn to VM tick scope after cutscene handoff",
                self.vm.time
            );
        }
        // Refresh the posed hit boxes before the VM traces (host `fire` and any script trace in
        // this tick see the current body pose).
        self.update_hit_boxes();
        let profiling = self.vm.native_profile().enabled;
        let t0 = Instant::now();
        if !script_owned_before {
            let _ = self
                .vm
                .set_property(self.player, "Location", 0, Value::Vector(location));
            // On the landing tick, publish the impact velocity so `TakeFallingDamage` reads it.
            let published_velocity = match modes.landed_velocity_z {
                Some(vz) => [velocity[0], velocity[1], vz],
                None => velocity,
            };
            let _ = self.vm.set_property(
                self.player,
                "Velocity",
                0,
                Value::Vector(published_velocity),
            );
            let yaw_units = (yaw / std::f32::consts::TAU * ROTATOR_UNITS_PER_TURN).round() as i32;
            let _ = self.vm.set_property(
                self.player,
                "Rotation",
                0,
                Value::Rotator([0, yaw_units, 0]),
            );
            // The engine tracks the look direction on the `PlayerController` (`Pawn.GetViewRotation`
            // returns `Controller.Rotation` for a player pawn); the host owns the player's yaw, so
            // write it to the controller too. Without this `XIIIWeapon.RealTraceFire` traces along the
            // controller default rotation (item14).
            if let Some(ctrl) = self.controller {
                let _ =
                    self.vm
                        .set_property(ctrl, "Rotation", 0, Value::Rotator([0, yaw_units, 0]));
            }
            // The host owns the player's movement; keep the held weapon at the eye so the script's
            // damage falloff (`XIIIBulletsAmmo.ProcessTraceHit` measures `HitLocation - W.Location`)
            // sees the muzzle distance, not the weapon's stale spawn position. The engine's
            // `XIIIWeapon.Active.BeginState` does `SetLocation(Instigator.Location + CalcDrawOffset)`.
            if let Some(weapon) = self.player_weapon() {
                let eye = match self.vm.get_property(self.player, "EyeHeight") {
                    Some(Value::Float(h)) => *h,
                    _ => match self.vm.get_property(self.player, "BaseEyeHeight") {
                        Some(Value::Float(h)) => *h,
                        _ => 0.0,
                    },
                };
                let _ = self.vm.set_property(
                    weapon,
                    "Location",
                    0,
                    Value::Vector([location[0], location[1], location[2] + eye]),
                );
            }
            let _ =
                self.vm
                    .set_property(self.player, "bIsCrouched", 0, Value::Bool(modes.crouched));
            let _ = self.vm.set_property(
                self.player,
                "bWantsToCrouch",
                0,
                Value::Bool(modes.crouched),
            );
            let _ =
                self.vm
                    .set_property(self.player, "bUnderWater", 0, Value::Bool(modes.in_water));
            let _ = self
                .vm
                .set_property(self.player, "Physics", 0, Value::Byte(modes.physics));
        }
        if profiling {
            self.vm.native_profile_mut().player_write_micros += t0.elapsed().as_micros() as u64;
        }

        // VM touch update for the host-moved player (the walk into a trigger volume). Run every
        // tick even when the player is stationary: movers, spawned actors and re-enabled
        // collision can change the player's touch set without the player moving.
        let t0 = Instant::now();
        if let Err(e) = self.vm.refresh_touching_of(self.player) {
            self.suspend(self.player, &e);
        }
        if profiling {
            self.vm.native_profile_mut().touch_micros += t0.elapsed().as_micros() as u64;
        }
        self.drain_events();

        // The pawn's own landing path (fall damage is the game's code, not the host's).
        if !script_owned_before && modes.landed_velocity_z.is_some() {
            let arg = Value::Vector(modes.floor_normal);
            if let Err(e) = self.vm.send_event(self.player, "Landed", vec![arg]) {
                self.record_failure("Landed", &e);
            }
            self.drain_events();
        }

        for (id, e) in self.vm.tick_suspending(dt) {
            self.suspend(id, &e);
        }
        // item14b: drive the engine's own AI perception. The host performs the sight test (range /
        // facing / line of sight) and dispatches `SeePlayer`/`EnemyNotVisible`; the soldier's own
        // `IAController` states react (acquire, turn, fire). Run after the VM tick so the
        // controller state machine has advanced this step.
        let mut perception = Vec::new();
        self.vm.update_ai_perception(self.player, &mut perception);
        for (controller, event) in perception {
            let t = self.vm.time;
            println!("[play] AI t={t:.3}s {controller}: {event}");
            self.perception_log.push_back((t, controller, event));
        }
        while self.perception_log.len() > 128 {
            self.perception_log.pop_front();
        }
        self.tick_count += 1;
        let t0 = Instant::now();
        self.drain_events();
        self.update_touches();
        // Stop the cutscene controllers the moment the end-game starts, before the next tick can
        // re-assert `NoControl`/`NoMove` over `GameEndedSuccess`.
        self.stop_cutscenes_if_ended();
        if profiling {
            self.vm.native_profile_mut().events_micros += t0.elapsed().as_micros() as u64;
        }
        let t0 = Instant::now();
        self.update_sync();
        if script_owned_before || self.script_owns_player_pawn() {
            let location = self.vm.vector_prop(self.player, "Location");
            let rotation = self.vm.rotation_prop(self.player);
            let velocity = self.vm.vector_prop(self.player, "Velocity");
            if let (Some(location), Some(rotation), Some(velocity)) = (location, rotation, velocity)
            {
                self.script_pawn_sync = Some((
                    location,
                    rotation[1] as f32 * std::f32::consts::TAU / ROTATOR_UNITS_PER_TURN,
                    velocity,
                ));
            }
        }
        if profiling {
            self.vm.native_profile_mut().sync_micros += t0.elapsed().as_micros() as u64;
        }
    }

    /// Drive render-phase callbacks in the deterministic headless harness. The actual windowed
    /// app calls `HUD.PostRender` from `hud::refresh`; this mirrors that engine callback until the
    /// script HUD opens the one-way `MapInfo.EndCartoonEffect` gate.
    pub fn drive_render_phase(&mut self) {
        let end_cartoon = self.map_info().is_some_and(|mi| {
            matches!(
                self.vm.get_property(mi, "EndCartoonEffect"),
                Some(Value::Bool(true))
            )
        });
        if !end_cartoon {
            let hud = self
                .controller
                .and_then(|pc| instance_prop(&self.vm, pc, "myHUD"))
                .or_else(|| crate::play::hud::find_hud(&self.vm));
            if let Some(hud) = hud {
                let canvas = if let Some(canvas) = self.render_canvas {
                    canvas
                } else {
                    let Some(class) = runtime::resolve_class_path(self.vm.set(), "Engine.Canvas")
                    else {
                        self.blocked
                            .push("headless HUD gate: Engine.Canvas class not loaded".into());
                        return;
                    };
                    let canvas = match self.vm.spawn(class, "HUDCanvas(headless)") {
                        Ok(canvas) => canvas,
                        Err(e) => {
                            self.record_failure("headless HUD Canvas", &e);
                            return;
                        }
                    };
                    for (prop, value) in [
                        ("ClipX", Value::Float(1280.0)),
                        ("ClipY", Value::Float(720.0)),
                        ("CurX", Value::Float(0.0)),
                        ("CurY", Value::Float(0.0)),
                        ("OrgX", Value::Float(0.0)),
                        ("OrgY", Value::Float(0.0)),
                        ("Style", Value::Byte(1)),
                    ] {
                        let _ = self.vm.set_property(canvas, prop, 0, value);
                    }
                    self.render_canvas = Some(canvas);
                    canvas
                };
                let arg = Value::Object(Some(ObjRef::Instance(canvas)));
                if let Err(e) = self.vm.send_event(hud, "PostRender", vec![arg]) {
                    self.record_failure("headless HUD.PostRender", &e);
                }
                self.vm.drain_canvas();
                self.drain_events();
            }
        }
        if let Some(pc) = self.controller
            && self.vm.is_in_state(pc, "WaitForFirstDisplay")
            && let Err(e) = self
                .vm
                .send_event(pc, "RenderOverlays", vec![Value::Object(None)])
        {
            self.record_failure("headless PlayerController.RenderOverlays", &e);
        }
    }

    /// Host movement ownership is temporarily ceded by the Plage01 wake-up script. While this is
    /// true, `BeachInBedWithXIII` owns `Location`/`Rotation` and restores collision before it
    /// returns the controller to PlayerWalking.
    fn script_owns_player_pawn(&self) -> bool {
        self.controller.is_some_and(|pc| {
            self.vm.is_in_state(pc, "NoControl")
                && matches!(
                    self.vm.get_property(self.player, "bCollideWorld"),
                    Some(Value::Bool(false))
                )
        })
    }

    /// Script-driven player pose for the host movement sim to follow during the bed wake-up cine.
    pub fn script_pawn_pose(&self) -> Option<([f32; 3], f32, [f32; 3])> {
        self.script_pawn_sync
    }

    /// VM time in seconds.
    pub fn vm_time(&self) -> f64 {
        self.vm.time
    }

    /// Host bridge (item15): once `GameInfo.bGameEnded` is set, suspend the cutscene controllers.
    /// The decoded `CineController2.Interpret` calls `PC.GotoState('NoControl')`/`'NoMove'` when
    /// its sequence commands run; while a level-start cine is still playing it would clobber
    /// `XIIIPlayerController.GameEndedSuccess` and the level would never travel. The real engine
    /// finishes the intro before the level ends; this restores that ordering for a direct
    /// level-completion call. Visible in the log, never silent.
    fn stop_cutscenes_if_ended(&mut self) {
        if self.cine_stopped {
            return;
        }
        let ended = self.game_info.is_some_and(|gi| {
            matches!(
                self.vm.get_property(gi, "bGameEnded"),
                Some(Value::Bool(true))
            )
        });
        if !ended {
            return;
        }
        self.cine_stopped = true;
        let stopped = self.stop_cutscene_actors();
        if !stopped.is_empty() {
            println!(
                "[play] game ended: stopped {} cutscene controller(s): {}",
                stopped.len(),
                stopped.join(", ")
            );
        }
    }

    /// Suspends every active cutscene actor (`class_chain_contains("cine")`/`"beachinbed"`). The
    /// decoded `CineController2.Interpret` re-asserts `PC.GotoState('NoControl')` each time its
    /// sequence runs, so a stuck level-start cine keeps freezing the player; both the level-end
    /// bridge and the item18 `take_control` bridge stop them. Returns the stopped names.
    fn stop_cutscene_actors(&mut self) -> Vec<String> {
        let mut stopped = Vec::new();
        for i in 0..self.vm.objects.len() {
            let id = i as ObjectId;
            if self.vm.objects[i].deleted || !self.vm.objects[i].is_actor {
                continue;
            }
            let cutscene = self.vm.class_chain_contains(id, "cine")
                || self.vm.class_chain_contains(id, "beachinbed");
            if cutscene && self.vm.objects[i].active {
                self.vm.set_active(id, false);
                stopped.push(self.vm.objects[i].name.clone());
            }
        }
        stopped
    }

    /// Read-only access to the script VM for host-side queries (the `--play` pawn renderer reads
    /// actor locations, rotations, meshes and animation channels; it never mutates the VM).
    pub fn vm(&self) -> &Vm<'static> {
        &self.vm
    }

    /// Arms the VM's optional per-native/section timers (`--perf-natives`).
    pub fn enable_native_timers(&mut self, on: bool) {
        self.vm.enable_native_timers(on);
    }

    /// Mutable access to the script VM for the host HUD refresh (`hud.rs`): create the `Canvas`,
    /// set its clip, call `HUD.PostRender` and drain the recorded draw commands. The fixed-step
    /// movement/VM ordering still owns every simulation field; this only drives the per-frame
    /// presentation call.
    pub fn vm_mut(&mut self) -> &mut Vm<'static> {
        &mut self.vm
    }

    /// Live actors still in the executed scope.
    pub fn active_actors(&self) -> usize {
        self.vm
            .objects
            .iter()
            .filter(|o| o.is_actor && !o.deleted && o.active)
            .count()
    }

    /// Live actors (executed scope or not).
    pub fn live_actors(&self) -> usize {
        self.vm
            .objects
            .iter()
            .filter(|o| o.is_actor && !o.deleted)
            .count()
    }

    /// Every live, placed (`Default__`-excluded) actor whose class is (or derives from)
    /// `XIIIPlayerPawn`. A correct single-player login creates exactly one; more means the login
    /// path spawned duplicates. Used by the duplicate-pawn regression test and the reports.
    pub fn player_pawn_actors(&self) -> Vec<(ObjectId, String)> {
        (0..self.vm.objects.len())
            .filter(|&i| {
                let o = &self.vm.objects[i];
                o.is_actor
                    && !o.deleted
                    && !o.name.starts_with("Default__")
                    && self.vm.is_a(i as ObjectId, "XIIIPlayerPawn")
            })
            .map(|i| (i as ObjectId, self.vm.objects[i].name.clone()))
            .collect()
    }

    /// Current dispatcher state name (`Fin` when the trigger chain completed).
    pub fn dispatcher_state(&self) -> Option<String> {
        self.dispatcher.and_then(|d| self.vm.state_name(d))
    }

    /// Whether the dispatcher is still inside the executed scope (a suspended dispatcher can
    /// still report its last state name, so this distinguishes "stuck" from "cancelled").
    pub fn dispatcher_active(&self) -> bool {
        self.dispatcher
            .and_then(|d| self.vm.objects.get(d as usize))
            .is_some_and(|o| o.active)
    }

    /// Name, current state and suspended flag of every live `Controller` (player and AI), for the
    /// trigger-chain report ("the soldiers' controllers reacting").
    pub fn controller_states(&self) -> Vec<(String, Option<String>, bool)> {
        let mut out = Vec::new();
        for (i, o) in self.vm.objects.iter().enumerate() {
            if o.is_actor && !o.deleted && self.vm.is_a(i as ObjectId, "controller") {
                out.push((o.name.clone(), self.vm.state_name(i as ObjectId), o.active));
            }
        }
        out
    }

    /// The player controller's current state (for the per-tick trace; `-` when there is no
    /// controller).
    pub fn player_controller_state(&self) -> String {
        self.controller
            .and_then(|c| self.vm.state_name(c))
            .unwrap_or_else(|| "-".to_owned())
    }

    /// Name and current state of each soldier, for the trigger-chain report.
    pub fn soldier_states(&self) -> Vec<(String, Option<String>)> {
        let mut out = Vec::new();
        for (i, o) in self.vm.objects.iter().enumerate() {
            if o.is_actor && !o.deleted && o.name.to_ascii_lowercase().contains("basesoldier") {
                out.push((o.name.clone(), self.vm.state_name(i as ObjectId)));
            }
        }
        out
    }

    /// Number of presentation events seen (not just the retained tail).
    pub fn total_events(&self) -> usize {
        self.events.len()
    }

    /// The last `n` presentation events, formatted `[t] event`.
    pub fn recent_events(&self, n: usize) -> Vec<String> {
        self.events
            .iter()
            .rev()
            .take(n)
            .map(|(t, e)| format!("[{t:.3}s] {e}"))
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect()
    }

    /// The last `Touch` involving the player, formatted `[t] actor`.
    pub fn last_touch(&self) -> Option<String> {
        self.touches
            .back()
            .map(|(t, a)| format!("[{t:.3}s] Touch {a}"))
    }

    /// Every recorded `Touch` involving the player, oldest first.
    pub fn touches(&self) -> Vec<(f64, String)> {
        self.touches.iter().cloned().collect()
    }

    /// Current player `Location` (UU).
    pub fn player_location(&self) -> Option<[f32; 3]> {
        self.vm.vector_prop(self.player, "Location")
    }

    /// Current player pawn `Physics` byte (`PHYS_*`).
    pub fn player_physics(&self) -> Option<u8> {
        match self.vm.get_property(self.player, "Physics") {
            Some(Value::Byte(p)) => Some(*p),
            _ => None,
        }
    }

    /// Live item in `id`'s `Inventory` chain, if the property is an object reference.
    fn inventory_head(&self, id: ObjectId) -> Option<ObjectId> {
        match self.vm.get_property(id, "Inventory") {
            Some(Value::Object(Some(ObjRef::Instance(i))))
                if self.vm.objects.get(*i as usize).is_some_and(|o| !o.deleted) =>
            {
                Some(*i)
            }
            _ => None,
        }
    }

    /// `(item name, class path)` of every item in the player pawn's `Inventory` chain
    /// (`Inventory` links), in chain order. This reads the game's own inventory state, not a host
    /// copy: the chain is built by `Pawn.AddInventory` from the real pickup flow. Used by the
    /// opt-in corpus tests and the diagnostic report.
    #[allow(dead_code)]
    pub fn inventory_items(&self) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut cur = self.inventory_head(self.player);
        let mut guard = 0;
        while let Some(id) = cur {
            guard += 1;
            if guard > 256 {
                break;
            }
            let o = &self.vm.objects[id as usize];
            out.push((o.name.clone(), self.vm.set().path(o.class)));
            cur = self.inventory_head(id);
        }
        out
    }

    /// Current player `Rotation` (Unreal rotator units), if the pawn has one.
    pub fn player_rotation(&self) -> Option<[i32; 3]> {
        match self.vm.get_property(self.player, "Rotation") {
            Some(Value::Rotator(r)) => Some(*r),
            _ => None,
        }
    }

    /// Every live mover actor with its current pose (for dynamic collision and the door trace).
    pub fn mover_states(&self) -> Vec<MoverState> {
        self.vm.mover_states()
    }

    /// Registers every live mover's box-collision triangles (world space at its `BasePos`/
    /// `BaseRot`) with the VM's physics provider, so the VM's own `Move`/`Trace` consider the
    /// moving brush. The map collision soup is read from `scene`; the host's player physics is
    /// separate (`movers::MoverCollision`).
    pub fn register_movers(&mut self, scene: &xiii_world::WorldScene) {
        let movers = self.vm.mover_states();
        for m in movers {
            let mut triangles = Vec::new();
            let mut source = 0u32;
            for &i in &scene.collision_box {
                let (tri, src) = scene.collision_triangles[i as usize];
                let Some(actor) = scene
                    .collision_sources
                    .get(src as usize)
                    .and_then(|p| p.split_once(" -> "))
                    .map(|(a, _)| a)
                else {
                    continue;
                };
                if actor.eq_ignore_ascii_case(&m.name) {
                    source = src;
                    triangles.push(tri.map(xiii_world::physics::bevy_to_unreal_position));
                }
            }
            if triangles.is_empty() {
                continue;
            }
            self.vm
                .register_mover(&m.name, source, &triangles, m.base_pos, m.base_rot);
        }
    }

    /// The key item the player carries that unlocks `door`, found by walking the pawn's own
    /// `Inventory` chain (built by `Pawn.AddInventory` from the real `Pickup.Touch` flow) and
    /// matching the door's `UnlockItemCode`/`UnLockItemName` against the item's `KeyCodeName`/
    /// `ItemName`. `None` when the player carries no matching key.
    pub fn carried_key_for(&self, door: ObjectId) -> Option<ObjectId> {
        let code = self.string_prop(door, "UnlockItemCode");
        let name = self.string_prop(door, "UnLockItemName");
        let mut cur = self.inventory_head(self.player);
        let mut guard = 0;
        while let Some(id) = cur {
            guard += 1;
            if guard > 256 {
                break;
            }
            if self.vm.is_a(id, "keys") {
                let key_code = self.string_prop(id, "KeyCodeName");
                let item_name = self.string_prop(id, "ItemName");
                let code_match = code
                    .as_deref()
                    .zip(key_code.as_deref())
                    .is_some_and(|(c, k)| c.eq_ignore_ascii_case(k));
                let name_match = name
                    .as_deref()
                    .zip(item_name.as_deref())
                    .is_some_and(|(n, i)| n.eq_ignore_ascii_case(i));
                if code_match || name_match {
                    return Some(id);
                }
            }
            cur = self.inventory_head(id);
        }
        None
    }

    /// `string`/`name` property value as text (case preserved; names are case-insensitive).
    fn string_prop(&self, id: ObjectId, name: &str) -> Option<String> {
        match self.vm.get_property(id, name) {
            Some(Value::Str(s)) | Some(Value::Name(s)) => Some(s.clone()),
            _ => None,
        }
    }

    /// Host use action (`E` in `--play`, `use` in a script). Mirrors the tail of
    /// `XIIIPlayerController.Grab` for a mover target: a locked `XIIIPorte` is unlocked with the
    /// matching key the player **carries in the game's inventory** via its own `Trigger`
    /// (`TryPickLock`), any other state is opened via `PlayerTrigger`. No host key grant.
    pub fn use_mover(&mut self, target_name: &str) -> UseOutcome {
        let Some(target) = self.vm.find_object(target_name) else {
            return UseOutcome::NotAMover;
        };
        if !self.vm.is_mover(target) {
            return UseOutcome::NotAMover;
        }
        let pawn = self.player;
        let controller = self.controller.unwrap_or(pawn);
        // A user-issued use action is delivered to the local controller and pawn even when an
        // unrelated deferred call previously suspended either from the VM's tick set. In
        // particular, the final truck-door trigger broadcasts GameEndedSuccess to the controller.
        self.vm.set_active(pawn, true);
        self.vm.set_active(controller, true);
        if self.vm.is_in_state(target, "Locked") {
            let Some(key) = self.carried_key_for(target) else {
                // No matching key carried: run the door's own `Locked.PlayerTrigger` (plays the
                // locked sound) and report Locked; this is visible, not a silent success.
                let args = vec![
                    Value::Object(Some(ObjRef::Instance(controller))),
                    Value::Object(Some(ObjRef::Instance(pawn))),
                ];
                if let Err(e) = self.vm.send_event(target, "PlayerTrigger", args) {
                    return UseOutcome::Error(e.to_string());
                }
                return UseOutcome::Locked;
            };
            let args = vec![
                Value::Object(Some(ObjRef::Instance(key))),
                Value::Object(Some(ObjRef::Instance(pawn))),
            ];
            match self.vm.send_event(target, "Trigger", args) {
                Ok(_) => UseOutcome::Unlocked,
                Err(e) => UseOutcome::Error(e.to_string()),
            }
        } else {
            let args = vec![
                Value::Object(Some(ObjRef::Instance(controller))),
                Value::Object(Some(ObjRef::Instance(pawn))),
            ];
            match self.vm.send_event(target, "PlayerTrigger", args) {
                Ok(_) => UseOutcome::Triggered,
                Err(e) => UseOutcome::Error(e.to_string()),
            }
        }
    }

    /// item18: host corpse-search bridge. The engine's `XIIIPlayerController.Grab` reaches
    /// `SearchPawn(Pawn)` when `MyInteraction.bCanSearchCorpse` (the HUD interaction target); the
    /// host has no dynamic-pawn targeting, so a named dead pawn is searched through the
    /// controller's **own** `SearchPawn`, which transfers the corpse's inventory to the player.
    /// Not a no-op: a non-pawn or live target returns [`UseOutcome::NotAMover`].
    pub fn search_corpse(&mut self, target_name: &str) -> UseOutcome {
        let Some(target) = self.vm.find_object(target_name) else {
            return UseOutcome::NotAMover;
        };
        if !self.vm.is_a(target, "pawn") || !self.actor_is_dead(target) {
            return UseOutcome::NotAMover;
        }
        // A player-controlled pawn remains a live participant in the host search action even if
        // an earlier deferred controller/UI call removed it from the VM's tick set. The real
        // SearchPawn -> Transfer -> AddInventory chain needs that scope to link the picked-up key.
        self.vm.set_active(self.player, true);
        // Collect every item the corpse owns: the `Inventory` chain plus any live `Inventory`
        // object whose `Instigator` is the corpse. The second set matters because the VM defers a
        // script call on an out-of-scope actor, so `FirstFrame.GiveSomething -> GiveTo ->
        // AddInventory` can leave the truck key with `Instigator` set but never linked into the
        // chain (measured: `XIII.Keys` owns `Instigator=BaseSoldier6` yet the chain is
        // `Fists -> FistsAmmo`). UE2's `SearchPawn` walks the chain only; the host also picks up
        // the orphaned owner items, transfers each through its own `Transfer` (which fires
        // `cleftueur`), and enforces the unlink so the walk always advances.
        let mut items: Vec<ObjectId> = Vec::new();
        {
            let mut cur = target;
            let mut guard = 0;
            loop {
                guard += 1;
                if guard > 256 {
                    break;
                }
                match self.vm.get_property(cur, "Inventory") {
                    Some(Value::Object(Some(ObjRef::Instance(n)))) => {
                        if !items.contains(n) {
                            items.push(*n);
                        }
                        cur = *n;
                    }
                    _ => break,
                }
            }
        }
        for (i, o) in self.vm.objects.iter().enumerate() {
            let id = i as ObjectId;
            if o.deleted || id == target || items.contains(&id) {
                continue;
            }
            if !self.vm.is_a(id, "inventory") {
                continue;
            }
            if matches!(
                self.vm.get_property(id, "Instigator"),
                Some(Value::Object(Some(ObjRef::Instance(n)))) if *n == target
            ) {
                items.push(id);
            }
        }
        for item in items {
            // Corpse-owned inventory objects can be outside the actor execution scope even though
            // the dead pawn is searchable. `Transfer` is the game's real pickup chain and must run
            // on the key so its `GiveTo`/`AddInventory` code can link it to the live player.
            self.vm.set_active(item, true);
            if let Some(f) = self.vm.class_function(item, "Transfer") {
                let arg = Value::Object(Some(ObjRef::Instance(self.player)));
                if let Err(e) = self.vm.call_function(f, item, vec![arg]) {
                    return UseOutcome::Error(e.to_string());
                }
            }
            self.vm.unlink_inventory(target, item);
        }
        self.drain_events();
        UseOutcome::CorpseSearched
    }

    /// Host use action on a named actor: a mover (lock/unlock/open) or, failing that, a dead pawn
    /// (search). See [`Session::use_mover`] and [`Session::search_corpse`].
    pub fn use_target(&mut self, name: &str) -> UseOutcome {
        match self.use_mover(name) {
            UseOutcome::NotAMover => self.search_corpse(name),
            other => other,
        }
    }

    /// The map's live `MapInfo` object (the `XIDMaps.<Map>` instance), if begin-play spawned one.
    pub fn map_info(&self) -> Option<ObjectId> {
        instance_prop(&self.vm, self.game_info?, "MapInfo")
    }

    /// item18: the map's `Objectif[]` entries as the script sees them, so a run can report the
    /// objective states over time (requirement 4).
    pub fn objective_states(&self) -> Vec<ObjectiveState> {
        let Some(mi) = self.map_info() else {
            return Vec::new();
        };
        // `MapInfo.Objectif` is a dynamic array of `XIIIGoals` structs (read as one `Value::Array`
        // whose elements are `Value::Struct`, see the `opt_in_plage00_objectif_probe` test).
        let Some(Value::Array(elems)) = self.vm.get_property(mi, "Objectif") else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for (index, elem) in elems.iter().enumerate() {
            let Value::Struct(fields) = elem else {
                continue;
            };
            let field = |name: &str| {
                fields
                    .iter()
                    .find(|(k, _)| k.eq_ignore_ascii_case(name))
                    .map(|(_, v)| v)
            };
            let text = match field("GoalText") {
                Some(Value::Str(s)) | Some(Value::Name(s)) => s.clone(),
                _ => String::new(),
            };
            let get_bool = |name: &str| matches!(field(name), Some(Value::Bool(true)));
            out.push(ObjectiveState {
                index,
                text,
                completed: get_bool("bCompleted"),
                primary: get_bool("bPrimary"),
                anti_goal: get_bool("bAntiGoal"),
            });
        }
        out
    }

    /// Reapplies the decoded checkpoint fields and returns the checkpoint actor's Unreal location.
    /// XIII's save semantics are map travel plus checkpoint tag and ThingsToSave fields, not a VM
    /// heap restore. Inventory reconstruction is done through the same `GiveTo` entry point.
    pub fn restore_checkpoint(&mut self, save: &crate::save::SaveFile) -> Result<[f32; 3], String> {
        let tag_match = |v: &Value| match v {
            Value::Name(n) | Value::Str(n) => n.eq_ignore_ascii_case(&save.teleporter),
            _ => false,
        };
        let checkpoint = self
            .vm
            .objects
            .iter()
            .enumerate()
            .find_map(|(i, o)| {
                (!o.deleted && o.is_actor && tag_match(self.vm.get_property(i as ObjectId, "Tag")?))
                    .then_some(i as ObjectId)
            })
            .ok_or_else(|| {
                format!(
                    "checkpoint tag {:?} is absent on map {}",
                    save.teleporter, save.map
                )
            })?;
        let loc = self
            .vm
            .vector_prop(checkpoint, "Location")
            .ok_or_else(|| format!("checkpoint {:?} has no Location", save.teleporter))?;
        self.vm
            .set_property(self.player, "Location", 0, Value::Vector(loc));
        self.vm.set_property(
            self.player,
            "Health",
            0,
            Value::Int(save.health.round() as i32),
        );
        self.vm.set_property(
            self.player,
            "SpeedFactorLimit",
            0,
            Value::Float(save.speed_factor_limit),
        );
        if let Some(gi) = self.game_info {
            self.vm.set_property(
                gi,
                "CheckpointNumber",
                0,
                Value::Int(save.checkpoint_number),
            );
        }
        if let Some(mi) = self.map_info()
            && let Some(Value::Array(mut goals)) = self.vm.get_property(mi, "Objectif").cloned()
        {
            for (i, state) in save.objectives.iter().enumerate() {
                let Some(Value::Struct(fields)) = goals.get_mut(i) else {
                    break;
                };
                for (name, flag) in [
                    ("bCompleted", state.completed),
                    ("bPrimary", state.primary),
                    ("bAntiGoal", state.anti_goal),
                ] {
                    if let Some((_, value)) = fields
                        .iter_mut()
                        .find(|(key, _)| key.eq_ignore_ascii_case(name))
                    {
                        *value = Value::Bool(flag);
                    }
                }
            }
            self.vm.set_property(mi, "Objectif", 0, Value::Array(goals));
        }
        for item in &save.inventory {
            if item
                .class_path
                .eq_ignore_ascii_case("XIII.XIIIThingsToSave")
            {
                continue;
            }
            let class =
                runtime::resolve_class_path(self.vm.set(), &item.class_path).ok_or_else(|| {
                    format!("saved inventory class is not loaded: {}", item.class_path)
                })?;
            let id = self
                .vm
                .spawn_actor(
                    self.player,
                    Some(class),
                    Some(self.player),
                    None,
                    Some(loc),
                    None,
                )
                .map_err(|e| format!("spawning saved inventory {}: {e}", item.class_path))?
                .ok_or_else(|| {
                    format!("saved inventory spawn returned None: {}", item.class_path)
                })?;
            self.vm
                .send_event(
                    id,
                    "GiveTo",
                    vec![Value::Object(Some(ObjRef::Instance(self.player)))],
                )
                .map_err(|e| format!("restoring saved inventory {}: {e}", item.class_path))?;
        }
        Ok(loc)
    }

    /// Captures the minimal ThingsToSave-compatible fields from the live VM.
    pub fn checkpoint_snapshot(
        &self,
        map: &str,
        event: &SaveCheckpointEvent,
        position: [f32; 3],
        rotation: [i32; 3],
    ) -> crate::save::SaveFile {
        let health = self.player_health().unwrap_or(0.0);
        let speed_factor_limit = match self.vm.get_property(self.player, "SpeedFactorLimit") {
            Some(Value::Float(v)) => *v,
            _ => 0.0,
        };
        let checkpoint_number = self
            .game_info
            .and_then(|i| match self.vm.get_property(i, "CheckpointNumber") {
                Some(Value::Int(v)) => Some(*v),
                _ => None,
            })
            .unwrap_or(0);
        let objectives = self
            .objective_states()
            .into_iter()
            .map(|o| crate::save::Objective {
                completed: o.completed,
                primary: o.primary,
                anti_goal: o.anti_goal,
            })
            .collect();
        let inventory = self
            .inventory_items()
            .into_iter()
            .map(|(name, class_path)| crate::save::InventoryItem { class_path, name })
            .collect();
        crate::save::SaveFile {
            map: map.to_owned(),
            teleporter: event.teleporter_name.clone(),
            description: event.description.clone(),
            health,
            speed_factor_limit,
            checkpoint_number,
            location: position,
            rotation,
            objectives,
            inventory,
        }
    }

    /// One compact line of the objective states (goal index, primary/anti/completed flags, text).
    pub fn objective_summary(&self) -> String {
        let states = self.objective_states();
        if states.is_empty() {
            return "objectives: <none>".to_owned();
        }
        states
            .iter()
            .map(|o| {
                format!(
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
                )
            })
            .collect::<Vec<_>>()
            .join(" | ")
    }

    /// First failure formatted with its stack, if any.
    pub fn first_error(&self) -> Option<&str> {
        self.first_error.as_deref()
    }

    /// Calls the map's own `MapInfo.SetGoalComplete(N)` (item15 demonstration bridge). The
    /// decoded campaign reaches this through goal triggers fired by cutscene `TriggerEvent`s the
    /// host does not yet play; calling it runs the game's `TestGoalComplete`/`DoTravel`/`EndGame`/
    /// `ServerTravel` chain. Returns an error string when there is no live `MapInfo`, never a
    /// silent no-op.
    pub fn set_goal(&mut self, n: i32) -> Result<(), String> {
        let gi = self
            .game_info
            .ok_or_else(|| "no GameInfo; cannot resolve MapInfo".to_owned())?;
        let map_info = instance_prop(&self.vm, gi, "MapInfo")
            .ok_or_else(|| "GameInfo.MapInfo is None".to_owned())?;
        let name = self.vm.objects[map_info as usize].name.clone();
        self.vm
            .send_event(map_info, "SetGoalComplete", vec![Value::Int(n)])
            .map_err(|e| e.to_string())?;
        self.drain_events();
        self.stop_cutscenes_if_ended();
        let complete = matches!(
            self.vm.get_property(map_info, "bLevelComplete"),
            Some(Value::Bool(true))
        );
        println!("[play] set_goal {n} via {name}.SetGoalComplete (bLevelComplete={complete})");
        Ok(())
    }

    /// item18 host bridge: hand the local player control by running the game's own
    /// `XIIIPlayerController.EnterStartState` with `bOkForMoving = true`. In the retail game the
    /// HUD sets `bOkForMoving` once the first frame is displayed; the decoded Plage01 intro
    /// instead leaves the controller frozen in `NoControl` (the host does not play its cutscene
    /// sequence), so a windowed run cannot move. This calls the same game function the HUD path
    /// uses; the resulting state is returned. Labelled a bridge in the report, never silent.
    pub fn take_control(&mut self) -> Result<String, String> {
        let c = self
            .controller
            .ok_or_else(|| "no player controller".to_owned())?;
        let _ = self
            .vm
            .set_property(c, "bOkForMoving", 0, Value::Bool(true));
        let f = self
            .vm
            .class_function(c, "EnterStartState")
            .ok_or_else(|| "controller has no EnterStartState".to_owned())?;
        self.vm
            .call_function(f, c, Vec::new())
            .map_err(|e| e.to_string())?;
        // A stuck level-start cutscene re-asserts `NoControl` every tick
        // (`CineController2.Interpret`); stop those controllers so the control holds.
        let stopped = self.stop_cutscene_actors();
        self.drain_events();
        let state = self.vm.state_name(c).unwrap_or_else(|| "<none>".to_owned());
        if state.eq_ignore_ascii_case("NoControl") {
            return Err("EnterStartState did not leave NoControl".to_owned());
        }
        if !stopped.is_empty() {
            println!(
                "[play] take_control: stopped {} cutscene controller(s): {}",
                stopped.len(),
                stopped.join(", ")
            );
        }
        Ok(state)
    }

    /// The player pawn's current `Weapon` object, if any.
    pub fn player_weapon(&self) -> Option<ObjectId> {
        instance_prop(&self.vm, self.player, "Weapon")
    }

    /// `Fire` on the player's weapon through the game's own entry point: the controller's exec
    /// `Fire(1.0)` (`XIIIPlayerController.Fire` -> `Pawn.Weapon.Fire`), or the weapon directly
    /// when the pawn has no controller. The weapon runs its own `ServerFire` ->
    /// `TraceFire`/`ProjectileFire` -> `ProcessTraceHit` -> `TakeDamage` chain (item14).
    pub fn fire(&mut self, yaw: f32, pitch: f32) -> FireOutcome {
        let Some(weapon) = self.player_weapon() else {
            return FireOutcome::NoWeapon;
        };
        // A host fire input can arrive while the VM has suspended the locally controlled pawn
        // and its diagnostic weapon after unrelated deferred inventory/UI calls. Both are live
        // participants in this action; restore their execution scope before running the game's
        // normal aim, reload and trace-fire chain.
        self.vm.set_active(self.player, true);
        self.vm.set_active(weapon, true);
        // The host owns the player view (yaw and pitch); the VM's controller state code does not
        // sync it, so re-assert it here where the script reads `GetViewRotation` (item14). The
        // pitch matters for item14b: a level shot at eye height only ever hits the head box, so
        // the per-bone hit zones cannot be demonstrated (or used) without a vertical aim.
        let pitch_units = (pitch / std::f32::consts::TAU * ROTATOR_UNITS_PER_TURN).round() as i32;
        let yaw_units = (yaw / std::f32::consts::TAU * ROTATOR_UNITS_PER_TURN).round() as i32;
        let _ = self.vm.set_property(
            self.player,
            "Rotation",
            0,
            Value::Rotator([pitch_units, yaw_units, 0]),
        );
        if let Some(ctrl) = self.controller {
            let _ = self.vm.set_property(
                ctrl,
                "Rotation",
                0,
                Value::Rotator([pitch_units, yaw_units, 0]),
            );
            // `XIIIPlayerController.AdjustAim` returns `OldAdjustAim`/`AdjustedAimForFiring` for
            // an instant-hit weapon; the engine refreshes those from the view each frame, which
            // the headless VM does not. Feed the host view direction (item14).
            let (sp, cp) = pitch.sin_cos();
            let (sy, cy) = yaw.sin_cos();
            let dir = Value::Vector([cp * cy, cp * sy, sp]);
            let _ = self.vm.set_property(ctrl, "OldAdjustAim", 0, dir.clone());
            let _ = self.vm.set_property(ctrl, "AdjustedAimForFiring", 0, dir);
            self.vm.set_property(ctrl, "bFire", 0, Value::Byte(1));
            self.vm.set_property(ctrl, "bWeaponMode", 0, Value::Byte(1));
        }
        let target = self
            .controller
            .filter(|c| self.vm.objects.get(*c as usize).is_some_and(|o| !o.deleted));
        let target = target.unwrap_or(weapon);
        // `XIIIWeapon`'s `Active` state defines an empty `Fire` shadow, so the state-aware virtual
        // call from `PlayerController.Fire` is a no-op while the weapon is idle. The engine
        // resolves `Pawn.Weapon.Fire` against the class once the state stops shadowing (e.g. the
        // `Idle` state's own `Fire(0.0)` poll); resolve the class `Fire` explicitly.
        let r = match self.vm.class_function(weapon, "Fire") {
            Some(f) => self
                .vm
                .call_function(f, weapon, vec![Value::Float(1.0)])
                .map(Some),
            None => self.vm.send_event(target, "Fire", vec![Value::Float(1.0)]),
        };
        match r {
            Ok(_) => FireOutcome::Fired,
            Err(e) => FireOutcome::Error(e.to_string()),
        }
    }

    /// Diagnostic weapon bootstrap for `--play-script` (item14/item14c): spawn `class_path` and
    /// run the **game's own** give/equip path — `Weapon.GiveTo(Pawn)` (inventory + ammo +
    /// `ClientWeaponSet`) and, when the pawn already carries a weapon, `Pawn.ChangedWeapon` (the
    /// same switch the game runs for the first weapon or a manual change), which sets
    /// `Pawn.Weapon`, calls `Weapon.BringUp` and `Weapon.AttachToPawn`. Nothing wires
    /// `Pawn.Weapon` directly. The campaign maps start the player with `XIII.Fists`, and
    /// `Weapon.ClientWeaponSet(true)` deliberately does not switch a human-controlled pawn that
    /// already has a weapon, so the explicit `PendingWeapon` + `ChangedWeapon` step is the game's
    /// own switch, not a host shortcut. This grant is reported, never silent, and the
    /// firing/damage/death behaviour is still the game's scripts.
    pub fn grant_weapon(&mut self, class_path: &str) -> Result<String, String> {
        let class = runtime::resolve_class_path(self.vm.set(), class_path)
            .ok_or_else(|| format!("weapon class {class_path} is not loaded"))?;
        let pawn = self.player;
        // `spawn_actor` runs the weapon's own PreBeginPlay/BeginPlay/PostBeginPlay lifecycle.
        let loc = self.vm.vector_prop(pawn, "Location");
        let id = self
            .vm
            .spawn_actor(pawn, Some(class), Some(pawn), None, loc, None)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("spawning {class_path} returned None"))?;
        let p = Value::Object(Some(ObjRef::Instance(pawn)));
        // `GiveTo`/`ClientWeaponSet` read `Owner` (the engine's `Spawn` sets it) and `Instigator`.
        self.vm.set_property(id, "Instigator", 0, p.clone());
        self.vm.set_property(id, "Owner", 0, p.clone());
        // The game's own pickup entry point: adds to the inventory chain, creates the ammo and
        // (only when the pawn has no active weapon) runs `ClientWeaponSet` -> `ChangedWeapon`.
        let give = self.vm.send_event(id, "GiveTo", vec![p.clone()]);
        let active_after_give = self.player_weapon() == Some(id);
        // When the pawn already had a weapon, run the game's own switch: set the field the engine
        // sets and call the pawn's `ChangedWeapon` (the same call `ClientWeaponSet` makes).
        let changed = if active_after_give {
            None
        } else {
            self.vm.set_property(
                pawn,
                "PendingWeapon",
                0,
                Value::Object(Some(ObjRef::Instance(id))),
            );
            Some(self.vm.send_event(pawn, "ChangedWeapon", Vec::new()))
        };
        let name = self.vm.objects[id as usize].name.clone();
        if let Err(e) = &give {
            self.blocked
                .push(format!("grant_weapon {name} GiveTo: {e}"));
        }
        if let Some(Err(e)) = &changed {
            // A failing switch leaves the pawn unarmed: reported, not a silent success.
            self.blocked
                .push(format!("grant_weapon {name} ChangedWeapon: {e}"));
        }
        let weapon = self.player_weapon();
        let attach = weapon.and_then(|w| instance_prop(&self.vm, w, "ThirdPersonActor"));
        Ok(format!(
            "granted {name} ({class_path}) via GiveTo; GiveTo {give:?}, \
             active-after-GiveTo={active_after_give}, ChangedWeapon {changed:?}, \
             Pawn.Weapon={weapon:?}, ThirdPersonActor={attach:?}"
        ))
    }

    /// Equips the best weapon the player already carries in the game's own `Inventory` chain,
    /// through the game's own `Weapon.BringUp` -> `Instigator.ChangedWeapon()` path (item14b).
    /// This is the normal weapon-switch action; it never spawns or grants a weapon. Used after
    /// walking onto a map weapon pickup. Returns the equipped weapon name, or an error naming the
    /// reason (no weapon carried, or the script raised).
    pub fn equip_inventory_weapon(&mut self) -> Result<String, String> {
        let mut cur = self.inventory_head(self.player);
        let mut best: Option<ObjectId> = None;
        let mut guard = 0;
        while let Some(id) = cur {
            guard += 1;
            if guard > 256 {
                break;
            }
            if self.vm.is_a(id, "weapon") {
                // Prefer a real gun over the starting `Fists`.
                if best.is_none() || !self.vm.is_a(id, "fists") {
                    best = Some(id);
                }
            }
            cur = self.inventory_head(id);
        }
        let Some(weapon) = best else {
            return Err("no weapon in the inventory chain".to_owned());
        };
        let name = self.vm.objects[weapon as usize].name.clone();
        // `Weapon.BringUp` sets `Instigator.PendingWeapon` and calls `ChangedWeapon`.
        match self.vm.send_event(weapon, "BringUp", Vec::new()) {
            Ok(_) => Ok(name),
            Err(e) => Err(format!("{name} BringUp: {e}")),
        }
    }

    /// The player weapon's first-person mesh path: the decoded `MeshName` string
    /// (`XIIIWeapon`/`Weapon` load `Mesh` from it in `PostBeginPlay`), falling back to the
    /// resolved `Mesh` object path.
    pub fn weapon_mesh_name(&self, weapon: ObjectId) -> Option<String> {
        if let Some(Value::Str(s)) | Some(Value::Name(s)) = self.vm.get_property(weapon, "MeshName")
            && !s.is_empty()
        {
            return Some(s.clone());
        }
        self.vm.mesh_object(weapon).map(|(p, _)| p)
    }

    /// A vector property of a weapon (`PlayerViewOffset`, `FPMFRelativeLoc`).
    pub fn weapon_vector(&self, weapon: ObjectId, name: &str) -> Option<[f32; 3]> {
        self.vm.vector_prop(weapon, name)
    }

    /// A float property of a weapon (`DrawScale`).
    pub fn weapon_float(&self, weapon: ObjectId, name: &str) -> Option<f32> {
        match self.vm.get_property(weapon, name) {
            Some(Value::Float(v)) => Some(*v),
            Some(Value::Int(v)) => Some(*v as f32),
            _ => None,
        }
    }

    /// Current `Health` of `id` (int or float), if present.
    pub fn actor_health(&self, id: ObjectId) -> Option<f32> {
        match self.vm.get_property(id, "Health") {
            Some(Value::Int(h)) => Some(*h as f32),
            Some(Value::Float(h)) => Some(*h),
            _ => None,
        }
    }

    /// The player pawn's current `Health`, if present.
    pub fn player_health(&self) -> Option<f32> {
        self.actor_health(self.player)
    }

    /// `bIsDead` on `id` (the `XIIIPawn.Died` flag), if present.
    pub fn actor_is_dead(&self, id: ObjectId) -> bool {
        matches!(self.vm.get_property(id, "bIsDead"), Some(Value::Bool(true)))
    }

    fn drain_events(&mut self) {
        for ev in self.vm.drain_events() {
            let t = self.vm.time;
            if let PresentationEvent::Dialogue(d) = &ev {
                self.dialogue_total += 1;
                self.dialogues.push_back((t, d.clone()));
            }
            if let PresentationEvent::SaveCheckpoint(s) = &ev {
                self.save_total += 1;
                self.saves.push_back((t, s.clone()));
            }
            if let PresentationEvent::TravelRequest(r) = &ev {
                // Keep the first un-consumed request; the host reloads on it.
                if self.travel.is_none() {
                    self.travel = Some(r.clone());
                }
            }
            self.events.push_back((t, ev));
        }
        while self.events.len() > 64 {
            self.events.pop_front();
        }
        while self.dialogues.len() > 64 {
            self.dialogues.pop_front();
        }
        while self.saves.len() > 64 {
            self.saves.pop_front();
        }
    }

    /// Takes the pending level-travel request, if any. The VM reports it; the host owns the
    /// reload ([`crate::play::travel`]).
    pub fn take_travel_request(&mut self) -> Option<TravelRequest> {
        // Prefer the VM's own queue (a `ClientTravel` native or the `NextURL` check) in case the
        // event was not drained yet, then the event-captured copy.
        self.vm.take_travel_request().or_else(|| self.travel.take())
    }

    /// Dialogue events emitted since `seen` (a cumulative count). Returns the events in order;
    /// advances `seen` to [`Session::dialogue_total`]. Newest entries survive the bounded window.
    pub fn new_dialogues(&self, seen: &mut u64) -> Vec<&DialogueEvent> {
        if *seen >= self.dialogue_total {
            return Vec::new();
        }
        let new = (self.dialogue_total - *seen).min(self.dialogues.len() as u64) as usize;
        *seen = self.dialogue_total;
        self.dialogues
            .iter()
            .rev()
            .take(new)
            .rev()
            .map(|(_, d)| d)
            .collect()
    }

    fn update_touches(&mut self) {
        let now = match self.vm.get_property(self.player, "Touching") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::Object(Some(ObjRef::Instance(i)))
                        if self.vm.objects.get(*i as usize).is_some_and(|o| !o.deleted) =>
                    {
                        Some(*i)
                    }
                    _ => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        for id in &now {
            if !self.player_touching.contains(id) {
                let name = self.vm.objects[*id as usize].name.clone();
                self.touches.push_back((self.vm.time, name));
                // Host bridge (item15): the one decoded level-end starter
                // (`XIDCine.BeachFinalFall`) derives from `Engine.Triggers`, has no `Touch`, and
                // has no decoded caller; see `fire_touch_event_bridge` and the report.
                self.fire_touch_event_bridge(*id);
            }
        }
        self.player_touching = now;
        while self.touches.len() > 64 {
            self.touches.pop_front();
        }
    }

    /// Reproduces the engine's `Trigger.Touch` for the one decoded actor that starts a level end
    /// but has no decoded starter: `XIDCine.BeachFinalFall` (its `Event` names the goal trigger,
    /// but it derives from `Engine.Triggers`, which has no `Touch`, and no script or native fires
    /// its `'Fall'` tag — see the report). The allow-list is by class name, so no other actor
    /// ever invents an event. Visible in the log on every firing.
    fn fire_touch_event_bridge(&mut self, actor: ObjectId) {
        const ALLOW: &[&str] = &["beachfinalfall"];
        if !ALLOW.iter().any(|c| self.vm.class_chain_contains(actor, c)) {
            return;
        }
        let event = match self.vm.get_property(actor, "Event") {
            Some(Value::Name(n)) if !n.eq_ignore_ascii_case("None") => n.clone(),
            _ => return,
        };
        let args = vec![
            Value::Name(event.clone()),
            Value::Object(Some(ObjRef::Instance(actor))),
            Value::Object(Some(ObjRef::Instance(self.player))),
        ];
        let name = self.vm.objects[actor as usize].name.clone();
        match self.vm.send_event(actor, "TriggerEvent", args) {
            Ok(_) => println!(
                "[play] host bridge: {name} (XIDCine.BeachFinalFall) has no decoded starter; \
                 firing its own TriggerEvent({event:?})"
            ),
            Err(e) => self.record_failure(&name, &e),
        }
        self.drain_events();
    }

    fn update_sync(&mut self) {
        let mut moved = Vec::new();
        if self.vm.objects.len() > self.last_synced.len() {
            self.last_synced.resize(self.vm.objects.len(), None);
        }
        for (i, o) in self.vm.objects.iter().enumerate() {
            if !o.is_actor || o.deleted {
                continue;
            }
            let Some(cur) = self.vm.location_prop(i as ObjectId) else {
                continue;
            };
            match self.last_synced[i] {
                Some(base) => {
                    if base != cur {
                        moved.push((o.name.clone(), render_delta(base, cur)));
                        self.last_synced[i] = Some(cur);
                    }
                }
                None => self.last_synced[i] = Some(cur),
            }
        }
        self.moved = moved;
    }

    fn suspend(&mut self, id: ObjectId, e: &VmError) {
        self.vm.set_active(id, false);
        let name = self
            .vm
            .objects
            .get(id as usize)
            .map(|o| o.name.clone())
            .unwrap_or_else(|| "<unknown>".to_owned());
        self.record_failure(&name, e);
    }

    fn record_failure(&mut self, name: &str, e: &VmError) {
        if !self.suspended.iter().any(|s| s == name) {
            self.suspended.push(name.to_owned());
        }
        let text = format!("{e}");
        if !self.failures.iter().any(|(n, _)| n == name) {
            self.failures.push((name.to_owned(), text.clone()));
            while self.failures.len() > 32 {
                self.failures.remove(0);
            }
        }
        if self.first_error.is_none() {
            self.first_error = Some(text);
        }
    }
}

/// Unreal-space delta between two VM `Location`s. The host converts it with the single
/// coordinate policy and adds it to the render entity's translation, so a VM-moved actor's
/// render transform follows the VM one way (an unmoved actor gives a zero delta).
pub fn render_delta(previous: [f32; 3], current: [f32; 3]) -> [f32; 3] {
    [
        current[0] - previous[0],
        current[1] - previous[1],
        current[2] - previous[2],
    ]
}

/// item18: reads every Bink clip next to the installation and registers its real duration in the
/// VM (`Vm::set_video_duration`). Returns `(clips found, clips timed)`.
///
/// The Bink 1 header is fixed and cheap to read (header layout: `BIK` + revision at 0; frame count
/// at 8; width at 20; height at 24; fps numerator at 28; fps denominator at 32 — see
/// `crates/xiii-video/src/container.rs` in the item17a worktree and the public Bink container
/// documentation). Duration = frame_count / (fps_num / fps_den). Only the first 36 bytes are read,
/// so registering all clips costs a handful of small reads, not the multi-megabyte files.
fn register_video_durations(vm: &mut Vm<'_>, game_dir: &Path) -> (usize, usize) {
    let mut clips = 0usize;
    let mut timed = 0usize;
    let Ok(entries) = std::fs::read_dir(game_dir) else {
        return (clips, timed);
    };
    for entry in entries.flatten() {
        let dir = entry.path();
        if !dir.is_dir()
            || !dir
                .file_name()
                .is_some_and(|n| n.eq_ignore_ascii_case("video"))
        {
            continue;
        }
        let Ok(files) = std::fs::read_dir(&dir) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if !path
                .extension()
                .is_some_and(|x| x.eq_ignore_ascii_case("bik"))
            {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            clips += 1;
            let mut header = [0u8; 36];
            if std::fs::File::open(&path)
                .and_then(|mut f| f.read_exact(&mut header))
                .is_err()
            {
                continue;
            }
            if let Some(secs) = bink_duration_secs(&header) {
                vm.set_video_duration(stem, secs);
                timed += 1;
            }
        }
    }
    (clips, timed)
}

/// Duration of a Bink 1 clip from its fixed header, or `None` for a bad signature / zero fields.
fn bink_duration_secs(header: &[u8; 36]) -> Option<f32> {
    if &header[0..3] != b"BIK" {
        return None;
    }
    let frames = u32::from_le_bytes(header[8..12].try_into().ok()?);
    let fps_num = u32::from_le_bytes(header[28..32].try_into().ok()?);
    let fps_den = u32::from_le_bytes(header[32..36].try_into().ok()?);
    if frames == 0 || fps_num == 0 || fps_den == 0 {
        return None;
    }
    let secs = frames as f32 * fps_den as f32 / fps_num as f32;
    secs.is_finite().then_some(secs)
}

/// First live `PlayerStart` in the map: `(Location, Rotation)` in Unreal units.
fn find_player_start(vm: &Vm) -> Option<([f32; 3], [i32; 3])> {
    for (i, o) in vm.objects.iter().enumerate() {
        if !o.is_actor || o.deleted || !vm.is_a(i as ObjectId, "playerstart") {
            continue;
        }
        let loc = vm.vector_prop(i as ObjectId, "Location")?;
        let rot = match vm.get_property(i as ObjectId, "Rotation") {
            Some(Value::Rotator(r)) => *r,
            _ => [0; 3],
        };
        return Some((loc, rot));
    }
    None
}

/// Decodes the hit boxes of every live pawn's `SkeletalMesh` (once per mesh path) and builds the
/// host hit-zone provider (item14b). Returns `(actor, shared mesh)` pairs, the number of boxes
/// dropped because their bone index is outside the skeleton, decode failures and the provider.
#[allow(clippy::type_complexity)]
fn build_hit_boxes(
    vm: &Vm<'static>,
    game_dir: &Path,
) -> (
    Vec<(ObjectId, Rc<xiii_world::hitbox::HitBoxMesh>)>,
    usize,
    Vec<String>,
    xiii_world::hitbox::PosedHitZones,
) {
    let mut meshes: Vec<(ObjectId, Rc<xiii_world::hitbox::HitBoxMesh>)> = Vec::new();
    let mut dropped = 0usize;
    let mut errors = Vec::new();
    let mut cache = match xiii_world::PackageCache::open(game_dir) {
        Ok(c) => c,
        Err(e) => {
            errors.push(format!("package cache: {e}"));
            return (
                meshes,
                dropped,
                errors,
                xiii_world::hitbox::PosedHitZones::new(),
            );
        }
    };
    let mut decoded: std::collections::HashMap<
        String,
        Result<Rc<xiii_world::hitbox::HitBoxMesh>, String>,
    > = std::collections::HashMap::new();
    for i in 0..vm.objects.len() {
        let id = i as ObjectId;
        let o = &vm.objects[i];
        if !o.is_actor || o.deleted || o.name.starts_with("Default__") {
            continue;
        }
        let Some((path, class)) = vm.mesh_object(id) else {
            continue;
        };
        if !class
            .rsplit('.')
            .next()
            .is_some_and(|s| s.eq_ignore_ascii_case("SkeletalMesh"))
        {
            continue;
        }
        let entry = decoded
            .entry(path.clone())
            .or_insert_with(|| xiii_world::hitbox::load_mesh(&mut cache, &path).map(Rc::new))
            .clone();
        match entry {
            Ok(m) => {
                dropped += m.dropped_boxes;
                meshes.push((id, m));
            }
            Err(e) => errors.push(format!("{} {}: {e}", o.name, path)),
        }
    }
    let provider = xiii_world::hitbox::PosedHitZones::new();
    (meshes, dropped, errors, provider)
}

/// Live instance held by the object property `name` of `id`.
fn instance_prop(vm: &Vm, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(ObjRef::Instance(p))))
            if vm.objects.get(*p as usize).is_some_and(|o| !o.deleted) =>
        {
            Some(*p)
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Options;
    use crate::play::{resolve_params, run_script, script, viewer};
    use xiii_decode::common::UNREAL_UNITS_PER_METER;

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

    /// item18: the Bink header duration used by `VideoPlayer.GetStatus` (frames / fps).
    #[test]
    fn bink_header_duration_is_frames_over_fps() {
        let mut h = [0u8; 36];
        h[0..4].copy_from_slice(b"BIKi");
        h[8..12].copy_from_slice(&1358u32.to_le_bytes()); // Cine01 frames
        h[28..32].copy_from_slice(&25u32.to_le_bytes());
        h[32..36].copy_from_slice(&1u32.to_le_bytes());
        let secs = bink_duration_secs(&h).expect("duration");
        assert!((secs - 54.32).abs() < 0.01, "1358/25 = {secs}");
        // A bad signature or a zero field is rejected (never a guessed duration).
        let mut bad = h;
        bad[0] = b'X';
        assert!(bink_duration_secs(&bad).is_none());
        let mut zero = h;
        zero[8..12].copy_from_slice(&0u32.to_le_bytes());
        assert!(bink_duration_secs(&zero).is_none());
    }

    /// Opt-in corpus test: the Plage00 single-player login runs through script
    /// (`GameInfo.Login` -> `PostLogin` -> `RestartPlayer`) and creates the real
    /// `XIIIPlayerController`/`XIIIPlayerPawn` at the map's PlayerStart; the explicit harness
    /// bootstrap is NOT used. `LevelInfo.GetLocalURL` is on this path
    /// (`XIIIPlayerController.SetInitialState`).
    #[test]
    fn opt_in_plage00_login_creates_script_controller_and_pawn() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let session = Session::open(&game_dir, "Plage00").expect("open Plage00 session");
        println!("[login test] {}", session.bootstrap_note);
        println!("[login test] blocked: {:?}", session.blocked);
        assert_eq!(
            session.login_script,
            1,
            "the script login path must create the pawn (blocked: {})",
            session.blocked.join("; ")
        );
        assert_eq!(
            session.login_bootstrap, 0,
            "the explicit harness bootstrap must not run"
        );
        let pc = session.controller.expect("a script player controller");
        assert!(
            session.vm.is_a(pc, "XIIIPlayerController"),
            "controller is {}",
            session.vm.set().path(session.vm.objects[pc as usize].class)
        );
        assert!(
            session.vm.is_a(session.player, "XIIIPlayerPawn"),
            "pawn is {}",
            session
                .vm
                .set()
                .path(session.vm.objects[session.player as usize].class)
        );
        let (start, _) = find_player_start(&session.vm).expect("Plage00 has a PlayerStart");
        let pawn = session
            .player_location()
            .expect("player pawn has a Location");
        assert_eq!(
            pawn, start,
            "the pawn must spawn at the PlayerStart, not the bootstrap pose"
        );
        println!(
            "[login test] path=script player={} controller={} at {:?} UU",
            session.player_name,
            session.vm.set().path(session.vm.objects[pc as usize].class),
            pawn
        );
    }

    /// Opt-in corpus test (Part B): on Plage01 the pawn **walks** onto the hut key (autopilot
    /// `goto`, with jumps over the counter), the game's own `Pickup.Touch` -> `SpawnCopy`/
    /// `GiveTo` -> `Pawn.AddInventory` chain puts `Plage01CahuteKey` in the pawn's inventory (no
    /// host key grant), and the `E`/`use` action at `Porte6` unlocks and opens it with that
    /// carried key.
    ///
    /// The pickup is reached from the key's open (-Y) side: the +Y/+X/-X approaches are blocked
    /// by map collision (a wall at Y=-250 with a +Y normal, measured), and the full PlayerStart
    /// path stops at the same wall even with jumps; the reach graph's last point is ~170 UU short
    /// of the key. The final ~100 UU are walked and the counter edge is jumped. No teleport onto
    /// the key.
    #[test]
    fn opt_in_plage01_key_pickup_without_host_grant_opens_porte6() {
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
        .expect("run Plage01 key+door walk");
        let s = &outcome.session;
        let inv = s.inventory_items();
        println!("[key test] inventory: {inv:?}");
        assert!(
            inv.iter()
                .any(|(_, c)| c.eq_ignore_ascii_case("xidmaps.Plage01CahuteKey")),
            "the hut key must be in the pawn's inventory through the real pickup chain: {inv:?}"
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
            "Porte6 rotation did not change (the door did not swing)"
        );
        let (_, _, pos, _) = outcome.trace.last().expect("trace sample");
        let pos = *pos;
        let base = door.base_pos;
        let yaw = door.base_rot[1] as f32 * std::f32::consts::TAU / 65536.0;
        let fwd = [yaw.cos(), yaw.sin()];
        let dist = (pos[0] - base[0]) * fwd[0] + (pos[1] - base[1]) * fwd[1];
        println!(
            "[key test] Porte6 key={} rot={:?} (base {:?}), player {:?} UU, {dist:.1} UU outside",
            door.key_num, door.rotation, door.base_rot, pos
        );
        assert!(
            dist >= 2.0 * UNREAL_UNITS_PER_METER,
            "player only {dist:.1} UU outside the door plane (need >= {:.0})",
            2.0 * UNREAL_UNITS_PER_METER
        );
    }

    /// Opt-in corpus (item14b requirement 2, re-measured by item14c): the map's real
    /// `XIII.BerettaPick` (`BerettaPick0`) sits at `(-737.654,-511.886,1262.99)` UU, ~3.3 m from
    /// `PlayerStart0`, but the player spawns **inside the Plage01 hut**. The pickup's collision
    /// is correct (`CollisionRadius 30`, `CollisionHeight 10`, `bCollideActors=true`,
    /// `bBlockActors/bBlockPlayers=false`), so the cylinder touch threshold is `34 + 30 = 64` UU;
    /// the swept walk from the item14b teleport point stops on the static-mesh hut wall
    /// `StaticMeshActor287 -> StaticPlage2.GR_interieur02` (normal +X) 80.4 UU short, and a
    /// natural walk from the PlayerStart stops after ~7 UU. The pickup is therefore **not
    /// reachable early by a direct walk**; the item14b demonstration keeps the labelled `weapon
    /// XIII.Beretta` grant for the synthetic zone test, and the real pickup chain itself is
    /// exercised by the `opt_in_plage01_key_pickup_without_host_grant_opens_porte6` test on the
    /// same map. This test records the measured block (it asserts the negative, so a future
    /// reachability fix fails it visibly). See `opt_in_plage01_beretta_blocking_contacts` for the
    /// contact dump and the touch-shape probe.
    #[test]
    fn opt_in_plage01_beretta_pickup_is_not_reachable_early() {
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
            "t=0.00 teleport -637.654 -511.886 1265.0\n\
             t=0.10 goto -737.654 -511.886\n\
             t=4.00 forward 0\n",
        )
        .unwrap();
        let outcome = run_script(&game_dir, "Plage01", &script, &resolved.params, &scene, 6.0)
            .expect("run Plage01 Beretta walk");
        let s = &outcome.session;
        let (_, _, pos, _) = outcome.trace.last().expect("trace sample");
        let pick = s
            .vm()
            .find_object("BerettaPick0")
            .and_then(|p| s.vm().vector_prop(p, "Location"))
            .expect("Plage01 has BerettaPick0");
        let d = ((pos[0] - pick[0]).powi(2) + (pos[1] - pick[1]).powi(2)).sqrt();
        println!(
            "[beretta test] player end ({:.1},{:.1}), BerettaPick0 ({:.1},{:.1}), horizontal gap {d:.1} UU",
            pos[0], pos[1], pick[0], pick[1]
        );
        assert!(
            d > 64.0,
            "the Beretta is now reachable early ({d:.1} UU): update the item14b demonstration to walk to it"
        );
        assert!(
            !s.inventory_items()
                .iter()
                .any(|(_, c)| c.to_ascii_lowercase().contains("beretta")),
            "the Beretta was picked up without a reachable walk"
        );
    }

    /// Opt-in diagnostic + regression (item14c requirement 3): what blocks the player near
    /// `BerettaPick0`. The pickup's own collision is correct (`CollisionRadius 30`,
    /// `CollisionHeight 10`, `bCollideActors=true`, `bBlockActors`/`bBlockPlayers=false`), so the
    /// cylinder-vs-cylinder touch threshold is `34 + 30 = 64` UU. Placing the player exactly on
    /// the pickup (a shape probe, not a walk) delivers the game's own `Pickup.Touch` and collects
    /// it, so the player-vs-pickup touch uses the right shapes. The block is world geometry: the
    /// swept walk from the item14b teleport point stops on `StaticMeshActor287 ->
    /// StaticPlage2.GR_interieur02` (a static-mesh hut wall, contact normal +X) 80.4 UU from the
    /// pickup, and a natural walk from the PlayerStart stops after ~7 UU. The pickup is therefore
    /// not reachable early by a direct walk; this is the measured truth the negative regression
    /// below records.
    #[test]
    fn opt_in_plage01_beretta_blocking_contacts() {
        use xiii_collision::{CollisionWorld, MoveParams, move_slide};
        use xiii_decode::common::to_bevy_position;
        use xiii_world::physics::{bevy_to_unreal_direction, bevy_to_unreal_position};

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
        let params = &resolved.params;
        let session = Session::open(&game_dir, "Plage01").expect("open Plage01");
        let pick = session
            .vm()
            .find_object("BerettaPick0")
            .expect("BerettaPick0");
        let ploc = session
            .vm()
            .vector_prop(pick, "Location")
            .expect("pickup location");
        let radius = match session.vm().get_property(pick, "CollisionRadius") {
            Some(Value::Float(f)) => *f,
            other => panic!("BerettaPick0.CollisionRadius = {other:?}"),
        };
        let height = match session.vm().get_property(pick, "CollisionHeight") {
            Some(Value::Float(f)) => *f,
            other => panic!("BerettaPick0.CollisionHeight = {other:?}"),
        };
        println!(
            "[beretta test] BerettaPick0 loc ({:.2},{:.2},{:.2}) UU, cylinder r{radius} h{height}, \
             bCollideActors={:?} bBlockActors={:?} bBlockPlayers={:?}",
            ploc[0],
            ploc[1],
            ploc[2],
            session.vm().get_property(pick, "bCollideActors"),
            session.vm().get_property(pick, "bBlockActors"),
            session.vm().get_property(pick, "bBlockPlayers"),
        );
        assert_eq!((radius, height), (30.0, 10.0), "pickup cylinder changed");
        assert!(
            matches!(
                session.vm().get_property(pick, "bCollideActors"),
                Some(Value::Bool(true))
            ),
            "the pickup must be touchable (bCollideActors)"
        );
        assert!(
            matches!(
                session.vm().get_property(pick, "bBlockActors"),
                Some(Value::Bool(false))
            ),
            "the pickup must not block actors"
        );
        let threshold = params.radius_uu + radius;
        // Swept walk from the item14b teleport point toward the pickup: dump every contact.
        let world = CollisionWorld::new(scene.box_collision());
        let half = params.half_extents_bevy();
        let start = to_bevy_position([-637.654, -511.886, 1265.0]);
        let end = to_bevy_position([ploc[0], ploc[1], ploc[2]]);
        let mv = MoveParams {
            max_step_height: 0.0,
            ..MoveParams::default()
        };
        let res = move_slide(
            &world,
            start,
            [end[0] - start[0], 0.0, end[2] - start[2]],
            half,
            &mv,
        );
        let final_uu = bevy_to_unreal_position(res.position);
        let gap = ((final_uu[0] - ploc[0]).powi(2) + (final_uu[1] - ploc[1]).powi(2)).sqrt();
        println!(
            "[beretta test] teleport-side walk: end ({:.1},{:.1},{:.1}) UU, gap {gap:.1} UU \
             (touch threshold {threshold:.0}), {} contact(s)",
            final_uu[0],
            final_uu[1],
            final_uu[2],
            res.contacts.len()
        );
        let mut contact_sources = Vec::new();
        for c in &res.contacts {
            let src = scene
                .collision_sources
                .get(c.source as usize)
                .map(String::as_str)
                .unwrap_or("?");
            let n = bevy_to_unreal_direction(c.normal);
            let p = bevy_to_unreal_position(c.position);
            println!(
                "[beretta test]   contact src {src} tri {} normal ({:.2},{:.2},{:.2}) at \
                 ({:.1},{:.1},{:.1}) UU",
                c.triangle, n[0], n[1], n[2], p[0], p[1], p[2]
            );
            contact_sources.push(src.to_owned());
        }
        assert!(
            contact_sources
                .iter()
                .any(|s| s.contains("StaticPlage2.GR_interieur02")),
            "the measured blocker changed: {contact_sources:?}"
        );
        assert!(
            gap > threshold,
            "the pickup is now within touch range ({gap:.1} <= {threshold:.0}): update the walk"
        );
        // Natural walk from the actual PlayerStart (no teleport) confirms the spawn-side block.
        let natural =
            script::Script::parse("t=0.00 goto -737.654 -511.886\nt=8.00 forward 0\n").unwrap();
        let outcome = run_script(&game_dir, "Plage01", &natural, params, &scene, 10.0)
            .expect("run natural walk");
        let (_, _, npos, _) = outcome.trace.last().expect("trace sample");
        let ngap = ((npos[0] - ploc[0]).powi(2) + (npos[1] - ploc[1]).powi(2)).sqrt();
        println!(
            "[beretta test] natural walk from PlayerStart: end ({:.1},{:.1},{:.1}) UU, gap {ngap:.1} UU",
            npos[0], npos[1], npos[2]
        );
        assert!(
            ngap > threshold,
            "the PlayerStart walk now reaches the pickup ({ngap:.1}): update the regression"
        );
        // Shape probe: the cylinder test must collect the pickup when the player is on it. This
        // is a touch-shape check, not a walk.
        let mut probe = Session::open(&game_dir, "Plage01").expect("open probe");
        let player = probe.player;
        let before = probe.inventory_items();
        probe
            .vm_mut()
            .set_property(player, "Location", 0, Value::Vector(ploc));
        let _ = probe.vm_mut().refresh_touching_of(player);
        let after = probe.inventory_items();
        println!(
            "[beretta test] overlap probe: inventory before {} item(s), after {} item(s): {:?}",
            before.len(),
            after.len(),
            after
        );
        assert!(
            after
                .iter()
                .any(|(_, c)| c.to_ascii_lowercase().contains("beretta")),
            "the cylinder-vs-cylinder touch did not collect the overlapping pickup: {after:?}"
        );
    }

    /// Opt-in corpus regression (item3j Part A): the script login leaves **exactly one**
    /// `XIIIPlayerPawn`. Before the fix, `GameInfo.PostLogin` -> `StartMatch` restarted every
    /// placed `Engine.Camera` `PlayerController` (no pawn, not a spectator) and spawned 11 extra
    /// pawns at the PlayerStart on each map. Checked at open and after 120 fixed ticks.
    #[test]
    fn opt_in_single_player_login_spawns_one_player_pawn() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        for map in ["Plage00", "Plage01"] {
            let mut session = Session::open(&game_dir, map).expect("open session");
            let at_open = session.player_pawn_actors();
            assert_eq!(
                at_open.len(),
                1,
                "{map}: expected one XIIIPlayerPawn at login, got {at_open:?}"
            );
            for _ in 0..120 {
                let loc = session.player_location().unwrap_or([0.0; 3]);
                session.step(1.0 / 60.0, loc, 0.0, [0.0; 3], &PlayerVMModes::default());
            }
            let after = session.player_pawn_actors();
            assert_eq!(
                after.len(),
                1,
                "{map}: expected one XIIIPlayerPawn after 120 ticks, got {after:?}"
            );
            println!("[dupe test] {map}: one player pawn {:?}", after[0].1);
        }
    }

    /// Opt-in: falling damage comes from the pawn's own VM code. A published impact velocity
    /// runs `XIIIPawn.Landed` -> `TakeFallingDamage` -> `TakeDamage`; the resulting `Health`
    /// read back from the VM must be lower. A failure is never silent: the pawn's `Landed`
    /// error (if any) is recorded and printed.
    #[test]
    fn opt_in_banque01_falling_damage_through_vm() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Banque01").expect("open Banque01");
        let h0 = session.player_health();
        let loc = session.player_location().expect("player location");
        // Land from a terminal fall: the game decides the damage.
        let modes = PlayerVMModes {
            landed_velocity_z: Some(-1500.0),
            floor_normal: [0.0, 0.0, 1.0],
            ..Default::default()
        };
        session.step(1.0 / 60.0, loc, 0.0, [0.0, 0.0, 0.0], &modes);
        let h1 = session.player_health();
        println!(
            "[fall test] Banque01 Health {h0:?} -> {h1:?}; Landed error: {}",
            session.first_error().unwrap_or("none")
        );
        let (Some(h0), Some(h1)) = (h0, h1) else {
            panic!("the pawn has no health property");
        };
        assert!(
            h1 < h0,
            "the VM's fall damage did not reduce Health ({h0} -> {h1}); first error: {:?}",
            session.first_error()
        );
    }
    /// Opt-in corpus (item14b requirement 4): a soldier that is in its own active state
    /// (`Base01` `BaseSoldier17`, order `Tenir`) detects the player through the host sight bridge
    /// (`SeePlayer`), runs its own `Tenir -> Acquisition -> Attaque` states, turns toward the
    /// player, fires its M16 and the player's `Health` drops. Base01 is used because every
    /// campaign soldier on an active order carries its real weapon there; Plage00/01 soldiers are
    /// ordered to the scripted `faction` (stasis) state and never fight.
    #[test]
    fn opt_in_base01_soldier_fights_back() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Base01").expect("open Base01");
        let soldier = session
            .vm()
            .find_object("BaseSoldier17")
            .expect("BaseSoldier17");
        let sloc = session
            .vm()
            .vector_prop(soldier, "Location")
            .expect("soldier location");
        // Warm the soldier to `Tenir` (its `Init` runs, it receives a weapon) for 6 s.
        let ploc0 = session.player_location().unwrap_or([0.0; 3]);
        for _ in 0..360 {
            session.step(1.0 / 60.0, ploc0, 0.0, [0.0; 3], &PlayerVMModes::default());
        }
        let ctrl = instance_prop(session.vm(), soldier, "Controller").expect("controller");
        let warm_state = session.vm().state_name(ctrl);
        assert_eq!(
            warm_state.as_deref(),
            Some("Tenir"),
            "BaseSoldier17 did not reach Tenir (state {warm_state:?})"
        );
        // Put the player in front of the soldier, in the open, facing it.
        let srot = session.vm().rotation_prop(soldier).unwrap_or([0; 3]);
        let k = std::f32::consts::TAU / 65536.0;
        let (sp, cp) = ((srot[0] as f32) * k).sin_cos();
        let (sy, cy) = ((srot[1] as f32) * k).sin_cos();
        let fwd = [cp * cy, cp * sy, sp];
        let ploc = [
            sloc[0] + fwd[0] * 70.0,
            sloc[1] + fwd[1] * 70.0,
            sloc[2] + 20.0,
        ];
        let yaw_to_soldier = (-fwd[1]).atan2(-fwd[0]);
        let hp0 = session.player_health().expect("player health");
        let mut first_attack = None;
        let mut hp_low = hp0;
        for t in 0..420u32 {
            session.step(
                1.0 / 60.0,
                ploc,
                yaw_to_soldier,
                [0.0; 3],
                &PlayerVMModes::default(),
            );
            let st = session.vm().state_name(ctrl);
            if first_attack.is_none() && st.as_deref() == Some("Attaque") {
                first_attack = Some(t as f32 / 60.0);
            }
            if let Some(h) = session.player_health() {
                hp_low = hp_low.min(h);
            }
        }
        let hp1 = session.player_health().expect("player health");
        println!("[ai test] perception: {:?}", session.perception_log);
        let weapon = instance_prop(session.vm(), soldier, "Weapon").expect("weapon");
        let combat_events: Vec<_> = session
            .vm()
            .trace
            .iter()
            .filter_map(|event| match &event.kind {
                xiii_script::TraceKind::Event {
                    target, function, ..
                } if target == &session.vm().objects[ctrl as usize].name
                    && (function.ends_with(".Timer") || function.ends_with(".AnimEnd")) =>
                {
                    Some((event.time, function.clone()))
                }
                xiii_script::TraceKind::StateChange {
                    actor, from, to, ..
                } if actor == &session.vm().objects[weapon as usize].name => {
                    Some((event.time, format!("weapon {from:?}->{to:?}")))
                }
                _ => None,
            })
            .collect();
        println!(
            "[ai test] player Health {hp0} -> {hp1} (low {hp_low}), first Attaque at {first_attack:?}s; combat events={combat_events:?}, final bTire={:?} bFire={:?}, weapon state={:?}, ReloadCount={:?}",
            session.vm().get_property(ctrl, "bTire"),
            session.vm().get_property(ctrl, "bFire"),
            session.vm().state_name(weapon),
            session.vm().get_property(weapon, "ReloadCount"),
        );
        // The soldier's own state machine acquired the enemy.
        assert!(
            session
                .perception_log
                .iter()
                .any(|(_, c, e)| c == "IAController1" && e == "SeePlayer"),
            "the sight bridge did not dispatch SeePlayer to IAController1"
        );
        assert!(
            first_attack.is_some(),
            "the soldier never entered Attaque (perception {:?})",
            session.perception_log
        );
        // The game's own damage chain reduced the player's Health.
        assert!(
            hp1 < hp0,
            "the soldier's fire did not reduce the player's Health ({hp0} -> {hp1}); first error {:?}",
            session.first_error()
        );
    }

    /// Opt-in Plage01 combat: deliver the authored `TouchTrigger7` (`Event=tueur_conducteur`)
    /// cue to BaseSoldier6's controller, then record whether the game's scripted attack targets
    /// and fires at a stationary player. This also pins the current compiled-script no-fire path.
    #[test]
    fn opt_in_plage01_authored_cue_soldier_combat_or_measured_no_fire_path() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage01").expect("open Plage01");
        let soldier = session.vm().find_object("BaseSoldier6").expect("soldier");
        let cue = session
            .vm()
            .find_object("TouchTrigger7")
            .expect("authored cue");
        let trigger_event = session.vm().get_property(cue, "Event").cloned();
        assert_eq!(trigger_event, Some(Value::Name("tueur_conducteur".into())));
        let warm_player = session.player_location().expect("player location");
        for _ in 0..360 {
            session.step(
                1.0 / 60.0,
                warm_player,
                0.0,
                [0.0; 3],
                &PlayerVMModes::default(),
            );
        }
        // Deliver the map's own Touch event with the player's actual spawn position. Touch
        // dispatches the authored Event through its configured targets into the AI controller.
        let player = session.player;
        let player_location = session.player_location().expect("player location");
        session
            .vm_mut()
            .send_event(
                cue,
                "Touch",
                vec![Value::Object(Some(ObjRef::Instance(player)))],
            )
            .expect("deliver authored TouchTrigger7.Touch");
        assert!(
            session.vm().trace.iter().any(|event| {
                matches!(
                    &event.kind,
                    xiii_script::TraceKind::Event { target, function, args }
                        if target == "TouchTrigger7"
                            && function.ends_with(".Touch")
                            && args.iter().any(|arg| arg == &session.player_name)
                )
            }),
            "the player's authored TouchTrigger7 cue was not delivered"
        );
        let controller = instance_prop(session.vm(), soldier, "Controller").expect("AI controller");
        let weapon = instance_prop(session.vm(), soldier, "Weapon").expect("soldier weapon");
        assert_eq!(
            session.vm().state_name(controller).as_deref(),
            Some("AttaqueScriptee")
        );
        let cue_enemy = instance_prop(session.vm(), controller, "Enemy");
        let cue_enemy_name = cue_enemy.map(|id| session.vm().objects[id as usize].name.clone());
        let initial_rotation = session.vm().rotation_prop(soldier).unwrap_or([0; 3]);
        let health_before = session.player_health().expect("player health");
        let soldier_location = session
            .vm()
            .vector_prop(soldier, "Location")
            .expect("soldier location");
        let sight_radius = match session.vm().get_property(soldier, "SightRadius") {
            Some(Value::Float(radius)) => *radius,
            other => panic!("BaseSoldier6 SightRadius is not a float: {other:?}"),
        };
        let player_distance = ((player_location[0] - soldier_location[0]).powi(2)
            + (player_location[1] - soldier_location[1]).powi(2)
            + (player_location[2] - soldier_location[2]).powi(2))
        .sqrt();
        let mut scripted_attack_seen = false;
        let mut controller_attack_seen = false;
        let mut held_fire_seen = false;
        let mut fire_request_seen = false;
        for _ in 0..900 {
            session.step(
                1.0 / 60.0,
                player_location,
                0.0,
                [0.0; 3],
                &PlayerVMModes::default(),
            );
            scripted_attack_seen |=
                session.vm().state_name(controller).as_deref() == Some("AttaqueScriptee");
            controller_attack_seen |=
                session.vm().state_name(controller).as_deref() == Some("Attaque");
            held_fire_seen |= matches!(
                session.vm().get_property(controller, "bTire"),
                Some(Value::Bool(true))
            );
            fire_request_seen |= [
                (controller, "bFire"),
                (controller, "bAltFire"),
                (soldier, "bFire"),
                (soldier, "bAltFire"),
                (weapon, "bFire"),
                (weapon, "bAltFire"),
            ]
            .into_iter()
            .any(|(actor, property)| {
                matches!(
                    session.vm().get_property(actor, property),
                    Some(Value::Bool(true))
                )
            }) || held_fire_seen;
        }
        let state = session.vm().state_name(soldier);
        let controller_state = session.vm().state_name(controller);
        let final_rotation = session.vm().rotation_prop(soldier).unwrap_or([0; 3]);
        let health_after = session.player_health().expect("player health");
        let wants_fire = matches!(
            session.vm().get_property(controller, "bTire"),
            Some(Value::Bool(true))
        );
        let final_enemy = instance_prop(session.vm(), controller, "Enemy")
            .map(|id| session.vm().objects[id as usize].name.clone());
        let scripted_states: Vec<_> = session
            .vm()
            .trace
            .iter()
            .filter_map(|event| match &event.kind {
                xiii_script::TraceKind::StateChange {
                    actor, from, to, ..
                } if actor == &session.vm().objects[controller as usize].name => {
                    Some((from.clone(), to.clone()))
                }
                _ => None,
            })
            .collect();
        let can_see: Vec<_> = session
            .vm()
            .trace
            .iter()
            .filter_map(|event| match &event.kind {
                xiii_script::TraceKind::Native {
                    path,
                    this,
                    args,
                    result,
                    ..
                } if path.ends_with("Controller.CanSee")
                    && this == &session.vm().objects[controller as usize].name =>
                {
                    Some((event.time, args.clone(), result.clone()))
                }
                _ => None,
            })
            .collect();
        let left_scripted_attack = scripted_states.iter().any(|(from, to)| {
            from.as_deref() == Some("AttaqueScriptee") && to.as_deref() == Some("temporise")
        });
        println!(
            "[Plage01 combat] cue=TouchTrigger7 event={trigger_event:?}, player={player_location:?}, soldier={soldier_location:?}, distance={player_distance:.1}, SightRadius={sight_radius}, cue Enemy={cue_enemy_name:?}, final Enemy={final_enemy:?}, scripted={scripted_attack_seen}, combat={controller_attack_seen}, held-fire-seen={held_fire_seen}, fire-request-seen={fire_request_seen}, bTire={wants_fire}, soldier state={state:?}, controller state={controller_state:?}, controller states={scripted_states:?}, CanSee={can_see:?}, rotation {initial_rotation:?}->{final_rotation:?}, Health {health_before}->{health_after}; suspended={:?}",
            session.suspended
        );
        assert!(
            scripted_attack_seen,
            "the authored cue did not release the soldier"
        );
        assert!(
            player_distance > sight_radius
                && can_see.iter().any(|(_, _, result)| result == "false"),
            "the actual player position should be outside SightRadius with a measured failed CanSee: distance={player_distance}, SightRadius={sight_radius}, CanSee={can_see:?}"
        );
        // Whether this authored map cue yields combat is determined by the map's own dispatch,
        // AI state chain, and LOS result at the player's actual spawn location.
        assert!(
            cue_enemy == Some(player)
                && final_enemy.is_none()
                && left_scripted_attack
                && controller_state.as_deref() == Some("Patrouille")
                && !controller_attack_seen
                && !held_fire_seen
                && !fire_request_seen
                && !wants_fire
                && health_after == health_before,
            "unexpected combat result: Health {health_before}->{health_after}, bTire={wants_fire}"
        );
    }

    /// Opt-in corpus regression (item3o): the Plage00 level-start checkpoint path runs the
    /// game's own `XIIISaveGameTrigger.GoSaving.DoSave`, which used to suspend on the struct-array
    /// member `bcompleted` (`ObjectivesState.length` grew the element as `int 0`, so the
    /// subsequent `.bCompleted` write failed with "no struct member bcompleted"). The save must
    /// now emit the host `SaveCheckpoint` event. The intro's control-return is measured by the
    /// acceptance `--play` run (the cutscene needs the HUD `PostRender` to leave
    /// `WaitForFirstDisplay`, which this headless harness does not drive).
    #[test]
    fn opt_in_plage00_checkpoint_save_event() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").expect("open Plage00");
        // The save trigger fires when the host delivers the player's Touch after the first steps.
        for _ in 0..120 {
            if session.save_total >= 1 {
                break;
            }
            let loc = session.player_location().unwrap_or([0.0; 3]);
            session.step(1.0 / 60.0, loc, 0.0, [0.0; 3], &PlayerVMModes::default());
        }
        assert!(
            session.save_total >= 1,
            "the level-start checkpoint save did not run (suspended: {:?})",
            session.suspended
        );
        let save = session
            .saves
            .front()
            .expect("a SaveCheckpoint event")
            .1
            .clone();
        assert_eq!(save.teleporter_name, "PlayerStart");
        assert_eq!(save.description, "Brighton Beach 1");
        println!(
            "[save test] checkpoint {:?} teleporter={:?} description={:?}",
            save.actor, save.teleporter_name, save.description
        );
    }

    #[test]
    fn opt_in_plage00_checkpoint_resume_retains_health_for_ten_seconds() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session =
            Session::open_checkpoint(&game_dir, "Plage00").expect("open Plage00 resume");
        // The port presents the resume the way the engine's native checkpoint-load path does:
        // after the login chain (`XIIIGameInfo.RestartPlayer` 0x037A has already copied
        // `StartSpot.Event`), GameInfo.StartSpotEvent is "LOAD", so the level's own
        // `Plage00.FirstFrame` guard (0x0013) skips re-applying the wounded intro Health.
        let game_info = session.game_info.expect("GameInfo");
        assert_eq!(
            session.vm().get_property(game_info, "StartSpotEvent"),
            Some(&Value::Name("LOAD".into())),
            "open_checkpoint must leave StartSpotEvent=LOAD after the login chain"
        );
        let saved = crate::save::SaveFile {
            map: "Plage00".into(),
            teleporter: "PlayerStart".into(),
            description: "corpus resume regression".into(),
            health: 150.0,
            speed_factor_limit: 1.0,
            checkpoint_number: 7,
            location: session.player_location().expect("player location"),
            rotation: session.player_rotation().unwrap_or([0; 3]),
            objectives: session
                .objective_states()
                .into_iter()
                .map(|o| crate::save::Objective {
                    completed: o.completed,
                    primary: o.primary,
                    anti_goal: o.anti_goal,
                })
                .collect(),
            inventory: Vec::new(),
        };
        // One restore, before the first ticked step — the same point the `--play` host applies
        // it (`setup_inner`, after `Session::open`). FirstFrame runs later, on the MapInfo's
        // first Timer tick, and must not overwrite the restored fields.
        let location = saved.location;
        session
            .restore_checkpoint(&saved)
            .expect("apply checkpoint once before the first step");
        assert_eq!(
            session.vm().get_property(game_info, "CheckpointNumber"),
            Some(&Value::Int(7)),
            "the restore must set the saved CheckpointNumber before play resumes"
        );
        let mut health_min = f32::MAX;
        for _ in 0..660 {
            session.step(
                1.0 / 60.0,
                location,
                0.0,
                [0.0; 3],
                &PlayerVMModes::default(),
            );
            if let Some(h) = session.player_health() {
                health_min = health_min.min(h);
            }
        }
        assert!(
            session.vm_time() >= 10.0,
            "did not simulate ten seconds (t={})",
            session.vm_time()
        );
        assert_eq!(
            session.player_health(),
            Some(150.0),
            "FirstFrame re-applied the wounded intro health over the restored value"
        );
        assert!(
            (health_min - 150.0).abs() < f32::EPSILON,
            "health dropped below the restored 150 during ten seconds (min {health_min})"
        );
        assert_eq!(
            session
                .objective_states()
                .iter()
                .map(|o| (o.completed, o.primary, o.anti_goal))
                .collect::<Vec<_>>(),
            saved
                .objectives
                .iter()
                .map(|o| (o.completed, o.primary, o.anti_goal))
                .collect::<Vec<_>>()
        );
        // `RestartPlayer`'s own `TriggerEvent(StartSpot.Event)` re-arms the level's
        // Tag='Debut' `XIIISaveGameTrigger`, whose `GoSaving.DoSave` (0x0191) increments the
        // checkpoint again during resumed play — the game's own re-save, measured 7 -> 8.
        let checkpoint_now = session.vm().get_property(game_info, "CheckpointNumber");
        assert!(
            matches!(checkpoint_now, Some(Value::Int(n)) if *n >= 7),
            "CheckpointNumber regressed below the restored 7 ({checkpoint_now:?})"
        );
        println!(
            "[save corpus] Plage00 Health={} checkpoint={checkpoint_now:?} after {:.3}s of resumed play",
            session.player_health().unwrap_or(-1.0),
            session.vm_time()
        );
    }

    /// Opt-in corpus regression (coordinator requirement): a normal (non-checkpoint) Plage00
    /// start must still run `Plage00.FirstFrame`'s wounded-intro write
    /// (`Health = default.Health * 0.5` at bytecode 0x003E), because `StartSpotEvent` is the
    /// PlayerStart's own `Event` (`'Debut'`), not "LOAD".
    #[test]
    fn opt_in_plage00_normal_start_applies_the_wounded_intro_health() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").expect("open Plage00");
        assert_eq!(
            session
                .vm()
                .get_property(session.game_info.expect("GameInfo"), "StartSpotEvent"),
            Some(&Value::Name("Debut".into())),
            "a normal start must leave RestartPlayer's StartSpot.Event value in place"
        );
        let location = session.player_location().expect("player location");
        for _ in 0..30 {
            session.step(
                1.0 / 60.0,
                location,
                0.0,
                [0.0; 3],
                &PlayerVMModes::default(),
            );
        }
        assert_eq!(
            session.player_health(),
            Some(75.0),
            "FirstFrame's wounded-intro Health write (default 150 * 0.5) did not run"
        );
        println!(
            "[save corpus] normal Plage00 start: Health={} after {:.3}s",
            session.player_health().unwrap_or(-1.0),
            session.vm_time()
        );
    }

    /// Opt-in corpus (item14c requirement 4): the diagnostic grant uses the game's own
    /// `Weapon.GiveTo(Pawn)` path; `ClientWeaponSet` -> `Pawn.ChangedWeapon` ->
    /// `Weapon.AttachToPawn` sets `Pawn.Weapon` and spawns the third-person attachment
    /// (`XIII.BerettaAttach`). Firing then runs the game's own `XIIIWeapon.Fire` -> `LoneFire` ->
    /// `LocalFire` -> `PlayFiring` -> `IncrementFlashCount` ->
    /// `WeaponAttachment(ThirdPersonActor).ThirdPersonEffects` -> `MuzzleAttach` (spawns
    /// `MFSmallAttach`, whose `PostBeginPlay` spawns `MuzzleLight`) ->
    /// `MuzzleFlashAttachment.Visible.Tick` -> `MuzzleLight.Flash`. No host bridge exists any
    /// more: the `MuzzleLight` actor and its `LT_Steady` flash are the game's own code.
    #[test]
    fn opt_in_plage01_give_to_attaches_and_fires_the_muzzle_light() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage01").expect("open Plage01");
        let msg = session.grant_weapon("XIII.Beretta").expect("grant Beretta");
        println!("[muzzle test] {msg}");
        let weapon = session
            .player_weapon()
            .expect("the game's GiveTo path must set Pawn.Weapon");
        assert!(
            session.vm().is_a(weapon, "weapon"),
            "Pawn.Weapon is not a weapon"
        );
        let attach = instance_prop(session.vm(), weapon, "ThirdPersonActor")
            .expect("GiveTo/ChangedWeapon must set ThirdPersonActor");
        assert!(
            session.vm().is_a(attach, "weaponattachment"),
            "ThirdPersonActor is not a WeaponAttachment: {}",
            session
                .vm()
                .set()
                .path(session.vm().objects[attach as usize].class)
        );
        let count_muzzle = |s: &Session| -> usize {
            (0..s.vm().objects.len())
                .filter(|&i| {
                    !s.vm().objects[i].deleted && s.vm().is_a(i as ObjectId, "MuzzleLight")
                })
                .count()
        };
        assert_eq!(
            count_muzzle(&session),
            0,
            "a MuzzleLight already exists before any shot"
        );
        // Fire through the same entry point the host uses (`class_function(weapon, "Fire")`).
        let outcome = session.fire(0.0, 0.0);
        assert_eq!(outcome, FireOutcome::Fired, "the weapon did not fire");
        // `ThirdPersonEffects` runs inside the fire call, so `MuzzleAttach`/`MFSmallAttach` and
        // its `MFLight` must already exist.
        let mf = instance_prop(session.vm(), attach, "MuzzleFlash")
            .expect("ThirdPersonEffects must spawn MuzzleFlash");
        assert!(
            session.vm().is_a(mf, "muzzleflashattachment"),
            "MuzzleFlash is not a MuzzleFlashAttachment"
        );
        let light = instance_prop(session.vm(), mf, "MFLight")
            .expect("MFSmallAttach.PostBeginPlay must spawn MFLight");
        assert!(
            session.vm().is_a(light, "muzzlelight"),
            "MFLight is not a MuzzleLight"
        );
        // The first `Visible` state runs with `DrawType == 8` (set by
        // `InventoryAttachment.PostNetBeginPlay`), so its `Visible.Tick` guard cannot fire; when
        // it ends, `Visible.EndState` sets `DrawType = 0`. The next fire's `MuzzleFlash.Flash()`
        // re-enters `Visible` with `DrawType == 0`, and its `Visible.Tick` calls
        // `MFLight.Flash` at `TickCount > 3` (this is the retail repeated-fire path). Fire a few
        // shots, ticking each, and sample `LightType` to catch the `LT_Steady` (1) flash before
        // `MuzzleLight.Tick` turns it back off.
        let mut muzzle_ids: Vec<ObjectId> = Vec::new();
        let mut flash_seen = false;
        for shot in 0..3 {
            let outcome = session.fire(0.0, 0.0);
            assert_eq!(outcome, FireOutcome::Fired, "shot {shot} did not fire");
            for _ in 0..14 {
                let loc = session.player_location().unwrap_or([0.0; 3]);
                session.step(1.0 / 60.0, loc, 0.0, [0.0; 3], &PlayerVMModes::default());
                for i in 0..session.vm().objects.len() {
                    let id = i as ObjectId;
                    if !session.vm().objects[i].deleted && session.vm().is_a(id, "MuzzleLight") {
                        if !muzzle_ids.contains(&id) {
                            muzzle_ids.push(id);
                        }
                        flash_seen |=
                            session.vm().get_property(id, "LightType") == Some(&Value::Byte(1));
                    }
                }
            }
        }
        println!(
            "[muzzle test] MuzzleLight actors {:?}; game Flash reached LT_Steady: {flash_seen}",
            muzzle_ids
        );
        assert!(
            !muzzle_ids.is_empty(),
            "firing did not spawn the game's MuzzleLight"
        );
        assert!(
            flash_seen,
            "the game's MuzzleFlashAttachment.Visible.Tick never called MuzzleLight.Flash"
        );
    }

    /// Opt-in checkpoint persistence/restore through the same map and named teleporter used by
    /// Plage00's `DoSave`; this deliberately uses a temporary user directory, not game data.
    #[test]
    fn opt_in_plage00_checkpoint_file_restores_decoded_state() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut source = Session::open(&game_dir, "Plage00").expect("open source Plage00");
        for _ in 0..120 {
            if source.save_total > 0 {
                break;
            }
            let loc = source.player_location().unwrap_or([0.; 3]);
            source.step(1.0 / 60.0, loc, 0., [0.; 3], &PlayerVMModes::default());
        }
        let event = source.saves.back().expect("checkpoint event").1.clone();
        let saved = source.checkpoint_snapshot(
            "Plage00",
            &event,
            source.player_location().expect("live player position"),
            source.player_rotation().expect("live player rotation"),
        );
        let dir = std::env::temp_dir().join(format!("xiii-corpus-save-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        crate::save::write(&dir, 0, &saved).expect("write checkpoint file");
        let disk = crate::save::read(&dir, 0).expect("read checkpoint file");
        let mut restored = Session::open(&game_dir, &disk.map).expect("open load map");
        let checkpoint_position = restored
            .restore_checkpoint(&disk)
            .expect("restore checkpoint");
        assert_eq!(restored.player_health(), Some(disk.health));
        assert_eq!(
            restored
                .objective_states()
                .iter()
                .map(|x| (x.completed, x.primary, x.anti_goal))
                .collect::<Vec<_>>(),
            disk.objectives
                .iter()
                .map(|x| (x.completed, x.primary, x.anti_goal))
                .collect::<Vec<_>>()
        );
        assert!(checkpoint_position.iter().all(|v| v.is_finite()));
        assert_eq!(disk.map, "Plage00");
        assert_eq!(disk.teleporter, "PlayerStart");
        let _ = std::fs::remove_dir_all(dir);
        println!(
            "[save test] restored map={} teleporter={} health={} objectives={}",
            disk.map,
            disk.teleporter,
            disk.health,
            disk.objectives.len()
        );
    }

    /// Opt-in corpus probe (item3o requirement 2): the player message path through
    /// `Engine.LocalMessage.ClientReceive` -> `XIIIBaseHud.LocalizedMessage` -> `HudMessage`
    /// `SetUpLocalizedMessage` must not suspend. The item3m residual error
    /// (`TypeMismatch expected "string", found "void"` at `SetUpLocalizedMessage` code 0x0077) is
    /// not reproducible at HEAD; this drives the generic `AddHudMessage` branch (a message class
    /// outside the four special-cased ones) for several switches and asserts each call returns
    /// `Ok` and that a `HudMessage` is spawned on the HUD's `HudMsg`.
    #[test]
    fn opt_in_plage00_generic_hud_message_path_does_not_suspend() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let mut session = Session::open(&game_dir, "Plage00").expect("open Plage00");
        let pawn = session.player_pawn_actors()[0].0;
        let mut errors = Vec::new();
        {
            let vm = session.vm_mut();
            for path in [
                "xiii.XIIIDeathMessage",
                "xiii.XIIISoloMessage",
                "engine.GameMessage",
            ] {
                let Some(class) = runtime::resolve_class_path(vm.set(), path) else {
                    errors.push(format!("{path}: class not found"));
                    continue;
                };
                for switch in 0..4 {
                    let args = vec![
                        Value::Object(Some(ObjRef::Static(class))),
                        Value::Int(switch),
                        Value::Object(None),
                        Value::Object(None),
                        Value::Object(None),
                    ];
                    if let Err(e) = vm.send_event(pawn, "ReceiveLocalizedMessage", args) {
                        errors.push(format!("{path} switch={switch}: {e}"));
                    }
                }
            }
        }
        assert!(errors.is_empty(), "message path suspended: {errors:?}");
        let spawned = session
            .vm()
            .find_object("XIIIBaseHud")
            .and_then(|hud| session.vm().get_property(hud, "HudMsg").cloned());
        assert!(
            matches!(spawned, Some(Value::Object(Some(ObjRef::Instance(_))))),
            "no HudMessage was spawned through the generic path: {spawned:?}"
        );
        println!("[hud test] generic message path Ok; HudMsg={spawned:?}");
    }
}
