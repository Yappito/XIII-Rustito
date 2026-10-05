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
    /// Active language code of the install's localisation files (`int`, `frt`, ...).
    pub localization_language: String,
    /// Number of `localized` class-default values filled from the `.int` files while building
    /// the level's class layouts.
    pub localized_overrides: u64,
    /// Fixed steps run.
    pub tick_count: u64,
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
            localization_language,
            localized_overrides: 0,
            tick_count: 0,
        };
        session.localized_overrides = session.vm.localized_overrides;
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
                session.step(1.0 / 60.0, loc, 0.0, [0.0; 3]);
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
}
