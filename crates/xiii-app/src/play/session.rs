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
    /// Next VM time at which the host AI fire bridge may fire each controlled weapon. The engine
    /// fires from `AWeapon::Tick` (native); the VM has no weapon tick, so the host re-issues the
    /// weapon's class `Fire` while the controller requests fire, throttled per weapon.
    ai_fire_at: std::collections::HashMap<ObjectId, f64>,
    /// AI shots the host fire bridge issued: `(time, soldier, weapon)`.
    pub ai_shots: VecDeque<(f64, String, String)>,
    /// Fixed steps run.
    pub tick_count: u64,
    /// Set once the end-game has stopped the active cutscene controllers (item15 bridge): the
    /// decoded `CineController2.Interpret` keeps `GotoState('NoControl')`-ing the player, which
    /// would clobber `XIIIPlayerController.GameEndedSuccess` if the intro were still running when
    /// the level ends.
    cine_stopped: bool,
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
    /// The script raised on the transition.
    Error(String),
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
            hit_zones,
            hitbox_meshes,
            hitbox_dropped,
            hitbox_errors,
            perception_log: VecDeque::new(),
            ai_fire_at: std::collections::HashMap::new(),
            ai_shots: VecDeque::new(),
            tick_count: 0,
            cine_stopped: false,
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

    /// item14b host AI fire bridge (labelled). The engine fires an AI weapon from its native
    /// `AWeapon::Tick` while the controller's `bTire` is set. The VM has no weapon tick and
    /// `XIIIWeapon.Active`'s `Fire` shadow is empty, so `IAController.Timer`'s
    /// `pawn.weapon.fire(1.0)` resolves to nothing. This re-issues the weapon's **class**
    /// `Fire(1.0)` (the same resolution the player's fire path uses, item14) for every live
    /// `IAController` whose `bTire` is set and whose pawn is alive, throttled by the pawn's
    /// `OffsetTimeBetweenShots`. It is not the engine's native tick and is reported per shot.
    fn ai_fire(&mut self) {
        let now = self.vm.time;
        let mut candidates: Vec<(ObjectId, ObjectId, ObjectId, Option<ObjectId>)> = Vec::new();
        for i in 0..self.vm.objects.len() {
            let ctrl = i as ObjectId;
            let o = &self.vm.objects[i];
            if !o.is_actor
                || o.deleted
                || !o.active
                || !self.vm.is_a(ctrl, "iacontroller")
                || !matches!(self.vm.get_property(ctrl, "bTire"), Some(Value::Bool(true)))
            {
                continue;
            }
            let Some(pawn) = instance_prop(&self.vm, ctrl, "Pawn") else {
                continue;
            };
            if matches!(
                self.vm.get_property(pawn, "bIsDead"),
                Some(Value::Bool(true))
            ) {
                continue;
            }
            let Some(weapon) = instance_prop(&self.vm, pawn, "Weapon") else {
                continue;
            };
            let enemy = instance_prop(&self.vm, ctrl, "Enemy");
            candidates.push((ctrl, pawn, weapon, enemy));
        }
        for (ctrl, pawn, weapon, enemy) in candidates {
            let due = self.ai_fire_at.get(&weapon).copied().unwrap_or(0.0);
            if now < due {
                continue;
            }
            let gap = match self.vm.get_property(pawn, "OffsetTimeBetweenShots") {
                Some(Value::Float(f)) if *f > 0.0 => f64::from(*f),
                _ => 0.4,
            };
            self.ai_fire_at.insert(weapon, now + gap);
            let soldier = self.vm.objects[pawn as usize].name.clone();
            let weapon_name = self.vm.objects[weapon as usize].name.clone();
            // Engine `AIController`-native focus: turn the pawn and its controller toward the
            // enemy and keep the weapon at the eye (the host owns no AI rotation code otherwise).
            let pl = self.vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
            if let Some(enemy) = enemy {
                let el = self.vm.vector_prop(enemy, "Location").unwrap_or(pl);
                let d = [el[0] - pl[0], el[1] - pl[1], el[2] - pl[2]];
                let horiz = (d[0] * d[0] + d[1] * d[1]).sqrt();
                let yaw = d[1].atan2(d[0]);
                let pitch = d[2].atan2(horiz.max(1e-6));
                let rot = [
                    (pitch / std::f32::consts::TAU * ROTATOR_UNITS_PER_TURN).round() as i32,
                    (yaw / std::f32::consts::TAU * ROTATOR_UNITS_PER_TURN).round() as i32,
                    0,
                ];
                let _ = self
                    .vm
                    .set_property(ctrl, "Rotation", 0, Value::Rotator(rot));
                let _ = self
                    .vm
                    .set_property(pawn, "Rotation", 0, Value::Rotator(rot));
            }
            let eye = match self.vm.get_property(pawn, "EyeHeight") {
                Some(Value::Float(h)) => *h,
                _ => match self.vm.get_property(pawn, "BaseEyeHeight") {
                    Some(Value::Float(h)) => *h,
                    _ => 0.0,
                },
            };
            let _ = self.vm.set_property(
                weapon,
                "Location",
                0,
                Value::Vector([pl[0], pl[1], pl[2] + eye]),
            );
            match self.vm.class_function(weapon, "Fire") {
                Some(f) => match self.vm.call_function(f, weapon, vec![Value::Float(1.0)]) {
                    Ok(_) => {
                        println!("[play] AI t={now:.3}s {soldier} fires {weapon_name}");
                        self.ai_shots.push_back((now, soldier, weapon_name));
                    }
                    Err(e) => {
                        self.record_failure(&soldier, &e);
                    }
                },
                None => {
                    let _ = ctrl;
                    self.blocked.push(format!(
                        "AI fire: {soldier} {weapon_name} has no class Fire"
                    ));
                }
            }
        }
        while self.ai_shots.len() > 128 {
            self.ai_shots.pop_front();
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
        // Refresh the posed hit boxes before the VM traces (host `fire` and any script trace in
        // this tick see the current body pose).
        self.update_hit_boxes();
        let profiling = self.vm.native_profile().enabled;
        let t0 = Instant::now();
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
            let _ = self
                .vm
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
        let _ = self
            .vm
            .set_property(self.player, "bIsCrouched", 0, Value::Bool(modes.crouched));
        let _ = self.vm.set_property(
            self.player,
            "bWantsToCrouch",
            0,
            Value::Bool(modes.crouched),
        );
        let _ = self
            .vm
            .set_property(self.player, "bUnderWater", 0, Value::Bool(modes.in_water));
        let _ = self
            .vm
            .set_property(self.player, "Physics", 0, Value::Byte(modes.physics));
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
        if modes.landed_velocity_z.is_some() {
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
        // The engine fires AI weapons from the native weapon tick; the host bridge re-issues the
        // weapon's class `Fire` while the controller requests fire (see [`Session::ai_fire`]).
        self.ai_fire();
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
        if profiling {
            self.vm.native_profile_mut().sync_micros += t0.elapsed().as_micros() as u64;
        }
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
        if !stopped.is_empty() {
            println!(
                "[play] game ended: stopped {} cutscene controller(s): {}",
                stopped.len(),
                stopped.join(", ")
            );
        }
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

    /// Diagnostic weapon bootstrap for `--play-script` (item14): spawn `class_path`, run the
    /// game's own `Weapon.GiveTo`/`BringUp`, and wire the player's `Weapon`/`PendingWeapon` so the
    /// normal fire path works. The campaign maps start the player with `XIII.Fists`; the real
    /// pickup/equip chain (`Pickup.Touch` -> `Pawn.AddInventory` -> `ChangedWeapon`) needs a walk
    /// to a map pickup, which the deterministic demonstration does not include. This grant is
    /// reported, never silent, and the firing/damage/death behaviour is still the game's scripts.
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
        // The normal path sets these in `Inventory.GiveTo`/`ChangedWeapon`; wire them explicitly.
        self.vm.set_property(id, "Instigator", 0, p.clone());
        self.vm.set_property(id, "Owner", 0, p.clone());
        // The game's own GiveTo adds the weapon to the inventory chain and creates its ammo.
        let give = self.vm.send_event(id, "GiveTo", vec![p.clone()]);
        self.vm
            .set_property(pawn, "Weapon", 0, Value::Object(Some(ObjRef::Instance(id))));
        self.vm.set_property(
            pawn,
            "PendingWeapon",
            0,
            Value::Object(Some(ObjRef::Instance(id))),
        );
        // `Weapon.GiveTo` normally runs `GiveAmmo` + `AmmoType.AddAmmo(ReloadCount)`; the HUD
        // notification inside GiveTo is a deferred message native, so run the ammo half here.
        let give_ammo = self.vm.send_event(id, "GiveAmmo", vec![p.clone()]);
        if let Some(ammo) = instance_prop(&self.vm, id, "AmmoType") {
            let amount = match self.vm.get_property(id, "ReloadCount") {
                Some(Value::Int(n)) if *n > 0 => *n,
                _ => 13,
            };
            self.vm
                .set_property(ammo, "AmmoAmount", 0, Value::Int(amount));
            self.vm
                .set_property(ammo, "MaxAmmo", 0, Value::Int(amount.max(50)));
        }
        let bring = self.vm.send_event(id, "BringUp", vec![]);
        let name = self.vm.objects[id as usize].name.clone();
        if let Err(e) = &give {
            // Not fatal: the explicit wiring above still arms the weapon, and `GiveAmmo` above
            // creates the ammo. Reported so a real regression is visible.
            self.blocked
                .push(format!("grant_weapon {name} GiveTo: {e}"));
        }
        Ok(format!(
            "granted {name} ({class_path}); GiveTo {give:?}, GiveAmmo {give_ammo:?}, BringUp {bring:?}"
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

    /// Opt-in corpus (item14b requirement 2): the map's real `XIII.BerettaPick` (`BerettaPick0`)
    /// sits at `(-737.654,-511.886,1262.99)` UU, ~3.4 m from `PlayerStart0`, but the player
    /// spawns **inside the Plage01 hut** and every direct walk from the spawn / an open-side
    /// harness teleport stops at hut geometry (`x=-657` from +X, no movement from -X/-Y) more
    /// than 64 UU short of the pickup's touch radius. The pickup is therefore **not reachable
    /// early by walking**; the item14b demonstration keeps the labelled `weapon XIII.Beretta`
    /// grant for the synthetic zone test, and the real pickup chain itself is exercised by the
    /// `opt_in_plage01_key_pickup_without_host_grant_opens_porte6` test on the same map. This
    /// test records the measured block (it asserts the negative, so a future reachability fix
    /// fails it visibly).
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
        println!(
            "[ai test] {} AI shots, first at {:?}",
            session.ai_shots.len(),
            session.ai_shots.front()
        );
        println!(
            "[ai test] player Health {hp0} -> {hp1} (low {hp_low}), first Attaque at {first_attack:?}s"
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
        assert!(
            !session.ai_shots.is_empty(),
            "the soldier never fired its weapon"
        );
        // The game's own damage chain reduced the player's Health.
        assert!(
            hp1 < hp0,
            "the soldier's fire did not reduce the player's Health ({hp0} -> {hp1}); first error {:?}",
            session.first_error()
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
