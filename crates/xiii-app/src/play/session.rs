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
use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::rc::Rc;

use xiii_package::Limits;
use xiii_script::vm::MoverState;
use xiii_script::{ObjRef, ObjectId, PresentationEvent, ScriptSet, Value, Vm, VmError, VmLimits};
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
    /// First failure, formatted with its script stack.
    pub first_error: Option<String>,
    /// Last synced VM `Location` per live actor (for the one-way render sync).
    last_synced: HashMap<ObjectId, [f32; 3]>,
    /// Presentation events, most recent last (bounded).
    pub events: VecDeque<(f64, PresentationEvent)>,
    /// `Touch` events involving the player, most recent last (bounded).
    pub touches: VecDeque<(f64, String)>,
    player_touching: Vec<ObjectId>,
    /// Actors the VM moved this step (cleared at the start of [`Session::step`]).
    pub moved: Vec<MovedActor>,
    /// `XIIIDispatcher0`, if the map has one (the trigger chain's end state).
    pub dispatcher: Option<ObjectId>,
    /// Fixed steps run.
    pub tick_count: u64,
    /// Key inventory items the host spawned from touched `KeyPicks` actors (host shortcut; see
    /// [`Session::grant_touched_key`]).
    key_items: Vec<ObjectId>,
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
    /// The script raised on the transition.
    Error(String),
}

impl Session {
    /// Loads the map's script set, builds and installs the real map providers, activates every
    /// map actor, runs the level start tolerantly and creates the player pawn (script path when
    /// it works, explicit harness bootstrap otherwise).
    pub fn open(game_dir: &Path, map: &str) -> Result<Session, String> {
        let (set, map_idx) = runtime::load_with_map(game_dir, map)?;
        let set: &'static ScriptSet = Box::leak(Box::new(set));
        let mut vm = Vm::new(set, VmLimits::default());

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
        // `GameInfo.InitGame`/`Login` read it before any actor begins play.
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

        let dispatcher = vm.find_object("XIIIDispatcher0");

        // Baseline for the one-way render sync and the initial touch state.
        let mut last_synced = HashMap::new();
        for (i, o) in vm.objects.iter().enumerate() {
            if o.is_actor
                && !o.deleted
                && let Some(l) = vm.vector_prop(i as ObjectId, "Location")
            {
                last_synced.insert(i as ObjectId, l);
            }
        }

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
            first_error: None,
            last_synced,
            events: VecDeque::new(),
            touches: VecDeque::new(),
            player_touching: Vec::new(),
            moved: Vec::new(),
            dispatcher,
            tick_count: 0,
            key_items: Vec::new(),
        };
        session.suspended.dedup();
        session.drain_events();
        session.update_touches();
        Ok(session)
    }

    /// One fixed step, in the documented order: write the player pawn state (owned by the
    /// movement simulation) into the VM, refresh its touches, tick the VM tolerantly, drain the
    /// presentation events and record the actors the VM moved.
    pub fn step(&mut self, dt: f32, location: [f32; 3], yaw: f32, velocity: [f32; 3]) {
        self.moved.clear();
        let _ = self
            .vm
            .set_property(self.player, "Location", 0, Value::Vector(location));
        let _ = self
            .vm
            .set_property(self.player, "Velocity", 0, Value::Vector(velocity));
        let yaw_units = (yaw / std::f32::consts::TAU * ROTATOR_UNITS_PER_TURN).round() as i32;
        let _ = self.vm.set_property(
            self.player,
            "Rotation",
            0,
            Value::Rotator([0, yaw_units, 0]),
        );

        // VM touch update for the host-moved player (the walk into a trigger volume).
        if let Err(e) = self.vm.refresh_touching_of(self.player) {
            self.suspend(self.player, &e);
        }
        self.drain_events();

        for (id, e) in self.vm.tick_suspending(dt) {
            self.suspend(id, &e);
        }
        self.tick_count += 1;
        self.drain_events();
        self.update_touches();
        self.update_sync();
    }

    /// VM time in seconds.
    pub fn vm_time(&self) -> f64 {
        self.vm.time
    }

    /// Read-only access to the script VM for host-side queries (the `--play` pawn renderer reads
    /// actor locations, rotations, meshes and animation channels; it never mutates the VM).
    pub fn vm(&self) -> &Vm<'static> {
        &self.vm
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

    /// True when the player has touched a live actor whose name contains `needle`
    /// (case-insensitive). Recorded from the VM's own `Touch` updates.
    pub fn player_touched(&self, needle: &str) -> bool {
        let needle = needle.to_ascii_lowercase();
        self.touches
            .iter()
            .any(|(_, a)| a.to_ascii_lowercase().contains(&needle))
    }

    /// Spawns (once) the inventory item carried by a `KeyPicks` actor the player has touched and
    /// returns it. **Host shortcut**: the full `Pickup.Touch` -> `GiveTo` -> `Keys.Activate`
    /// inventory flow is not implemented; the host spawns the pickup's own `InventoryType` (its
    /// class default) and hands it to the door's own `Trigger`, so the door still validates the
    /// key against its `UnlockItemCode`/`UnLockItemName`.
    pub fn grant_touched_key(&mut self) -> Option<ObjectId> {
        self.key_items
            .retain(|k| self.vm.objects.get(*k as usize).is_some_and(|o| !o.deleted));
        if let Some(&k) = self.key_items.first() {
            return Some(k);
        }
        let mut class = None;
        for (i, o) in self.vm.objects.iter().enumerate() {
            if !o.is_actor || o.deleted || !self.vm.is_a(i as ObjectId, "keypicks") {
                continue;
            }
            if !self.player_touched(&o.name) {
                continue;
            }
            if let Some(Value::Object(Some(ObjRef::Static(g)))) =
                self.vm.get_property(i as ObjectId, "InventoryType")
            {
                class = Some(*g);
                break;
            }
        }
        let class = class?;
        let id = self.vm.spawn(class, "HeldKey(play)").ok()?;
        self.key_items.push(id);
        Some(id)
    }

    /// Host shortcut for a locked door when the key was not carried: spawns the `InventoryType`
    /// class of the first live `KeyPicks` actor in the level and returns it, printing a clear
    /// notice. **Deviation**: the `Pickup.Touch` -> `GiveTo` -> `Keys.Activate` inventory chain
    /// needs natives that are out of this task's scope; the door still validates the key against
    /// its own `UnlockItemCode`/`UnLockItemName`, so the unlock state machine is the game's.
    fn grant_level_key(&mut self) -> Option<ObjectId> {
        let mut class = None;
        for (i, o) in self.vm.objects.iter().enumerate() {
            if !o.is_actor || o.deleted || !self.vm.is_a(i as ObjectId, "keypicks") {
                continue;
            }
            if let Some(Value::Object(Some(ObjRef::Static(g)))) =
                self.vm.get_property(i as ObjectId, "InventoryType")
            {
                println!(
                    "[play] host shortcut: granting key {} for a locked door \
                     (Pickup/GiveTo inventory chain not implemented)",
                    o.name
                );
                class = Some(*g);
                break;
            }
        }
        let class = class?;
        let id = self.vm.spawn(class, "HeldKey(play)").ok()?;
        self.key_items.push(id);
        Some(id)
    }

    /// Host use action (`E` in `--play`, `use` in a script). Mirrors the tail of
    /// `XIIIPlayerController.Grab` for a mover target: a locked `XIIIPorte` is unlocked with the
    /// key via its own `Trigger` (`TryPickLock`), any other state is opened via `PlayerTrigger`.
    pub fn use_mover(&mut self, target_name: &str) -> UseOutcome {
        let Some(target) = self.vm.find_object(target_name) else {
            return UseOutcome::NotAMover;
        };
        if !self.vm.is_mover(target) {
            return UseOutcome::NotAMover;
        }
        let pawn = self.player;
        let controller = self.controller.unwrap_or(pawn);
        if self.vm.is_in_state(target, "Locked") {
            let key = self.grant_touched_key().or_else(|| self.grant_level_key());
            let Some(key) = key else {
                // No key available: run the door's own `Locked.PlayerTrigger` (plays the locked
                // sound) and report Locked; this is visible, not a silent success.
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

    /// First failure formatted with its stack, if any.
    pub fn first_error(&self) -> Option<&str> {
        self.first_error.as_deref()
    }

    fn drain_events(&mut self) {
        for ev in self.vm.drain_events() {
            let t = self.vm.time;
            self.events.push_back((t, ev));
        }
        while self.events.len() > 64 {
            self.events.pop_front();
        }
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
            }
        }
        self.player_touching = now;
        while self.touches.len() > 64 {
            self.touches.pop_front();
        }
    }

    fn update_sync(&mut self) {
        let mut moved = Vec::new();
        for (i, o) in self.vm.objects.iter().enumerate() {
            if !o.is_actor || o.deleted {
                continue;
            }
            let id = i as ObjectId;
            let Some(cur) = self.vm.vector_prop(id, "Location") else {
                continue;
            };
            let base = *self.last_synced.entry(id).or_insert(cur);
            if base != cur {
                moved.push((o.name.clone(), render_delta(base, cur)));
                self.last_synced.insert(id, cur);
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
        if self.first_error.is_none() {
            self.first_error = Some(format!("{e}"));
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
}
