//! Text input scripts for the deterministic `--play-script` driver.
//!
//! One command per line: `t=<seconds> <command> [args]`. Blank lines and `#` comments are
//! ignored. Commands are applied in time order at the start of the fixed tick whose elapsed
//! time reaches `t`; the resulting [`Input`] drives the same [`crate::play::sim::PlayerSim`]
//! the interactive window uses.
//!
//! Supported commands:
//! - `switch_weapon <byte>` / `next_weapon`: normal controller weapon selection execs.
//! - `select_weapon <byte>`: the retail number-key exec (DefUser.ini `8=SelectWeapon 14`), which
//!   also leaves item mode (`XIIIPlayerController.SelectWeapon` -> `SwitchWeaponMode`).
//! - `forward <v>` / `back <v>`: set the forward axis (`back` negates `v`).
//! - `right <v>` / `left <v>`: set the strafe axis (`left` negates `v`).
//! - `walk <0|1|on|off>`: set the Shift walk modifier.
//! - `crouch <0|1|on|off>`: hold the crouch key (XIII `C=Duck`).
//! - `jump`: request one jump (edge-triggered, consumed by the next tick).
//! - `yaw <degrees>`: set absolute yaw (Unreal convention: 0 = +X, positive toward +Y).
//! - `turn <degrees>`: add to yaw.
//! - `pitch <degrees>`: set camera pitch.
//! - `track <actor|off>`: continuously aim at a named actor (resolved by the playback host).
//! - `follow <actor|off> [distance]`: continuously aim at and trail a named actor (resolved by the
//!   playback host), holding the forward axis whenever the horizontal distance exceeds `distance`
//!   (default 200 UU) and releasing it inside. This is the same class of autopilot input as `goto`
//!   (the simulation still owns the movement; it is not a teleport): a campaign route uses it to
//!   walk behind an escorted NPC without a sensor bridge to the escort's own state.
//! - `use` (alias `grab`/`interact`): request one use/interact action (edge-triggered); the
//!   host runs the VM's mover lock/unlock/open chain (`Session::use_mover`).
//! - `use <ActorName>`: use/interact with a named actor directly (edges around hidden interaction
//!   doors and dynamic pawns the camera ray cannot pick).
//! - `take_control` (alias `assume_control`): explicit diagnostic command that runs the
//!   controller's own `EnterStartState` with `bOkForMoving = true`. No normal interactive or
//!   campaign route issues this command; it is available only in a supplied `--play-script`.
//! - `teleport <x> <y> <z>` (alias `place`): move the player box centre to this Unreal-unit
//!   position and drop the velocity. Used by the trigger demonstration because the Plage00
//!   trigger is ~47,000 UU from the PlayerStart (about 100 s of walking at `GroundSpeed`). The
//!   final approach into the trigger volume is still walked.
//! - `goto <x> <y> [z]`: autopilot to an Unreal-unit waypoint. Each tick the driver re-aims the
//!   player (yaw toward the waypoint) and holds the forward axis until it is within a small
//!   radius, then stops. The simulation still owns the movement (this is not a teleport); it is
//!   used to follow a decoded `ReachSpec` path through a room)
//! - `goto off`: cancel waypoint steering, retaining the explicitly set movement axes. This
//!   permits timed yaw/axis/jump input to take over for a running jump.
//!
//! Closed-loop commands (item107). Each one computes ordinary player input every tick from what
//! the player can perceive; none of them sets game state, positions, health, inventory or AI
//! state. A *blocking* command pauses the script clock: the events after it keep their relative
//! timing and apply when the block ends, so a route survives a fix that shifts combat timing.
//! While blocked, the axes, `goto` steering and held buttons set earlier keep running. Every
//! block has a timeout; when it expires the script continues (and a note is printed), so a route
//! never hangs.
//! - `engage [Actor] [radius] [secs] [aim=<f>] [hold=<s>] [period=<s>] [linger=<s>]`: blocking. While a
//!   living hostile pawn is visible within `radius` Unreal units (default 3000) the player turns
//!   to it (the `track` aiming, at `aim` x CollisionHeight above its Location, default 0.85: the
//!   head band) and presses the fire button; the nearest visible one is chosen and kept while it
//!   stays visible. Trigger discipline: the button is pressed (the game's own `Fire` exec on the
//!   rising edge, `UnFire` on release), held for `hold` seconds (default 0: a one-tick tap) and
//!   pressed again every `period` seconds (default 0.25), the cadence of the recorded routes;
//!   measured on Hual04a, a held M16 (15 rounds a second) sprayed 30 rounds for 6 hp of damage at
//!   1500 UU where the same rifle tapped killed. Out of loaded ammo (the HUD count, or a dry
//!   shot) the player presses `Reload`, and when that does not refill the weapon within 2.5 s
//!   switches to a carried ranged weapon that has ammo; with none the block ends (`OUT OF
//!   AMMO`) and the script decides. Fists are never fired at range while a gun is carried.
//!   Pending `goto` steering pauses while a target is engaged and resumes afterwards. The block
//!   ends when no hostile has been visible for `linger` seconds (default 1; a wave that arrives
//!   in pieces needs more) after at least one was engaged, or after
//!   `secs` (default 30), whichever is first. With an `Actor` name only that actor is engaged
//!   (it may be a non-pawn such as a destructible mover or a helicopter: `aim` then points at
//!   its raw Location) and the block also ends when it is dead. "Visible" is a clear line from
//!   the player's eye through the level geometry (the half of `LineOfSightTo` that the bullets
//!   share; no field-of-view limit: a player turns toward what shoots them); "hostile" is a
//!   living pawn driven by an `IAController` whose `InitialAlliances` entry for the player's
//!   alliance is negative (the soldier's authored enmity towards the player).
//!   A target that shots do not hurt (its Health has not fallen across 8 trigger presses and
//!   2.5 s; the bullets go into cover that the sight line does not show) is given up on for 6 s
//!   while another hostile is visible, as a player turns to another enemy when the health bar
//!   stays full.
//! - `engage_on [radius] [aim=<f>] [hold=<s>] [period=<s>]` / `engage_off`: the same shooting as a
//!   non-blocking *standing* engagement. The script clock keeps running, so a route whose walking
//!   schedule is tied to the game's own timing (waves that spawn on timers) keeps its schedule
//!   while the aiming and firing follow whatever hostile is visible, whenever that is. It never
//!   ends by itself (only `engage_off`, or a later `engage_on` that replaces it); it yields the
//!   trigger to a blocking `engage` or `loot` while one runs; a pending `goto` pauses while a
//!   target is engaged and resumes after. It engages visible hostile pawns only (name a target
//!   with the blocking form).
//! - `loot [radius] [secs]`: blocking. Walks onto the nearest visible dead pawn within `radius`
//!   (default 1500 UU) that still carries inventory, one corpse after another, as a player
//!   collects a fallen soldier's weapon (the game hands a corpse's weapon, ammo and keys over on
//!   contact). A corpse that was seen stays the target when the player walks out of sight of it;
//!   each is visited once. Ends when none is left, or after `secs` (default 20).
//! - `wait_until <cond> [or|and <cond> ...] [timeout]`: blocking; continue when the condition
//!   holds (default timeout 60 s). Conditions: `dead <Actor>` (a dead pawn, or an actor that has
//!   been destroyed), `alive <Actor>`, `visible <Actor>` (alive and in line of sight),
//!   `hostile_visible <radius>`, `goal <N>` (objective N completed), `event <Target>
//!   [FunctionSuffix]` (the VM trace holds an event call on that actor whose function ends with
//!   the suffix, default `.Trigger`; needs the VM trace, which the headless runner keeps and the
//!   windowed `--play` does not), `travel` (a level travel was requested after the block
//!   began), `health_below <N>` / `health_above <N>` (the player's Health) and `arrived` (no
//!   `goto` waypoint pending). `not <cond>` negates one condition.
//! - `goto_until <x> <y> <cond> [timeout]`: `goto <x> <y>`, then block until the player arrives
//!   or the condition holds (default timeout 60 s).

use std::collections::HashSet;
use std::path::Path;

use crate::play::sim::{Input, PlayerSim};
use xiii_script::{TraceEvent, TraceKind};

/// One parsed command.
#[derive(Debug, Clone, PartialEq)]
pub struct Event {
    /// Trigger time in seconds.
    pub t: f32,
    /// The command.
    pub command: Command,
}

/// A parsed script command.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    /// Forward axis value.
    Forward(f32),
    /// Strafe axis value.
    Right(f32),
    /// Walk modifier.
    Walk(bool),
    /// Crouch held.
    Crouch(bool),
    /// Jump once.
    Jump,
    /// Absolute yaw in degrees.
    Yaw(f32),
    /// Relative yaw in degrees.
    Turn(f32),
    /// Pitch in degrees.
    Pitch(f32),
    /// Continuously aim at an actor; `None` disables tracking. The f32 is the aim height as a
    /// fraction of the tracked pawn's CollisionHeight (0.6 default: spine band; a level ray at
    /// 0.85 crosses the head band for 3x bullet damage, see mod.rs track sites).
    Track(Option<(String, f32)>),
    /// Continuously aim at and trail a named actor, holding forward beyond the stop distance
    /// (default 200 UU). `None` disables it. Like `Track`, the host supplies the live location.
    Follow(Option<(String, f32)>),
    /// Press the player's Reload exec, as the R binding does.
    Reload,
    /// Dispatch the controller's ordinary Grab exec using its script-selected aim target.
    GrabInput,
    /// Move the box centre to an absolute Unreal-unit position (harness bootstrap).
    Teleport([f32; 3]),
    /// Autopilot toward an Unreal-unit waypoint (the driver re-aims and walks; not a teleport).
    Goto([f32; 3]),
    /// Cancel waypoint steering without changing position, velocity or movement axes.
    StopGoto,
    /// Request one use/interact action (edge-triggered; the VM `Grab`/use chain). Ray-based: the
    /// host picks the actor in front of the camera.
    Use,
    /// Named use (edge-triggered); deco pickups require the game's current aimed TargetActor.
    /// Needed for invisible interaction
    /// doors (Plage01 `Porte1`); corpses and pickups require the game's aimed TargetActor.
    UseNamed(String),
    /// Request one fire action (edge-triggered; routed to the player's weapon, item14).
    Fire,
    /// item64: hold/release the fire button (DefUser.ini `Fire` = `Button bFire | Fire`). The
    /// rising edge dispatches the game's own `XIIIPlayerController.Fire` exec; while held the
    /// controller's `bFire` is published each tick (`Pawn.PressingFire` polls — the rope climbs
    /// while held). `fire_hold 0` releases (`onrelease UnFire`).
    FireHold(bool),
    /// item64: hold/release the alt-fire button (DefUser.ini `AltFire` = `Button bAltFire |
    /// AltFire`); the rising edge dispatches the `AltFire` exec (the rope descends while held).
    AltFireHold(bool),
    /// item64: grant the player the named `Package.Class` inventory item and select it through
    /// `GiveTo` + `ServerChangedItem` (the hook's route bootstrap; the campaign grants it from a
    /// save the route does not load).
    Item(String),
    /// item64: the item-cycle press (DefUser.ini `InventoryNext`, the F/G keys); the hook's
    /// put-away/unhook input.
    NextItem,
    /// Grant the player the named `Package.Class` weapon (item14 diagnostic bootstrap; the
    /// gameplay maps start the player with `XIII.Fists`, so a demonstration weapon is granted
    /// through the game's own `Weapon.GiveTo`/`BringUp`).
    Weapon(String),
    /// Block the script until the game requests level travel (item15). The host reloads the next
    /// map; events after `wait_travel` apply from the reloaded session. In the headless
    /// `--play-script` mode the run ends at the travel request (the next map is a fresh run).
    WaitTravel,
    /// Call the map's own `MapInfo.SetGoalComplete(N)` (item15 demonstration bridge). The decoded
    /// campaign fires goals from cutscene `TriggerEvent`s the host does not yet play; this invokes
    /// the same game function the goal trigger calls, so `TestGoalComplete`/`DoTravel`/`EndGame`/
    /// `ServerTravel` all run through the game's code. Labelled a bridge in the report.
    SetGoal(i32),
    /// Normal player controller SwitchWeapon exec, selecting a carried inventory group.
    SwitchWeapon(u8),
    /// Retail number-key `SelectWeapon` exec (selects a group, leaving item mode if needed).
    SelectWeapon(u8),
    /// Normal player controller NextWeapon exec.
    NextWeapon,
    /// item18 diagnostic command: give the local player control by running the game's own
    /// `XIIIPlayerController.EnterStartState` with `bOkForMoving = true` (the HUD's normal
    /// "first display done" transition). Needed because the decoded Plage01 intro leaves the
    /// controller frozen in `NoControl` when a diagnostic script does not play the authored
    /// cutscene sequence. Normal campaign play does not depend on this command.
    TakeControl,
    /// Retail H-key: run the game's own `XIIIPlayerController.QuickHeal` exec (decoded: picks the
    /// MedKit/FullMedKit from the pawn's inventory by `HealthPercent` and calls
    /// `Med.UseMeQuick`). Edge-triggered like `use`.
    QuickHeal,
    /// item72: open the retail in-game menu (the same decoded `ShowTheMenu` path the
    /// `Escape=Showmenu` binding runs).
    Menu,
    /// item72: close the in-game menu through the page's own Esc handler (resume).
    MenuClose,
    /// item82: send one key through the active retail menu page's key-event dispatcher.
    MenuKey(u8),
    /// item72: save a screenshot of the current frame to this path (repeatable).
    Shot(String),
    /// item107: blocking closed-loop engagement of visible hostiles (see the module docs).
    Engage(EngageSpec),
    /// item107: blocking walk onto visible corpses that still carry a weapon (see the module docs).
    Loot(LootSpec),
    /// item107: non-blocking standing engagement (`engage_on`): the script clock keeps running
    /// while the player shoots at whatever hostile is visible (see the module docs).
    EngageOn(EngageSpec),
    /// item107: ends a standing engagement (`engage_off`).
    EngageOff,
    /// item107: block until a game-state condition holds or the timeout expires.
    WaitUntil {
        /// The condition.
        cond: Condition,
        /// Timeout in seconds of script clock (the block ends then even if unmet).
        timeout: f32,
    },
    /// item107: `goto` the waypoint, then block until arrival, the condition or the timeout.
    GotoUntil {
        /// Waypoint in Unreal units.
        target: [f32; 3],
        /// Extra end condition (arrival always ends the block).
        cond: Condition,
        /// Timeout in seconds.
        timeout: f32,
    },
}

/// Parameters of an `engage` command.
#[derive(Debug, Clone, PartialEq)]
pub struct EngageSpec {
    /// Engage only this named actor (may be a non-pawn); `None` engages visible hostile pawns.
    pub target: Option<String>,
    /// Maximum distance of a candidate target in Unreal units.
    pub radius: f32,
    /// Block timeout in seconds.
    pub secs: f32,
    /// Aim height as a fraction of the target pawn's CollisionHeight above its Location.
    pub aim: f32,
    /// Seconds the fire button stays down per press (0: a one-tick tap).
    pub hold: f32,
    /// Seconds between two presses.
    pub period: f32,
    /// Seconds with no visible hostile after which an engagement that has fired ends.
    pub linger: f32,
}

/// Parameters of a `loot` command.
#[derive(Debug, Clone, PartialEq)]
pub struct LootSpec {
    /// Maximum distance of a corpse worth walking to, in Unreal units.
    pub radius: f32,
    /// Block timeout in seconds.
    pub secs: f32,
}

/// A single closed-loop condition term.
#[derive(Debug, Clone, PartialEq)]
pub enum Cond {
    /// The named actor is a dead pawn or has been destroyed.
    Dead(String),
    /// The named actor exists and is alive.
    Alive(String),
    /// The named actor is alive and in the player's line of sight.
    Visible(String),
    /// A living hostile pawn is visible within this radius.
    HostileVisible(f32),
    /// Objective N is completed.
    Goal(i32),
    /// The VM trace holds an event call on `target` whose function ends with `suffix`.
    Event {
        /// Target actor name.
        target: String,
        /// Function-path suffix.
        suffix: String,
    },
    /// A level travel was requested after the block began.
    Travel,
    /// Player Health below N.
    HealthBelow(f32),
    /// Player Health above N.
    HealthAbove(f32),
    /// No `goto` waypoint is pending.
    Arrived,
    /// Negation.
    Not(Box<Cond>),
}

/// Condition terms joined by `or` (any) or `and` (all).
#[derive(Debug, Clone, PartialEq)]
pub struct Condition {
    /// The terms (at least one).
    pub terms: Vec<Cond>,
    /// `true` when every term must hold (`and`), `false` when any term suffices (`or`).
    pub all: bool,
}

/// Whether an actor named by a condition exists and is alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ActorState {
    /// No actor of that name exists (not spawned yet).
    #[default]
    Missing,
    /// Live and not dead.
    Alive,
    /// A dead pawn, or an actor that was destroyed.
    Dead,
}

/// What the host reports about one named actor.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ActorFact {
    /// Lowercased actor name.
    pub name: String,
    /// Alive, dead or missing.
    pub state: ActorState,
    /// In the player's line of sight (only meaningful when alive).
    pub visible: bool,
    /// Aim point (Unreal units) when alive and visible.
    pub aim: Option<[f32; 3]>,
    /// Distance from the player's eye in Unreal units.
    pub distance: f32,
}

/// A visible, living hostile pawn.
#[derive(Debug, Clone, PartialEq)]
pub struct Seen {
    /// Actor name.
    pub name: String,
    /// Aim point (Unreal units).
    pub aim: [f32; 3],
    /// Distance from the player's eye in Unreal units.
    pub distance: f32,
    /// In the player's line of sight now (always true for hostiles; a corpse the player has
    /// walked behind a corner stays a known target).
    pub visible: bool,
    /// The pawn's Health, when it is a pawn (the health bar a player reads off a target).
    pub health: Option<f32>,
}

/// What the player's weapon reports to the closed-loop fire logic.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WeaponFact {
    /// Rounds loaded in the weapon (the ammo count a player reads off the HUD), when known.
    pub clip: Option<u32>,
    /// The weapon is a melee weapon (fists): no use at range.
    pub melee: bool,
    /// `InventoryGroup` of the other carried ranged weapons that still have ammo.
    pub alternatives: Vec<u8>,
}

/// One tick of host-gathered perception and game-state facts, valid for that tick only.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Facts {
    /// The player's Health.
    pub player_health: Option<f32>,
    /// Facts about the named actors requested through [`FactRequest::actors`].
    pub actors: Vec<ActorFact>,
    /// Visible living hostile pawns (within [`FactRequest::hostile_radius`]), any order.
    pub hostiles: Vec<Seen>,
    /// Dead pawns that still carry a weapon (within [`FactRequest::corpse_radius`]).
    pub corpses: Vec<Seen>,
    /// Completed state of each objective, by index.
    pub goals: Vec<bool>,
    /// The player's weapon, when requested.
    pub weapon: Option<WeaponFact>,
    /// Player input is suppressed (authored cinematic or menu): no engagement is possible.
    pub suppressed: bool,
}

/// What the active block needs the host to gather this tick.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct FactRequest {
    /// Named actors (as written in the script) to report on.
    pub actors: Vec<String>,
    /// Gather visible hostiles up to this radius.
    pub hostile_radius: Option<f32>,
    /// Gather visible lootable corpses up to this radius.
    pub corpse_radius: Option<f32>,
    /// Aim fraction for the reported aim points.
    pub aim: f32,
    /// Report objective completion.
    pub goals: bool,
    /// Report the weapon facts.
    pub weapon: bool,
}

/// A normal controller weapon-selection exec, in script order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WeaponInput {
    /// `XIIIPlayerController.SwitchWeapon(group)`.
    Switch(u8),
    /// `XIIIPlayerController.SelectWeapon(group)` (the retail number keys).
    Select(u8),
    /// `XIIIPlayerController.NextWeapon`.
    Next,
}

/// Default `engage` candidate radius (Unreal units).
const ENGAGE_RADIUS: f32 = 3000.0;
/// Default `engage` timeout (seconds).
const ENGAGE_SECS: f32 = 30.0;
/// Default `engage` aim fraction of CollisionHeight (the head band).
const ENGAGE_AIM: f32 = 0.85;
/// Default `wait_until`/`goto_until` timeout (seconds).
const WAIT_SECS: f32 = 60.0;
/// Default `loot` radius (Unreal units) and timeout (seconds).
const LOOT_RADIUS: f32 = 1500.0;
const LOOT_SECS: f32 = 20.0;
/// Horizontal distance at which a corpse counts as walked over (Unreal units).
const LOOT_REACH: f32 = 35.0;

fn next_actor<'a>(
    it: &mut std::str::SplitWhitespace<'a>,
    n: usize,
    what: &str,
) -> Result<&'a str, String> {
    it.next()
        .ok_or_else(|| format!("line {n}: {what} needs an actor name"))
}

fn next_number(it: &mut std::str::SplitWhitespace, n: usize, what: &str) -> Result<f32, String> {
    let v = it
        .next()
        .ok_or_else(|| format!("line {n}: {what} needs a number"))?;
    let x = v
        .parse::<f32>()
        .map_err(|_| format!("line {n}: bad number {v:?} for {what}"))?;
    if x.is_finite() {
        Ok(x)
    } else {
        Err(format!("line {n}: {what} needs a finite number"))
    }
}

fn parse_term(it: &mut std::str::SplitWhitespace, n: usize) -> Result<Cond, String> {
    let name = it
        .next()
        .ok_or_else(|| format!("line {n}: missing condition"))?;
    Ok(match name {
        "not" => Cond::Not(Box::new(parse_term(it, n)?)),
        "dead" => Cond::Dead(next_actor(it, n, "dead")?.to_owned()),
        "alive" => Cond::Alive(next_actor(it, n, "alive")?.to_owned()),
        "visible" => Cond::Visible(next_actor(it, n, "visible")?.to_owned()),
        "hostile_visible" => Cond::HostileVisible(next_number(it, n, "hostile_visible")?),
        "goal" => {
            let v = it
                .next()
                .ok_or_else(|| format!("line {n}: goal needs an objective number"))?;
            Cond::Goal(
                v.parse::<i32>()
                    .map_err(|_| format!("line {n}: bad objective number {v:?}"))?,
            )
        }
        "event" => {
            let target = next_actor(it, n, "event")?.to_owned();
            // An optional function suffix: anything that is not a connector or the timeout.
            let suffix = match it.clone().next() {
                Some(tok) if !matches!(tok, "or" | "and") && tok.parse::<f32>().is_err() => {
                    it.next();
                    tok.to_owned()
                }
                _ => ".Trigger".to_owned(),
            };
            Cond::Event { target, suffix }
        }
        "travel" => Cond::Travel,
        "arrived" => Cond::Arrived,
        "health_below" => Cond::HealthBelow(next_number(it, n, "health_below")?),
        "health_above" => Cond::HealthAbove(next_number(it, n, "health_above")?),
        other => return Err(format!("line {n}: unknown condition {other:?}")),
    })
}

/// Parses `<term> [(or|and) <term> ...]` and the optional trailing timeout in seconds.
fn parse_condition(
    it: &mut std::str::SplitWhitespace,
    n: usize,
    default_timeout: f32,
) -> Result<(Condition, f32), String> {
    let mut terms = vec![parse_term(it, n)?];
    let mut connector: Option<bool> = None;
    while let Some(tok @ ("or" | "and")) = it.clone().next() {
        it.next();
        let all = tok == "and";
        if connector.is_some_and(|c| c != all) {
            return Err(format!(
                "line {n}: cannot mix `or` and `and` in one condition"
            ));
        }
        connector = Some(all);
        terms.push(parse_term(it, n)?);
    }
    let timeout = match it.next() {
        None => default_timeout,
        Some(v) => {
            let t = v
                .parse::<f32>()
                .map_err(|_| format!("line {n}: bad timeout {v:?}"))?;
            if !t.is_finite() || t <= 0.0 {
                return Err(format!("line {n}: timeout must be a positive number"));
            }
            if it.next().is_some() {
                return Err(format!("line {n}: unexpected argument after the timeout"));
            }
            t
        }
    };
    Ok((
        Condition {
            terms,
            all: connector.unwrap_or(false),
        },
        timeout,
    ))
}

fn parse_engage(it: &mut std::str::SplitWhitespace, n: usize) -> Result<EngageSpec, String> {
    let mut spec = EngageSpec {
        target: None,
        radius: ENGAGE_RADIUS,
        secs: ENGAGE_SECS,
        aim: ENGAGE_AIM,
        hold: FIRE_HOLD,
        period: FIRE_PERIOD,
        linger: ENGAGE_LINGER_SECS,
    };
    let mut numbers = 0;
    for tok in it {
        if let Some(v) = tok.strip_prefix("hold=") {
            spec.hold = v
                .parse::<f32>()
                .ok()
                .filter(|h| h.is_finite() && *h >= 0.0)
                .ok_or_else(|| format!("line {n}: bad hold seconds {v:?}"))?;
        } else if let Some(v) = tok.strip_prefix("linger=") {
            spec.linger = v
                .parse::<f32>()
                .ok()
                .filter(|l| l.is_finite() && *l >= 0.0)
                .ok_or_else(|| format!("line {n}: bad linger seconds {v:?}"))?;
        } else if let Some(v) = tok.strip_prefix("period=") {
            spec.period = v
                .parse::<f32>()
                .ok()
                .filter(|p| p.is_finite() && *p > 0.0)
                .ok_or_else(|| format!("line {n}: bad period seconds {v:?}"))?;
        } else if let Some(v) = tok.strip_prefix("aim=") {
            spec.aim = v
                .parse::<f32>()
                .ok()
                .filter(|a| a.is_finite() && (0.0..=1.5).contains(a))
                .ok_or_else(|| format!("line {n}: bad aim fraction {v:?}"))?;
        } else if let Ok(x) = tok.parse::<f32>() {
            if !x.is_finite() || x <= 0.0 {
                return Err(format!("line {n}: engage radius/secs must be positive"));
            }
            match numbers {
                0 => spec.radius = x,
                1 => spec.secs = x,
                _ => return Err(format!("line {n}: engage takes at most a radius and secs")),
            }
            numbers += 1;
        } else if spec.target.is_none() && numbers == 0 {
            spec.target = Some(tok.to_owned());
        } else {
            return Err(format!("line {n}: unexpected engage argument {tok:?}"));
        }
    }
    Ok(spec)
}

fn parse_loot(it: &mut std::str::SplitWhitespace, n: usize) -> Result<LootSpec, String> {
    let mut spec = LootSpec {
        radius: LOOT_RADIUS,
        secs: LOOT_SECS,
    };
    for (i, tok) in it.enumerate() {
        let x = tok
            .parse::<f32>()
            .ok()
            .filter(|x| x.is_finite() && *x > 0.0)
            .ok_or_else(|| format!("line {n}: bad loot argument {tok:?}"))?;
        match i {
            0 => spec.radius = x,
            1 => spec.secs = x,
            _ => return Err(format!("line {n}: loot takes at most a radius and secs")),
        }
    }
    Ok(spec)
}

/// A parsed input script, time-ordered.
#[derive(Debug, Clone, Default)]
pub struct Script {
    /// Events sorted by time (stable for equal times).
    pub events: Vec<Event>,
}

impl Script {
    /// Parses script text.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut events = Vec::new();
        for (lineno, raw) in text.lines().enumerate() {
            let line = raw.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let n = lineno + 1;
            let mut it = line.split_whitespace();
            let t_token = it.next().unwrap();
            let t = t_token
                .strip_prefix("t=")
                .ok_or_else(|| format!("line {n}: expected 't=<seconds>', got {t_token:?}"))?
                .parse::<f32>()
                .map_err(|_| format!("line {n}: bad time {t_token:?}"))?;
            if !t.is_finite() || t < 0.0 {
                return Err(format!("line {n}: time must be finite and >= 0"));
            }
            let cmd = it
                .next()
                .ok_or_else(|| format!("line {n}: missing command"))?;
            let num = |it: &mut std::str::SplitWhitespace| -> Result<f32, String> {
                let v = it
                    .next()
                    .ok_or_else(|| format!("line {n}: {cmd} needs a number"))?;
                v.parse::<f32>()
                    .map_err(|_| format!("line {n}: bad number {v:?}"))
            };
            let command = match cmd {
                "forward" => Command::Forward(num(&mut it)?),
                "back" => Command::Forward(-num(&mut it)?),
                "right" => Command::Right(num(&mut it)?),
                "left" => Command::Right(-num(&mut it)?),
                "walk" => {
                    let v = it
                        .next()
                        .ok_or_else(|| format!("line {n}: walk needs a value"))?;
                    Command::Walk(matches!(v, "1" | "on" | "true" | "yes"))
                }
                "crouch" | "duck" => {
                    let v = it
                        .next()
                        .ok_or_else(|| format!("line {n}: crouch needs a value"))?;
                    Command::Crouch(matches!(v, "1" | "on" | "true" | "yes"))
                }
                "jump" => Command::Jump,
                "yaw" => Command::Yaw(num(&mut it)?),
                "turn" => Command::Turn(num(&mut it)?),
                "pitch" => Command::Pitch(num(&mut it)?),
                "track" => match it.next() {
                    Some("off") => Command::Track(None),
                    Some(actor) => {
                        let height = it.next().and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.6);
                        Command::Track(Some((actor.to_owned(), height)))
                    }
                    None => return Err(format!("line {n}: track needs an actor name or off")),
                },
                "follow" => match it.next() {
                    Some("off") => Command::Follow(None),
                    Some(actor) => {
                        let distance = it
                            .next()
                            .and_then(|v| v.parse::<f32>().ok())
                            .unwrap_or(200.0);
                        Command::Follow(Some((actor.to_owned(), distance)))
                    }
                    None => return Err(format!("line {n}: follow needs an actor name or off")),
                },
                "teleport" | "place" => {
                    let x = num(&mut it)?;
                    let y = num(&mut it)?;
                    let z = num(&mut it)?;
                    Command::Teleport([x, y, z])
                }
                "goto" => {
                    if it.clone().next() == Some("off") {
                        it.next();
                        if it.next().is_some() {
                            return Err(format!("line {n}: unexpected goto off argument"));
                        }
                        Command::StopGoto
                    } else {
                        let x = num(&mut it)?;
                        let y = num(&mut it)?;
                        // The waypoint Z is optional; 0 is fine for the horizontal autopilot.
                        let z = it.next().and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
                        Command::Goto([x, y, z])
                    }
                }
                "use" | "grab" | "interact" => match it.next() {
                    Some(target) => Command::UseNamed(target.to_owned()),
                    None => Command::Use,
                },
                "fire" | "shoot" => Command::Fire,
                "fire_hold" | "rope_up" => {
                    let v = it
                        .next()
                        .ok_or_else(|| format!("line {n}: fire_hold needs 0 or 1"))?;
                    Command::FireHold(matches!(v, "1" | "on" | "true" | "yes"))
                }
                "altfire_hold" | "alt_fire_hold" | "rope_down" => {
                    let v = it
                        .next()
                        .ok_or_else(|| format!("line {n}: altfire_hold needs 0 or 1"))?;
                    Command::AltFireHold(matches!(v, "1" | "on" | "true" | "yes"))
                }
                "item" => {
                    let path = it
                        .next()
                        .ok_or_else(|| format!("line {n}: item needs a Package.Class"))?;
                    if !path.contains('.') {
                        return Err(format!("line {n}: item {path:?} must be Package.Class"));
                    }
                    Command::Item(path.to_owned())
                }
                "next_item" => Command::NextItem,
                "reload" => Command::Reload,
                "grab_input" => Command::GrabInput,
                "weapon" | "grant" => {
                    let path = it
                        .next()
                        .ok_or_else(|| format!("line {n}: weapon needs a Package.Class"))?;
                    if !path.contains('.') {
                        return Err(format!("line {n}: weapon {path:?} must be Package.Class"));
                    }
                    Command::Weapon(path.to_owned())
                }
                "wait_travel" | "wait-travel" => Command::WaitTravel,
                "set_goal" | "goal" => {
                    let n = it
                        .next()
                        .ok_or_else(|| format!("line {n}: set_goal needs an objective number"))?
                        .parse::<i32>()
                        .map_err(|_| format!("line {n}: bad objective number"))?;
                    Command::SetGoal(n)
                }
                "switch_weapon" => {
                    let group = it
                        .next()
                        .ok_or_else(|| format!("line {n}: switch_weapon needs a byte"))?
                        .parse::<u8>()
                        .map_err(|_| format!("line {n}: invalid weapon group"))?;
                    if it.next().is_some() {
                        return Err(format!("line {n}: unexpected switch_weapon argument"));
                    }
                    Command::SwitchWeapon(group)
                }
                "select_weapon" => {
                    let group = it
                        .next()
                        .ok_or_else(|| format!("line {n}: select_weapon needs a byte"))?
                        .parse::<u8>()
                        .map_err(|_| format!("line {n}: invalid weapon group"))?;
                    if it.next().is_some() {
                        return Err(format!("line {n}: unexpected select_weapon argument"));
                    }
                    Command::SelectWeapon(group)
                }
                "next_weapon" => {
                    if it.next().is_some() {
                        return Err(format!("line {n}: next_weapon takes no arguments"));
                    }
                    Command::NextWeapon
                }
                "take_control" | "take-control" | "assume_control" => Command::TakeControl,
                "heal" | "quick_heal" | "medkit" => {
                    if it.next().is_some() {
                        return Err(format!("line {n}: {cmd} takes no arguments"));
                    }
                    Command::QuickHeal
                }
                "menu" => {
                    if it.next().is_some() {
                        return Err(format!("line {n}: menu takes no arguments"));
                    }
                    Command::Menu
                }
                "menu_close" | "close_menu" => {
                    if it.next().is_some() {
                        return Err(format!("line {n}: {cmd} takes no arguments"));
                    }
                    Command::MenuClose
                }
                "menu_key" => {
                    let key = it
                        .next()
                        .ok_or_else(|| format!("line {n}: menu_key needs a byte"))?
                        .parse::<u8>()
                        .map_err(|_| format!("line {n}: invalid menu key"))?;
                    if it.next().is_some() {
                        return Err(format!("line {n}: unexpected menu_key argument"));
                    }
                    Command::MenuKey(key)
                }
                "shot" | "screenshot" => {
                    let path = it
                        .next()
                        .ok_or_else(|| format!("line {n}: shot needs a path"))?;
                    if it.next().is_some() {
                        return Err(format!("line {n}: unexpected shot argument"));
                    }
                    Command::Shot(path.to_owned())
                }
                "engage" => Command::Engage(parse_engage(&mut it, n)?),
                "engage_on" | "engage-on" => {
                    let spec = parse_engage(&mut it, n)?;
                    if spec.target.is_some() {
                        return Err(format!(
                            "line {n}: engage_on engages visible hostiles; name a target with a blocking `engage`"
                        ));
                    }
                    Command::EngageOn(spec)
                }
                "engage_off" | "engage-off" => {
                    if it.next().is_some() {
                        return Err(format!("line {n}: engage_off takes no arguments"));
                    }
                    Command::EngageOff
                }
                "loot" => Command::Loot(parse_loot(&mut it, n)?),
                "wait_until" | "wait-until" => {
                    let (cond, timeout) = parse_condition(&mut it, n, WAIT_SECS)?;
                    Command::WaitUntil { cond, timeout }
                }
                "goto_until" | "goto-until" => {
                    let x = next_number(&mut it, n, "goto_until x")?;
                    let y = next_number(&mut it, n, "goto_until y")?;
                    let (cond, timeout) = parse_condition(&mut it, n, WAIT_SECS)?;
                    Command::GotoUntil {
                        target: [x, y, 0.0],
                        cond,
                        timeout,
                    }
                }
                other => return Err(format!("line {n}: unknown command {other:?}")),
            };
            events.push(Event { t, command });
        }
        events.sort_by(|a, b| a.t.total_cmp(&b.t));
        Ok(Self { events })
    }

    /// Reads and parses a script file.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        Self::parse(&text)
    }

    /// Time of the last event, or 0.
    pub fn last_time(&self) -> f32 {
        self.events.last().map(|e| e.t).unwrap_or(0.0)
    }

    /// Time of the last event plus the timeouts of every blocking command: the longest the script
    /// can take to run when each block waits out its timeout (a run budget for the drivers that
    /// stop at the last event).
    pub fn budget(&self) -> f32 {
        self.last_time()
            + self
                .events
                .iter()
                .map(|e| match &e.command {
                    Command::Engage(spec) => spec.secs,
                    Command::Loot(spec) => spec.secs,
                    Command::WaitUntil { timeout, .. } | Command::GotoUntil { timeout, .. } => {
                        *timeout
                    }
                    _ => 0.0,
                })
                .sum::<f32>()
    }

    /// Whether the script contains a `wait_travel` command (the host then keeps running until the
    /// game requests travel).
    pub fn has_wait_travel(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e.command, Command::WaitTravel))
    }
}

/// Default seconds without a visible hostile after which an engagement that has fired ends.
const ENGAGE_LINGER_SECS: f32 = 1.0;
/// Default seconds between two trigger presses of an engagement, and how long each is held.
const FIRE_PERIOD: f32 = 0.25;
const FIRE_HOLD: f32 = 0.0;
/// Seconds a `Reload` press is given to refill the weapon before the next weapon is tried.
const RELOAD_WAIT_SECS: f32 = 2.5;
/// Seconds between two weapon switches while every weapon tried is empty, and how many are tried.
const SWAP_WAIT_SECS: f32 = 1.5;
const MAX_SWAPS: u32 = 6;
/// Seconds a freshly selected weapon gets to come up before it is judged.
const WEAPON_SETTLE_SECS: f32 = 1.0;
/// Trigger presses, over at least [`INEFFECTIVE_SECS`], on one target without its health falling
/// after which the engagement gives up on it for a while and turns to the next visible hostile
/// (the shots go into cover the sight line does not show, as a player notices that the health
/// bar stays full). The time floor keeps a fast automatic weapon from judging a target after a
/// fraction of a second.
const INEFFECTIVE_PRESSES: u32 = 8;
const INEFFECTIVE_SECS: f32 = 2.5;
/// Seconds a target that shots do not hurt is left alone while another one is visible.
const IGNORE_SECS: f32 = 6.0;

/// The active blocking command.
struct Block {
    kind: BlockKind,
    /// Elapsed time at which the block began.
    start: f32,
    /// Elapsed time at which it expires.
    deadline: f32,
    /// Travel count when the block began (`travel` condition).
    travel_baseline: u32,
}

enum BlockKind {
    /// Condition; the flag also ends the block on arrival (`goto_until`).
    Wait(Condition, bool),
    Engage(EngageState),
    Loot(LootState),
}

struct EngageState {
    spec: EngageSpec,
    /// A target has been engaged at least once.
    acquired: bool,
    /// Elapsed time since which no target has been visible.
    lost_since: Option<f32>,
    /// The currently engaged target's name.
    target: Option<String>,
    /// Elapsed time of the last trigger press.
    last_press: f32,
    /// Elapsed time of the `Reload` press of the current dry spell (`None` while the weapon has
    /// ammo).
    last_reload: Option<f32>,
    /// Weapon switches pressed so far, and when the last one was.
    swaps: u32,
    last_swap: f32,
    /// The engagement ended because nothing carried could fire.
    out_of_ammo: bool,
    /// The last weapon state that was noted in the log.
    noted_weapon: Option<WeaponFact>,
    /// Trigger presses on the current target since its health last fell, and the health it had.
    presses: u32,
    hp0: Option<f32>,
    /// Elapsed time at which the count above started.
    since: f32,
    /// Targets given up on (name, until): shots that do not hurt them (see [`INEFFECTIVE_PRESSES`]).
    ignored: Vec<(String, f32)>,
}

/// The nearest hostile within `radius`, optionally leaving out the targets given up on.
fn nearest_hostile<'a>(
    hostiles: &'a [Seen],
    radius: f32,
    ignored: &[(String, f32)],
    skip_ignored: bool,
) -> Option<&'a Seen> {
    hostiles
        .iter()
        .filter(|s| s.distance <= radius)
        .filter(|s| !skip_ignored || !ignored.iter().any(|(n, _)| *n == s.name))
        .min_by(|a, b| a.distance.total_cmp(&b.distance))
}

impl EngageState {
    fn new(spec: EngageSpec) -> Self {
        Self {
            spec,
            acquired: false,
            lost_since: None,
            target: None,
            last_press: f32::NEG_INFINITY,
            last_reload: None,
            swaps: 0,
            last_swap: f32::NEG_INFINITY,
            out_of_ammo: false,
            noted_weapon: None,
            presses: 0,
            hp0: None,
            since: 0.0,
            ignored: Vec::new(),
        }
    }
}

struct LootState {
    spec: LootSpec,
    /// The corpse being walked to.
    target: Option<String>,
    /// Corpses already walked over (never revisited).
    looted: HashSet<String>,
    /// Where the target corpse was last seen.
    last_aim: [f32; 3],
}

impl Cond {
    fn collect(&self, req: &mut FactRequest, aim: f32) {
        match self {
            Cond::Dead(a) | Cond::Alive(a) | Cond::Visible(a) => {
                if !req.actors.iter().any(|x| x.eq_ignore_ascii_case(a)) {
                    req.actors.push(a.clone());
                }
            }
            Cond::HostileVisible(r) => {
                req.hostile_radius = Some(req.hostile_radius.map_or(*r, |c| c.max(*r)));
                req.aim = aim;
            }
            Cond::Goal(_) => req.goals = true,
            Cond::Not(c) => c.collect(req, aim),
            Cond::Event { .. }
            | Cond::Travel
            | Cond::HealthBelow(_)
            | Cond::HealthAbove(_)
            | Cond::Arrived => {}
        }
    }

    fn events<'a>(&'a self, out: &mut Vec<(&'a str, &'a str)>) {
        match self {
            Cond::Event { target, suffix } => out.push((target, suffix)),
            Cond::Not(c) => c.events(out),
            _ => {}
        }
    }
}

impl Facts {
    fn actor(&self, name: &str) -> Option<&ActorFact> {
        self.actors
            .iter()
            .find(|a| a.name.eq_ignore_ascii_case(name))
    }
}

/// Mutable playback state of a [`Script`].
pub struct Drive {
    events: Vec<Event>,
    cursor: usize,
    forward: f32,
    right: f32,
    walk: bool,
    crouch: bool,
    jump_pending: bool,
    use_pending: bool,
    fire_pending: bool,
    weapon_inputs: Vec<WeaponInput>,
    /// Weapons requested (`weapon <Package.Class>`) and not yet applied by the host.
    weapons: Vec<String>,
    /// item64: held fire/alt-fire buttons (`fire_hold`/`altfire_hold`) and whether this tick's
    /// apply is a rising edge.
    fire_hold: bool,
    altfire_hold: bool,
    fire_hold_edge: bool,
    altfire_hold_edge: bool,
    /// item64: items requested (`item <Package.Class>`) and not yet applied by the host.
    items: Vec<String>,
    /// item64: `next_item` requested (edge-triggered) and not yet applied by the host.
    next_item_pending: bool,
    /// Named `use <ActorName>` targets not yet applied by the host.
    use_named: Vec<String>,
    /// Active `goto` waypoint (Unreal units), if any.
    goto: Option<[f32; 3]>,
    /// Set by `wait_travel`; blocks further events until the host calls [`Drive::notify_travel`].
    waiting_travel: bool,
    /// Objective numbers requested (`set_goal <N>`) and not yet applied by the host.
    goals: Vec<i32>,
    /// `take_control` requested (edge-triggered) and not yet applied by the host.
    control_pending: bool,
    /// `heal` (QuickHeal exec) requested (edge-triggered) and not yet applied by the host.
    heal_pending: bool,
    /// item72: `menu` requested (edge-triggered) and not yet applied by the host.
    menu_pending: bool,
    /// item72: `menu_close` requests, in order.
    menu_close_pending: usize,
    /// item82: queued key events for the active retail page.
    menu_keys: Vec<u8>,
    /// item72: `shot <path>` requests, in order.
    shots: Vec<String>,
    exec_inputs: Vec<&'static str>,
    tracking: Option<String>,
    /// Aim height fraction for the active tracking (0.6 default, `track <actor> <f>` sets it).
    track_height: f32,
    track_location: Option<[f32; 3]>,
    /// Active `follow <actor> [distance]` target and stop distance (Unreal units).
    follow: Option<(String, f32)>,
    /// item107: seconds the script clock lags the elapsed time (the total time spent blocked).
    shift: f32,
    /// item107: the active blocking command, if any.
    block: Option<Block>,
    /// item107: this tick's host facts (cleared at the end of every `advance`).
    facts: Facts,
    /// item107: the corpse `loot` is walking to this tick (Unreal units).
    loot_target: Option<[f32; 3]>,
    /// item107: aim point of the engaged target for this tick.
    engage_aim: Option<[f32; 3]>,
    /// item107: the standing engagement (`engage_on`), stepped every tick while no blocking
    /// command runs.
    standing: Option<EngageState>,
    /// item107: the fire button is held by `engage` (not by `fire_hold`).
    engage_holds: bool,
    /// item107: trace index already scanned for `event` conditions, and the events found.
    event_cursor: usize,
    fired_events: HashSet<(String, String)>,
    /// item107: the `event` condition needs the VM trace but the host has it disabled.
    event_warned: bool,
    /// item107: the host's running count of level travels.
    travel_count: u32,
}

impl Drive {
    /// Creates a drive over a parsed script.
    pub fn new(script: &Script) -> Self {
        Self {
            events: script.events.clone(),
            cursor: 0,
            forward: 0.0,
            right: 0.0,
            walk: false,
            crouch: false,
            jump_pending: false,
            use_pending: false,
            fire_pending: false,
            weapon_inputs: Vec::new(),
            weapons: Vec::new(),
            fire_hold: false,
            altfire_hold: false,
            fire_hold_edge: false,
            altfire_hold_edge: false,
            items: Vec::new(),
            next_item_pending: false,
            use_named: Vec::new(),
            goto: None,
            waiting_travel: false,
            goals: Vec::new(),
            control_pending: false,
            heal_pending: false,
            menu_pending: false,
            menu_close_pending: 0,
            menu_keys: Vec::new(),
            shots: Vec::new(),
            exec_inputs: Vec::new(),
            tracking: None,
            track_height: 0.6,
            track_location: None,
            follow: None,
            shift: 0.0,
            block: None,
            facts: Facts::default(),
            engage_aim: None,
            standing: None,
            loot_target: None,
            engage_holds: false,
            event_cursor: 0,
            fired_events: HashSet::new(),
            event_warned: false,
            travel_count: 0,
        }
    }

    /// Drains the `weapon` commands due so far (the host grants them through the VM). Kept out of
    /// [`Input`] because it is not a per-tick axis and carries a class path.
    pub fn take_weapons(&mut self) -> Vec<String> {
        std::mem::take(&mut self.weapons)
    }

    /// item64: the held rope buttons after applying this tick's events, plus whether the fire
    /// (alt-fire) button rose this tick (the exec dispatch edge).
    pub fn take_hook_buttons(&mut self) -> ((bool, bool), (bool, bool)) {
        let fire = (self.fire_hold, std::mem::take(&mut self.fire_hold_edge));
        let altfire = (
            self.altfire_hold,
            std::mem::take(&mut self.altfire_hold_edge),
        );
        (fire, altfire)
    }

    /// Drains the `item` commands due so far (the host grants + selects them through the VM).
    pub fn take_items(&mut self) -> Vec<String> {
        std::mem::take(&mut self.items)
    }

    /// Takes the pending `next_item` request (edge-triggered).
    pub fn take_next_item(&mut self) -> bool {
        std::mem::take(&mut self.next_item_pending)
    }

    /// Drains the `set_goal` objective numbers due so far (the host calls the map's own
    /// `MapInfo.SetGoalComplete`).
    pub fn take_goals(&mut self) -> Vec<i32> {
        std::mem::take(&mut self.goals)
    }

    /// Drains the named `use <ActorName>` targets due so far.
    pub fn take_use_named(&mut self) -> Vec<String> {
        std::mem::take(&mut self.use_named)
    }

    /// Takes the pending `take_control` request (edge-triggered).
    pub fn take_control(&mut self) -> bool {
        std::mem::take(&mut self.control_pending)
    }

    /// Takes the pending `heal` (QuickHeal) request (edge-triggered).
    pub fn take_quick_heal(&mut self) -> bool {
        std::mem::take(&mut self.heal_pending)
    }

    /// Takes the pending `menu` request (edge-triggered).
    pub fn take_menu_open(&mut self) -> bool {
        std::mem::take(&mut self.menu_pending)
    }

    /// Takes the pending `menu_close` requests, in order.
    pub fn take_menu_close(&mut self) -> bool {
        let n = &mut self.menu_close_pending;
        if *n > 0 {
            *n -= 1;
            true
        } else {
            false
        }
    }

    /// Drains menu key presses in script order.
    pub fn take_menu_keys(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.menu_keys)
    }

    /// Drains the pending `shot <path>` requests, in order.
    pub fn take_shots(&mut self) -> Vec<String> {
        std::mem::take(&mut self.shots)
    }

    /// Drains ordinary player exec edges (Reload and Grab), preserving input order.
    pub fn take_exec_inputs(&mut self) -> Vec<&'static str> {
        std::mem::take(&mut self.exec_inputs)
    }

    /// Supplies the current actor location to the active tracking command.
    pub fn set_track_location(&mut self, actor: Option<&str>, location: Option<[f32; 3]>) {
        self.track_location = match (self.tracking.as_deref(), actor) {
            (Some(wanted), Some(found)) if wanted.eq_ignore_ascii_case(found) => location,
            _ => None,
        };
    }

    pub fn tracking_actor(&self) -> Option<&str> {
        self.tracking.as_deref()
    }

    /// Aim height fraction for the active tracking (fraction of the pawn's CollisionHeight).
    pub fn track_height(&self) -> f32 {
        self.track_height
    }

    /// Whether the driver is blocked on a `wait_travel` command.
    pub fn waiting_travel(&self) -> bool {
        self.waiting_travel
    }

    /// Releases a `wait_travel` block (the host observed the travel request).
    pub fn notify_travel(&mut self) {
        self.waiting_travel = false;
    }

    /// Drains normal weapon-selection inputs, preserving repeated commands in order.
    pub fn take_weapon_inputs(&mut self) -> Vec<WeaponInput> {
        std::mem::take(&mut self.weapon_inputs)
    }

    /// item107: whether a blocking command is active; the host then gathers [`Facts`] each tick.
    pub fn wants_facts(&self) -> bool {
        self.block.is_some() || self.standing.is_some()
    }

    /// item107: what the active blocking command needs gathered this tick.
    pub fn fact_request(&self) -> FactRequest {
        let mut req = FactRequest {
            aim: ENGAGE_AIM,
            ..FactRequest::default()
        };
        match self.block.as_ref().map(|b| &b.kind) {
            Some(BlockKind::Wait(cond, _)) => {
                for term in &cond.terms {
                    term.collect(&mut req, ENGAGE_AIM);
                }
            }
            Some(BlockKind::Engage(e)) => {
                req.weapon = true;
                req.aim = e.spec.aim;
                match &e.spec.target {
                    Some(name) => req.actors.push(name.clone()),
                    None => req.hostile_radius = Some(e.spec.radius),
                }
            }
            Some(BlockKind::Loot(l)) => req.corpse_radius = Some(l.spec.radius),
            None => {}
        }
        if let Some(st) = &self.standing
            && self
                .block
                .as_ref()
                .is_none_or(|b| matches!(b.kind, BlockKind::Wait(..)))
        {
            req.weapon = true;
            req.aim = st.spec.aim;
            req.hostile_radius = Some(
                req.hostile_radius
                    .map_or(st.spec.radius, |r| r.max(st.spec.radius)),
            );
        }
        req
    }

    /// item107: supplies this tick's host facts (valid until the end of the next [`Drive::advance`]).
    pub fn set_facts(&mut self, facts: Facts) {
        self.facts = facts;
    }

    /// item107: the host's running count of level travels requested (every tick, whatever the
    /// block), so a `travel` condition counts from the block's start.
    pub fn set_travel_count(&mut self, count: u32) {
        self.travel_count = count;
    }

    /// item107: the VM session changed (a level travel): forget the scanned trace.
    pub fn session_changed(&mut self) {
        self.event_cursor = 0;
        self.fired_events.clear();
    }

    /// item107: scans the VM trace for the active block's `event` conditions. `enabled` is the
    /// VM's `trace_enabled`: without the trace an `event` condition cannot hold, which is said
    /// once rather than silently treated as unmet.
    pub fn observe_trace(&mut self, trace: &[TraceEvent], enabled: bool) {
        let mut wanted: Vec<(&str, &str)> = Vec::new();
        if let Some(Block {
            kind: BlockKind::Wait(cond, _),
            ..
        }) = &self.block
        {
            for term in &cond.terms {
                term.events(&mut wanted);
            }
        }
        if wanted.is_empty() {
            return;
        }
        if !enabled {
            if !self.event_warned {
                self.event_warned = true;
                println!(
                    "[play] wait_until event: the VM trace is disabled here, so the condition cannot hold"
                );
            }
            return;
        }
        let wanted: Vec<(String, String)> = wanted
            .into_iter()
            .map(|(t, s)| (t.to_ascii_lowercase(), s.to_ascii_lowercase()))
            .collect();
        let start = self.event_cursor.min(trace.len());
        for event in &trace[start..] {
            if let TraceKind::Event {
                target, function, ..
            } = &event.kind
            {
                let (t, f) = (target.to_ascii_lowercase(), function.to_ascii_lowercase());
                if wanted.iter().any(|(wt, ws)| *wt == t && f.ends_with(ws)) {
                    self.fired_events.insert((t, f));
                }
            }
        }
        self.event_cursor = trace.len();
    }

    /// item107: starts a blocking command at `elapsed`.
    fn begin_block(&mut self, command: Command, elapsed: f32) {
        let (kind, timeout) = match command {
            Command::Engage(spec) => {
                let secs = spec.secs;
                (BlockKind::Engage(EngageState::new(spec)), secs)
            }
            Command::Loot(spec) => {
                let secs = spec.secs;
                (
                    BlockKind::Loot(LootState {
                        spec,
                        target: None,
                        looted: HashSet::new(),
                        last_aim: [0.0; 3],
                    }),
                    secs,
                )
            }
            Command::WaitUntil { cond, timeout } => (BlockKind::Wait(cond, false), timeout),
            Command::GotoUntil {
                target,
                cond,
                timeout,
            } => {
                self.goto = Some(target);
                (BlockKind::Wait(cond, true), timeout)
            }
            _ => return,
        };
        println!(
            "[play] block begin [{elapsed:.3}s] {} (timeout {timeout:.1}s)",
            match &kind {
                BlockKind::Wait(..) => "wait_until/goto_until",
                BlockKind::Engage(_) => "engage",
                BlockKind::Loot(_) => "loot",
            }
        );
        // A new block rescans the whole trace so an event that already fired still counts.
        self.event_cursor = 0;
        self.fired_events.clear();
        self.block = Some(Block {
            kind,
            start: elapsed,
            deadline: elapsed + timeout,
            travel_baseline: self.travel_count,
        });
    }

    fn eval_cond(&self, cond: &Cond, travel_baseline: u32) -> bool {
        let facts = &self.facts;
        match cond {
            Cond::Dead(a) => facts.actor(a).is_some_and(|f| f.state == ActorState::Dead),
            Cond::Alive(a) => facts.actor(a).is_some_and(|f| f.state == ActorState::Alive),
            Cond::Visible(a) => facts
                .actor(a)
                .is_some_and(|f| f.state == ActorState::Alive && f.visible),
            Cond::HostileVisible(r) => facts.hostiles.iter().any(|s| s.distance <= *r),
            Cond::Goal(n) => usize::try_from(*n)
                .ok()
                .and_then(|i| facts.goals.get(i))
                .copied()
                .unwrap_or(false),
            Cond::Event { target, suffix } => {
                let (t, s) = (target.to_ascii_lowercase(), suffix.to_ascii_lowercase());
                self.fired_events
                    .iter()
                    .any(|(ft, ff)| *ft == t && ff.ends_with(&s))
            }
            Cond::Travel => self.travel_count > travel_baseline,
            Cond::HealthBelow(n) => facts.player_health.is_some_and(|h| h < *n),
            Cond::HealthAbove(n) => facts.player_health.is_some_and(|h| h > *n),
            Cond::Arrived => self.goto.is_none(),
            Cond::Not(c) => !self.eval_cond(c, travel_baseline),
        }
    }

    /// Presses the fire button for `engage` (rising edge: the host dispatches the `Fire` exec).
    fn engage_press(&mut self) {
        self.fire_hold_edge = !self.fire_hold;
        self.fire_hold = true;
        self.engage_holds = true;
    }

    /// Releases a fire button that `engage` holds (the `onrelease UnFire` exec).
    fn engage_release(&mut self) {
        if self.engage_holds {
            if self.fire_hold {
                self.exec_inputs.push("UnFire");
            }
            self.fire_hold = false;
            self.engage_holds = false;
        }
    }

    /// One tick of the engage fire logic against a visible target. Returns `true` when the
    /// engagement cannot go on: the weapon is out of ammo, `Reload` did not refill it and no other
    /// carried weapon has any.
    fn engage_fire(&mut self, e: &mut EngageState, elapsed: f32) -> bool {
        let weapon = self.facts.weapon.clone().unwrap_or_default();
        if e.noted_weapon.as_ref() != Some(&weapon) {
            println!("[play] engage [{elapsed:.3}s] weapon {weapon:?}");
            e.noted_weapon = Some(weapon.clone());
        }
        // Fists are no use at range while a gun is carried.
        let punching = weapon.melee && !weapon.alternatives.is_empty();
        if weapon.clip == Some(0) || punching {
            // Out of loaded ammo: let go of the trigger, press Reload, and when that does not
            // refill the weapon in time switch to a carried weapon that has ammo, as a player does.
            self.engage_release();
            // A freshly selected weapon gets a second to come up before it is judged.
            if elapsed - e.last_swap < WEAPON_SETTLE_SECS {
                return false;
            }
            if !punching && e.last_reload.is_none() {
                self.exec_inputs.push("Reload");
                e.last_reload = Some(elapsed);
                return false;
            }
            let waited = punching
                || e.last_reload
                    .is_some_and(|t| elapsed - t >= RELOAD_WAIT_SECS);
            if waited {
                match weapon.alternatives.first() {
                    Some(&group)
                        if e.swaps < MAX_SWAPS && elapsed - e.last_swap >= SWAP_WAIT_SECS =>
                    {
                        self.weapon_inputs.push(WeaponInput::Switch(group));
                        e.swaps += 1;
                        e.last_swap = elapsed;
                        // The new weapon gets its own reload attempt if it is empty too.
                        e.last_reload = None;
                    }
                    Some(_) if e.swaps < MAX_SWAPS => {}
                    _ => {
                        e.out_of_ammo = true;
                        return true;
                    }
                }
            }
            return false;
        }
        e.last_reload = None;
        // Trigger discipline: press, hold for `hold` seconds, let go, press again every `period`
        // seconds. The default is a tap (the button is down for one tick) every 0.25 s: the cadence
        // of the recorded routes, and the way to keep an automatic weapon accurate (each release
        // dispatches `UnFire`, which ends the burst; a held automatic weapon sprays).
        let (hold, period) = (e.spec.hold, e.spec.period);
        if self.engage_holds {
            if elapsed - e.last_press > hold {
                self.engage_release();
            }
        } else if !self.fire_hold && elapsed - e.last_press >= period {
            self.engage_press();
            e.last_press = elapsed;
            e.presses += 1;
        }
        false
    }

    /// One tick of an engagement; returns whether it is over.
    fn engage_step(&mut self, e: &mut EngageState, elapsed: f32) -> bool {
        if self.facts.suppressed {
            self.engage_release();
            self.engage_aim = None;
            return false;
        }
        let radius = e.spec.radius;
        let chosen: Option<(String, [f32; 3])> = match &e.spec.target {
            Some(name) => {
                let fact = self.facts.actor(name);
                if fact.is_some_and(|f| f.state == ActorState::Dead) {
                    return true;
                }
                fact.filter(|f| f.state == ActorState::Alive && f.visible && f.distance <= radius)
                    .and_then(|f| f.aim.map(|aim| (name.clone(), aim)))
            }
            None => {
                e.ignored.retain(|(_, until)| *until > elapsed);
                let mut current = e.target.as_deref().and_then(|t| {
                    self.facts.hostiles.iter().find(|s| {
                        s.name == t
                            && s.distance <= radius
                            && !e.ignored.iter().any(|(n, _)| *n == s.name)
                    })
                });
                // Shots that never hurt the target: give up on it for a while.
                if let Some(c) = current {
                    match (c.health, e.hp0) {
                        (Some(h), Some(h0)) if h < h0 - 0.5 => {
                            e.hp0 = Some(h);
                            e.presses = 0;
                            e.since = elapsed;
                        }
                        _ => {}
                    }
                    if c.health.is_some()
                        && e.presses >= INEFFECTIVE_PRESSES
                        && elapsed - e.since >= INEFFECTIVE_SECS
                    {
                        println!(
                            "[play] engage [{elapsed:.3}s] target {} ignored for {IGNORE_SECS:.0}s: {} shots, health unchanged",
                            c.name, e.presses
                        );
                        e.ignored.push((c.name.clone(), elapsed + IGNORE_SECS));
                        e.presses = 0;
                        e.since = elapsed;
                        current = None;
                    }
                }
                current
                    .or_else(|| nearest_hostile(&self.facts.hostiles, radius, &e.ignored, true))
                    // Nothing else to shoot at: keep trying the ignored one (a long-range
                    // automatic burst often needs seconds to land its first hits).
                    .or_else(|| nearest_hostile(&self.facts.hostiles, radius, &e.ignored, false))
                    .map(|s| {
                        if e.target.as_deref() != Some(s.name.as_str()) {
                            e.presses = 0;
                            e.hp0 = s.health;
                            e.since = elapsed;
                        }
                        (s.name.clone(), s.aim)
                    })
            }
        };
        match chosen {
            Some((name, aim)) => {
                e.acquired = true;
                e.lost_since = None;
                if e.target.as_deref() != Some(name.as_str()) {
                    println!(
                        "[play] engage [{elapsed:.3}s] target {name} aim=({:.0},{:.0},{:.0})",
                        aim[0], aim[1], aim[2]
                    );
                }
                e.target = Some(name);
                self.engage_aim = Some(aim);
                if self.engage_fire(e, elapsed) {
                    // Nothing carried can fire: stop holding the aim so a pending `goto` walks on
                    // (a standing engagement must not pin the player to a spot with an empty gun).
                    self.engage_aim = None;
                    self.engage_release();
                    return true;
                }
                false
            }
            None => {
                self.engage_aim = None;
                self.engage_release();
                e.target = None;
                if e.acquired {
                    let since = *e.lost_since.get_or_insert(elapsed);
                    elapsed - since >= e.spec.linger
                } else {
                    false
                }
            }
        }
    }

    /// One tick of `loot`; returns whether it is over (no unvisited lootable corpse is visible).
    fn loot_step(&mut self, l: &mut LootState, location: [f32; 3]) -> bool {
        if self.facts.suppressed {
            self.loot_target = None;
            return false;
        }
        let corpse = |name: &str| self.facts.corpses.iter().find(|c| c.name == name);
        let horizontal = |p: [f32; 3]| (p[0] - location[0]).hypot(p[1] - location[1]);
        if let Some(name) = l.target.clone() {
            match corpse(&name) {
                // Walked over it: whatever the game hands over on contact has been handed.
                Some(c) if horizontal(c.aim) <= LOOT_REACH => {
                    l.looted.insert(name);
                    l.target = None;
                }
                Some(_) => {}
                None => {
                    println!(
                        "[play] loot [{name} has no weapon left to take: {:.0} UU horizontal when it went]",
                        horizontal(l.last_aim)
                    );
                    l.looted.insert(name);
                    l.target = None;
                }
            }
        }
        if l.target.is_none() {
            l.target = self
                .facts
                .corpses
                .iter()
                .filter(|c| c.visible && !l.looted.contains(&c.name) && c.distance <= l.spec.radius)
                .min_by(|a, b| a.distance.total_cmp(&b.distance))
                .map(|c| c.name.clone());
            if let Some(name) = &l.target {
                println!("[play] loot [target {name}]");
            }
        }
        self.loot_target = l.target.as_deref().and_then(corpse).map(|c| c.aim);
        if let Some(aim) = self.loot_target {
            l.last_aim = aim;
        }
        l.target.is_none()
    }

    /// item107: advances the active blocking command by one tick and ends it when it is done or
    /// out of time, shifting the script clock so later events keep their relative timing.
    fn progress_block(&mut self, elapsed: f32, location: [f32; 3]) {
        let Some(mut block) = self.block.take() else {
            return;
        };
        let done = match &mut block.kind {
            BlockKind::Wait(cond, or_arrived) => {
                let hit = |s: &Self, t: &Cond| s.eval_cond(t, block.travel_baseline);
                let met = if cond.all {
                    cond.terms.iter().all(|t| hit(self, t))
                } else {
                    cond.terms.iter().any(|t| hit(self, t))
                };
                met || (*or_arrived && self.goto.is_none())
            }
            BlockKind::Engage(e) => self.engage_step(e, elapsed),
            BlockKind::Loot(l) => self.loot_step(l, location),
        };
        let timed_out = elapsed >= block.deadline;
        if !done && !timed_out {
            self.block = Some(block);
            return;
        }
        let waited = elapsed - block.start;
        match &block.kind {
            BlockKind::Wait(cond, _) if !done => println!(
                "[play] wait_until [{elapsed:.3}s] timed out after {waited:.1}s (condition not met): {cond:?}"
            ),
            BlockKind::Wait(..) => {}
            BlockKind::Loot(l) => println!(
                "[play] loot [{elapsed:.3}s] {} after {waited:.1}s ({} corpse(s) done)",
                if done { "ended" } else { "timed out" },
                l.looted.len()
            ),
            BlockKind::Engage(e) => println!(
                "[play] engage [{elapsed:.3}s] {} after {waited:.1}s (target {:?}, engaged {}{})",
                if done { "ended" } else { "timed out" },
                e.spec.target,
                e.acquired,
                if e.out_of_ammo { ", OUT OF AMMO" } else { "" }
            ),
        }
        self.engage_release();
        self.engage_aim = None;
        if matches!(block.kind, BlockKind::Loot(_)) {
            // Stop the walk `loot` was driving; a pending `goto` re-asserts its own forward.
            self.loot_target = None;
            self.forward = 0.0;
        }
        self.shift += waited;
    }

    /// Applies every event due at or before `elapsed` and returns this tick's input.
    ///
    /// Yaw/pitch commands are applied directly to `sim` (they are orientation, not an axis).
    pub fn advance(&mut self, elapsed: f32, sim: &mut PlayerSim) -> Input {
        if self.waiting_travel {
            return Input::default();
        }
        self.progress_block(elapsed, sim.location);
        // The script clock stops while a blocking command runs (`shift` is the time spent blocked).
        let clock = elapsed - self.shift;
        while self.block.is_none()
            && self.cursor < self.events.len()
            && self.events[self.cursor].t <= clock + 1e-6
        {
            if matches!(
                self.events[self.cursor].command,
                Command::Engage(_)
                    | Command::Loot(_)
                    | Command::WaitUntil { .. }
                    | Command::GotoUntil { .. }
            ) {
                let command = self.events[self.cursor].command.clone();
                self.cursor += 1;
                self.begin_block(command, elapsed);
                continue;
            }
            match &self.events[self.cursor].command {
                &Command::Forward(v) => self.forward = v,
                &Command::Right(v) => self.right = v,
                &Command::Walk(v) => self.walk = v,
                &Command::Crouch(v) => self.crouch = v,
                Command::Jump => self.jump_pending = true,
                &Command::Yaw(deg) => sim.yaw = deg.to_radians(),
                &Command::Turn(deg) => sim.yaw += deg.to_radians(),
                &Command::Pitch(deg) => sim.pitch = deg.to_radians(),
                Command::Track(actor_height) => {
                    match actor_height {
                        Some((actor, height)) => {
                            self.tracking = Some(actor.clone());
                            self.track_height = *height;
                        }
                        None => self.tracking = None,
                    }
                    self.track_location = None;
                }
                Command::Follow(actor_distance) => {
                    match actor_distance {
                        Some((actor, distance)) => {
                            self.follow = Some((actor.clone(), *distance));
                            // Resolve the followed actor through the host's tracking-location hook
                            // (the same one `track` uses).
                            self.tracking = Some(actor.clone());
                        }
                        None => self.follow = None,
                    }
                    self.track_location = None;
                }
                &Command::Teleport(p) => {
                    sim.location = p;
                    sim.velocity = [0.0; 3];
                    sim.grounded = false;
                    self.goto = None;
                }
                Command::EngageOn(spec) => self.standing = Some(EngageState::new(spec.clone())),
                Command::EngageOff => {
                    self.standing = None;
                    self.engage_release();
                    self.engage_aim = None;
                }
                &Command::Goto(p) => self.goto = Some(p),
                Command::StopGoto => self.goto = None,
                Command::Use => self.use_pending = true,
                Command::UseNamed(target) => self.use_named.push(target.clone()),
                Command::Fire => self.fire_pending = true,
                Command::FireHold(v) => {
                    self.fire_hold_edge = *v && !self.fire_hold;
                    // item105: DefUser.ini `Fire` = `Button bFire | Fire | onrelease UnFire`:
                    // the release also dispatches the `UnFire` exec (a weapon's own `UnFire`,
                    // e.g. `m60.UnFire` resets `iBurstCount`).
                    if !*v && self.fire_hold {
                        self.exec_inputs.push("UnFire");
                    }
                    self.fire_hold = *v;
                }
                Command::AltFireHold(v) => {
                    self.altfire_hold_edge = *v && !self.altfire_hold;
                    if !*v && self.altfire_hold {
                        self.exec_inputs.push("UnFire");
                    }
                    self.altfire_hold = *v;
                }
                Command::Item(path) => self.items.push(path.clone()),
                Command::NextItem => self.next_item_pending = true,
                Command::Reload => self.exec_inputs.push("Reload"),
                Command::GrabInput => self.exec_inputs.push("Grab"),
                Command::Weapon(path) => self.weapons.push(path.clone()),
                &Command::SetGoal(n) => self.goals.push(n),
                Command::WaitTravel => {
                    // Stop here; the events after `wait_travel` wait for the reload.
                    self.waiting_travel = true;
                    self.cursor += 1;
                    break;
                }
                &Command::SwitchWeapon(group) => {
                    self.weapon_inputs.push(WeaponInput::Switch(group))
                }
                &Command::SelectWeapon(group) => {
                    self.weapon_inputs.push(WeaponInput::Select(group))
                }
                Command::NextWeapon => self.weapon_inputs.push(WeaponInput::Next),
                Command::TakeControl => self.control_pending = true,
                Command::QuickHeal => self.heal_pending = true,
                Command::Menu => self.menu_pending = true,
                Command::MenuClose => self.menu_close_pending += 1,
                &Command::MenuKey(key) => self.menu_keys.push(key),
                Command::Shot(path) => self.shots.push(path.clone()),
                // Handled above: they start a blocking command.
                Command::Engage(_)
                | Command::Loot(_)
                | Command::WaitUntil { .. }
                | Command::GotoUntil { .. } => {}
            }
            self.cursor += 1;
        }
        // The standing engagement runs beside the script clock; a blocking `engage`/`loot` owns
        // the trigger while it lasts.
        if self
            .block
            .as_ref()
            .is_none_or(|b| matches!(b.kind, BlockKind::Wait(..)))
            && let Some(mut st) = self.standing.take()
        {
            if self.engage_step(&mut st, elapsed) {
                // It never ends by itself: forget the lost target and wait for the next one.
                st.acquired = false;
                st.lost_since = None;
            }
            self.standing = Some(st);
        }
        if let Some(target) = self.engage_aim.or(self.track_location) {
            let dx = target[0] - sim.location[0];
            let dy = target[1] - sim.location[1];
            let dz = target[2] - (sim.location[2] + 60.0);
            if dx != 0.0 || dy != 0.0 {
                sim.yaw = dy.atan2(dx).rem_euclid(std::f32::consts::TAU);
                sim.pitch = dz.atan2(dx.hypot(dy));
            }
        }
        // `goto` autopilot: re-aim and hold forward until within reach of the waypoint. The
        // simulation still moves the player; only the yaw/axis are driven.
        if let Some(target) = self.loot_target {
            // `loot`: walk onto the corpse; a pending waypoint resumes afterwards.
            let (dx, dy) = (target[0] - sim.location[0], target[1] - sim.location[1]);
            if dx != 0.0 || dy != 0.0 {
                sim.yaw = dy.atan2(dx).rem_euclid(std::f32::consts::TAU);
            }
            self.forward = 1.0;
        } else if self.engage_aim.is_some() {
            // A target is engaged: stand and shoot; the waypoint resumes afterwards.
            if self.goto.is_some() {
                self.forward = 0.0;
            }
        } else if let Some(target) = self.goto {
            let (dx, dy) = (target[0] - sim.location[0], target[1] - sim.location[1]);
            if dx * dx + dy * dy <= 24.0 * 24.0 {
                self.goto = None;
                self.forward = 0.0;
            } else {
                let mut yaw = dy.atan2(dx);
                if yaw < 0.0 {
                    yaw += std::f32::consts::TAU;
                }
                sim.yaw = yaw;
                self.forward = 1.0;
            }
        }
        // `follow` autopilot: the tracking block above already aims at the actor; hold forward
        // while the horizontal gap exceeds the stop distance, otherwise release it. This trails
        // the actor without ever driving into it (the same input axes a careful player uses).
        if let Some((_, distance)) = &self.follow
            && let Some(target) = self.track_location
        {
            let (dx, dy) = (target[0] - sim.location[0], target[1] - sim.location[1]);
            self.forward = if dx * dx + dy * dy > distance * distance {
                1.0
            } else {
                0.0
            };
        }
        self.facts = Facts::default();
        let jump = std::mem::take(&mut self.jump_pending);
        let use_action = std::mem::take(&mut self.use_pending);
        let fire = std::mem::take(&mut self.fire_pending);
        Input {
            forward: self.forward,
            right: self.right,
            jump,
            walk: self.walk,
            use_action,
            fire,
            crouch: self.crouch,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normal_weapon_input_preserves_order_and_does_not_request_diagnostic_equip() {
        let script = Script::parse(
            "t=1 switch_weapon 2\nt=1 next_weapon\nt=1 select_weapon 14\nt=1 switch_weapon 20\n",
        )
        .unwrap();
        let mut drive = Drive::new(&script);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        drive.advance(0.9, &mut sim);
        assert!(drive.take_weapon_inputs().is_empty());
        drive.advance(1.0, &mut sim);
        assert_eq!(
            drive.take_weapon_inputs(),
            [
                WeaponInput::Switch(2),
                WeaponInput::Next,
                WeaponInput::Select(14),
                WeaponInput::Switch(20)
            ]
        );
        assert!(drive.take_weapon_inputs().is_empty());
        assert!(Script::parse("t=0 equip").is_err());
        assert!(Script::parse("t=0 next_weapon extra").is_err());
        for invalid in ["-1", "256", "NaN", "2 extra", ""] {
            assert!(Script::parse(&format!("t=0 switch_weapon {invalid}")).is_err());
            assert!(Script::parse(&format!("t=0 select_weapon {invalid}")).is_err());
        }
    }

    #[test]
    fn parses_and_applies_events_in_order() {
        let s = Script::parse(
            "# a comment\nt=0.0 forward 1\nt=0.5 yaw 90\nt=1.0 forward 0\nt=1.0 jump\n",
        )
        .unwrap();
        assert_eq!(s.events.len(), 4);
        assert_eq!(s.last_time(), 1.0);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut d = Drive::new(&s);
        let i0 = d.advance(0.0, &mut sim);
        assert_eq!((i0.forward, i0.jump), (1.0, false));
        let i1 = d.advance(0.5, &mut sim);
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert_eq!(i1.forward, 1.0);
        let i2 = d.advance(1.0, &mut sim);
        assert_eq!(i2.forward, 0.0);
        assert!(i2.jump, "jump event is edge-triggered");
        // Jump is consumed: the next tick has no jump.
        let i3 = d.advance(1.02, &mut sim);
        assert!(!i3.jump);
    }

    #[test]
    fn goto_reaims_and_walks_then_stops_at_the_waypoint() {
        let s = Script::parse("t=0.0 goto 100 0\nt=5.0 forward 0\n").unwrap();
        assert_eq!(s.events.len(), 2);
        let mut sim = PlayerSim::new([0.0; 3], std::f32::consts::PI); // facing -X
        let mut d = Drive::new(&s);
        let i0 = d.advance(0.0, &mut sim);
        assert_eq!(i0.forward, 1.0, "goto holds forward");
        assert!(sim.yaw.abs() < 1e-4, "re-aimed toward +X, got {}", sim.yaw);
        // Once within the arrival radius the autopilot releases and clears forward.
        sim.location = [90.0, 0.0, 0.0];
        let i1 = d.advance(0.3, &mut sim);
        assert_eq!(i1.forward, 0.0);
        // A bad/absent coordinate is rejected.
        assert!(Script::parse("t=0.0 goto 1\n").is_err());
    }

    #[test]
    fn cancelling_goto_keeps_manual_axes_and_does_not_restore_steering() {
        let script = Script::parse(
            "t=0 goto 1000 0\nt=1 goto off\nt=1 forward 1\nt=1 yaw 90\nt=1 jump\nt=2 forward 0\n",
        )
        .unwrap();
        let mut drive = Drive::new(&script);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        assert_eq!(drive.advance(0.0, &mut sim).forward, 1.0);
        let before = (sim.location, sim.velocity);
        let input = drive.advance(1.0, &mut sim);
        assert_eq!(
            (sim.location, sim.velocity),
            before,
            "cancelling steering must not place or stop the pawn"
        );
        assert_eq!(input.forward, 1.0);
        assert!(input.jump);
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-6);
        sim.location = [0.0, 100.0, 75.0];
        let input = drive.advance(1.5, &mut sim);
        assert!(!input.jump, "jump remains edge-triggered");
        assert!(
            (sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-6,
            "old waypoint must not re-aim subsequent ticks"
        );
        assert_eq!(drive.advance(2.0, &mut sim).forward, 0.0);
        assert!(Script::parse("t=0 goto off 1").is_err());
    }

    #[test]
    fn rejects_bad_lines() {
        assert!(Script::parse("forward 1\n").is_err());
        assert!(Script::parse("t=x forward 1\n").is_err());
        assert!(Script::parse("t=0.0 fly 1\n").is_err());
        assert!(Script::parse("t=0.0 forward\n").is_err());
        assert!(Script::parse("t=-1.0 forward 1\n").is_err());
    }

    #[test]
    fn wait_travel_blocks_until_notified() {
        let s = Script::parse("t=0.0 forward 1\nt=1.0 wait_travel\nt=2.0 yaw 90\n").unwrap();
        assert!(s.has_wait_travel());
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut d = Drive::new(&s);
        assert_eq!(d.advance(0.0, &mut sim).forward, 1.0);
        // At the wait, input is cleared and the driver is blocked.
        let _ = d.advance(1.0, &mut sim);
        assert!(d.waiting_travel(), "wait_travel must block");
        let i2 = d.advance(1.1, &mut sim);
        assert_eq!(i2.forward, 0.0);
        // Events after the wait do not apply while blocked.
        let _ = d.advance(5.0, &mut sim);
        assert!((sim.yaw - 0.0).abs() < 1e-6, "yaw applied while blocked");
        // Releasing the wait lets the remaining events apply.
        d.notify_travel();
        assert!(!d.waiting_travel());
        let _ = d.advance(5.0, &mut sim);
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    }

    #[test]
    fn parses_named_use_and_take_control_and_rejects_removed_bridges() {
        let s = Script::parse("t=0.0 take_control\nt=0.5 use Porte1\nt=1.5 use\n").unwrap();
        assert_eq!(s.events.len(), 3);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut d = Drive::new(&s);
        let _ = d.advance(0.0, &mut sim);
        assert!(d.take_control(), "take_control is edge-triggered");
        assert!(!d.take_control());
        let _ = d.advance(0.5, &mut sim);
        assert_eq!(d.take_use_named(), vec!["Porte1".to_owned()]);
        // A bare `use` is still the ray-based action.
        let i = d.advance(1.5, &mut sim);
        assert!(i.use_action);
        assert!(d.take_use_named().is_empty());
        for command in [
            "search BaseSoldier6",
            "loot BaseSoldier6",
            "wake BaseSoldier6",
            "wake",
        ] {
            assert!(Script::parse(&format!("t=0.0 {command}\n")).is_err());
        }
    }

    #[test]
    fn empty_script_is_valid() {
        let s = Script::parse("\n# nothing\n").unwrap();
        assert!(s.events.is_empty());
        assert_eq!(s.last_time(), 0.0);
    }

    #[test]
    fn menu_key_queue_preserves_order_and_rejects_invalid_bytes() {
        let script = Script::parse("t=0 menu_key 13\nt=0 menu_key 27\n").unwrap();
        let mut drive = Drive::new(&script);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        drive.advance(0.0, &mut sim);
        assert_eq!(drive.take_menu_keys(), [13, 27]);
        assert!(drive.take_menu_keys().is_empty());
        for bad in ["-1", "256", "13 extra", ""] {
            assert!(Script::parse(&format!("t=0 menu_key {bad}")).is_err());
        }
    }

    #[test]
    fn controller_exec_edges_preserve_order_and_are_not_repeated() {
        let script = Script::parse("t=1 reload\nt=1 grab_input\nt=2 reload\n").unwrap();
        let mut drive = Drive::new(&script);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        drive.advance(0.5, &mut sim);
        assert!(drive.take_exec_inputs().is_empty());
        drive.advance(1.0, &mut sim);
        assert_eq!(drive.take_exec_inputs(), ["Reload", "Grab"]);
        drive.advance(1.0, &mut sim);
        assert!(drive.take_exec_inputs().is_empty());
        drive.advance(3.0, &mut sim);
        assert_eq!(drive.take_exec_inputs(), ["Reload"]);
        assert!(drive.take_exec_inputs().is_empty());
    }

    #[test]
    fn releasing_a_held_fire_button_dispatches_unfire_once() {
        // DefUser.ini: `Fire` = `Button bFire | Fire | onrelease UnFire`. A release of a button
        // that is up (a repeated `fire_hold 0`) or a press must not dispatch it.
        let script = Script::parse(
            "t=1 fire_hold 1
t=2 fire_hold 0
t=3 fire_hold 0
t=4 altfire_hold 1
t=5 altfire_hold 0
",
        )
        .unwrap();
        let mut drive = Drive::new(&script);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        drive.advance(1.0, &mut sim);
        assert_eq!(drive.take_hook_buttons(), ((true, true), (false, false)));
        assert!(
            drive.take_exec_inputs().is_empty(),
            "a press is not a release"
        );
        drive.advance(2.0, &mut sim);
        assert_eq!(drive.take_hook_buttons(), ((false, false), (false, false)));
        assert_eq!(drive.take_exec_inputs(), ["UnFire"]);
        drive.advance(3.0, &mut sim);
        assert!(drive.take_exec_inputs().is_empty(), "already released");
        drive.advance(4.0, &mut sim);
        assert!(drive.take_exec_inputs().is_empty());
        drive.advance(5.0, &mut sim);
        assert_eq!(drive.take_exec_inputs(), ["UnFire"]);
    }

    #[test]
    fn tracking_parses_aims_in_three_dimensions_and_off_releases_it() {
        let s = Script::parse("t=0 track Target\nt=1 track off\n").unwrap();
        assert_eq!(
            s.events[0].command,
            Command::Track(Some(("Target".into(), 0.6)))
        );
        assert_eq!(s.events[1].command, Command::Track(None));
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut d = Drive::new(&s);
        d.advance(0.0, &mut sim);
        d.set_track_location(Some("target"), Some([0.0, 10.0, 70.0]));
        d.advance(0.1, &mut sim);
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!((sim.pitch - (10.0f32 / 10.0).atan()).abs() < 1e-5);
        d.set_track_location(Some("other"), Some([10.0, 0.0, 100.0]));
        d.advance(0.2, &mut sim);
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        d.advance(1.0, &mut sim);
        d.set_track_location(Some("Target"), Some([10.0, 0.0, 100.0]));
        let yaw = sim.yaw;
        d.advance(1.1, &mut sim);
        assert_eq!(sim.yaw, yaw);
        assert!(Script::parse("t=0 track\n").is_err());
    }

    #[test]
    fn track_height_defaults_to_spine_band_and_override_applies() {
        let s = Script::parse("t=0 track Target\nt=1 track off\nt=2 track Target 0.85\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        d.advance(0.0, &mut sim);
        assert!((d.track_height() - 0.6).abs() < 1e-6);
        d.advance(1.0, &mut sim);
        assert!((d.track_height() - 0.6).abs() < 1e-6);
        // `track off` clears the actor (the height value is kept but unused while not tracking).
        d.advance(1.5, &mut sim);
        assert_eq!(d.tracking_actor(), None);
        d.advance(2.0, &mut sim);
        assert!((d.track_height() - 0.85).abs() < 1e-6);
        assert_eq!(d.tracking_actor(), Some("Target"));
        // A non-numeric extra token falls back to the default height.
        let s2 = Script::parse("t=0 track X 2a\n").unwrap();
        let mut d2 = Drive::new(&s2);
        d2.advance(0.0, &mut sim);
        assert!((d2.track_height() - 0.6).abs() < 1e-6);
    }

    #[test]
    fn follow_trails_a_named_actor_and_releases_inside_the_stop_distance() {
        let s = Script::parse("t=0 follow Cine3 200\nt=5 follow off\n").unwrap();
        assert_eq!(
            s.events[0].command,
            Command::Follow(Some(("Cine3".into(), 200.0)))
        );
        assert_eq!(s.events[1].command, Command::Follow(None));
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        d.advance(0.0, &mut sim);
        // `follow` also arms the host's location hook, so the actor resolves like `track`.
        assert_eq!(d.tracking_actor(), Some("Cine3"));
        // Far: forward held and aimed at the actor.
        d.set_track_location(Some("Cine3"), Some([500.0, 0.0, 60.0]));
        let i = d.advance(0.1, &mut sim);
        assert_eq!(i.forward, 1.0);
        assert!(sim.yaw.abs() < 1e-4, "aimed at the actor, got {}", sim.yaw);
        // Inside the stop distance: released, so the trailing player never drives into it.
        d.set_track_location(Some("Cine3"), Some([100.0, 0.0, 60.0]));
        assert_eq!(d.advance(0.2, &mut sim).forward, 0.0);
        // A default distance applies when the argument is omitted.
        let s2 = Script::parse("t=0 follow Cine3\n").unwrap();
        assert_eq!(
            s2.events[0].command,
            Command::Follow(Some(("Cine3".into(), 200.0)))
        );
        // `off` releases the follow control.
        d.set_track_location(Some("Cine3"), Some([500.0, 0.0, 60.0]));
        let i = d.advance(5.0, &mut sim);
        assert_eq!(i.forward, 0.0);
        assert!(Script::parse("t=0 follow\n").is_err());
    }

    #[test]
    fn crouch_command_holds_and_releases() {
        let s = Script::parse("t=0.0 crouch on\nt=1.0 duck off\n").unwrap();
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut d = Drive::new(&s);
        assert!(d.advance(0.0, &mut sim).crouch);
        assert!(
            d.advance(0.5, &mut sim).crouch,
            "crouch is held, not a one-shot"
        );
        assert!(!d.advance(1.0, &mut sim).crouch);
        // A missing/odd value is rejected.
        assert!(Script::parse("t=0.0 crouch\n").is_err());
    }

    // ---- item107 closed-loop commands -------------------------------------------------------

    const TICK: f32 = 1.0 / 60.0;

    fn seen(name: &str, at: [f32; 3], distance: f32) -> Seen {
        Seen {
            name: name.to_owned(),
            aim: at,
            distance,
            visible: true,
            health: None,
        }
    }

    fn actor(name: &str, state: ActorState, visible: bool, at: [f32; 3]) -> ActorFact {
        ActorFact {
            name: name.to_ascii_lowercase(),
            state,
            visible,
            aim: visible.then_some(at),
            distance: 100.0,
        }
    }

    type Stepped = (Input, (bool, bool), Vec<&'static str>);

    /// Runs one tick the way a host does: facts first (only while a block is active), then
    /// `advance`; returns the input and the `(fire_hold, fire_edge)` button state.
    fn step(d: &mut Drive, sim: &mut PlayerSim, t: &mut f32, facts: Facts) -> Stepped {
        if d.wants_facts() {
            d.set_facts(facts);
        }
        let input = d.advance(*t, sim);
        *t += TICK;
        let (fire, _) = d.take_hook_buttons();
        (input, fire, d.take_exec_inputs())
    }

    /// [`step`] that also collects the weapon switches and counts the `Reload` presses; returns
    /// whether the fire button is held.
    fn step_all(
        d: &mut Drive,
        sim: &mut PlayerSim,
        t: &mut f32,
        facts: Facts,
        switches: &mut Vec<WeaponInput>,
        reloads: &mut usize,
    ) -> (bool, bool) {
        let (_, fire, execs) = step(d, sim, t, facts);
        switches.extend(d.take_weapon_inputs());
        *reloads += execs.iter().filter(|e| **e == "Reload").count();
        fire
    }

    fn hostiles(list: Vec<Seen>) -> Facts {
        Facts {
            hostiles: list,
            weapon: Some(WeaponFact::default()),
            ..Facts::default()
        }
    }

    #[test]
    fn parses_engage_wait_until_and_goto_until() {
        let s = Script::parse(
            "t=1 engage\nt=2 engage 1500 12\nt=3 engage BaseSoldier3 800 5 aim=0.6\n\
             t=4 engage NastyHelico2\nt=5 wait_until dead BaseSoldier1\n\
             t=6 wait_until dead A or not alive B 12.5\nt=7 wait_until health_below 40 and goal 2\n\
             t=8 wait_until event CineTrigger2\nt=9 wait_until event CineTrigger2 .Touch 3\n\
             t=10 goto_until 100 -200 hostile_visible 900 30\nt=11 wait_until arrived\n\
             t=12 wait_until visible Foe or travel or hostile_visible 500\n",
        )
        .unwrap();
        let c = |i: usize| s.events[i].command.clone();
        assert_eq!(
            c(0),
            Command::Engage(EngageSpec {
                target: None,
                radius: 3000.0,
                secs: 30.0,
                aim: 0.85,
                hold: 0.0,
                period: 0.25,
                linger: 1.0
            })
        );
        assert_eq!(
            c(1),
            Command::Engage(EngageSpec {
                target: None,
                radius: 1500.0,
                secs: 12.0,
                aim: 0.85,
                hold: 0.0,
                period: 0.25,
                linger: 1.0
            })
        );
        assert_eq!(
            c(2),
            Command::Engage(EngageSpec {
                target: Some("BaseSoldier3".into()),
                radius: 800.0,
                secs: 5.0,
                aim: 0.6,
                hold: 0.0,
                period: 0.25,
                linger: 1.0
            })
        );
        assert!(
            matches!(c(3), Command::Engage(EngageSpec { target: Some(t), radius, .. })
                if t == "NastyHelico2" && radius == 3000.0)
        );
        assert_eq!(
            c(4),
            Command::WaitUntil {
                cond: Condition {
                    terms: vec![Cond::Dead("BaseSoldier1".into())],
                    all: false
                },
                timeout: 60.0
            }
        );
        assert_eq!(
            c(5),
            Command::WaitUntil {
                cond: Condition {
                    terms: vec![
                        Cond::Dead("A".into()),
                        Cond::Not(Box::new(Cond::Alive("B".into())))
                    ],
                    all: false
                },
                timeout: 12.5
            }
        );
        assert_eq!(
            c(6),
            Command::WaitUntil {
                cond: Condition {
                    terms: vec![Cond::HealthBelow(40.0), Cond::Goal(2)],
                    all: true
                },
                timeout: 60.0
            }
        );
        // The event suffix is optional and never swallows the timeout.
        assert!(matches!(c(7), Command::WaitUntil { cond, timeout }
            if timeout == 60.0 && cond.terms == vec![Cond::Event {
                target: "CineTrigger2".into(), suffix: ".Trigger".into() }]));
        assert!(matches!(c(8), Command::WaitUntil { cond, timeout }
            if timeout == 3.0 && cond.terms == vec![Cond::Event {
                target: "CineTrigger2".into(), suffix: ".Touch".into() }]));
        assert!(matches!(c(9), Command::GotoUntil { target, timeout, .. }
            if target == [100.0, -200.0, 0.0] && timeout == 30.0));
        assert!(matches!(c(10), Command::WaitUntil { cond, .. }
            if cond.terms == vec![Cond::Arrived]));
        assert!(matches!(c(11), Command::WaitUntil { cond, .. } if cond.terms.len() == 3));
        // Budget: the last event time plus every block's timeout (12 + 12 + 5 + 30 + 60 + 12.5
        // + 60 + 60 + 3 + 30 + 60 + 60 + 30 = engage x4, wait_until x8, goto_until x1).
        let blocks =
            30.0 + 12.0 + 5.0 + 30.0 + 60.0 + 12.5 + 60.0 + 60.0 + 3.0 + 30.0 + 60.0 + 60.0;
        assert!(
            (s.budget() - (12.0 + blocks)).abs() < 1e-3,
            "{}",
            s.budget()
        );
        // Malformed lines are errors, not silent defaults.
        for bad in [
            "t=0 engage -5",
            "t=0 engage 100 20 30",
            "t=0 engage A 100 B",
            "t=0 engage aim=x",
            "t=0 wait_until",
            "t=0 wait_until nothing",
            "t=0 wait_until dead",
            "t=0 wait_until dead A or alive B and goal 1",
            "t=0 wait_until goal x",
            "t=0 wait_until dead A 0",
            "t=0 wait_until dead A 5 6",
            "t=0 wait_until health_below NaN",
            "t=0 wait_until not",
            "t=0 goto_until 1 2",
            "t=0 goto_until 1 dead A",
        ] {
            assert!(Script::parse(bad).is_err(), "{bad:?} must not parse");
        }
    }

    #[test]
    fn engage_turns_to_the_nearest_visible_hostile_and_holds_fire_until_none_remain() {
        // `hold=100`: keep the button down while the target stays (the held-trigger discipline).
        let s = Script::parse(
            "t=0 engage 2000 20 hold=100
t=0.5 forward 1
",
        )
        .unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        // The creation tick has no facts yet: nothing fires, the script is blocked.
        let (i, fire, _) = step(&mut d, &mut sim, &mut t, Facts::default());
        assert!(d.wants_facts());
        assert_eq!((fire, i.forward), ((false, false), 0.0));
        // Nothing visible: still waiting, nothing fires.
        let (_, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![]));
        assert_eq!(fire, (false, false));
        // Two hostiles: the nearest one is aimed at (+Y, 10 up) and the trigger goes down.
        let far = seen("Far", [500.0, 0.0, 70.0], 500.0);
        let near = seen("Near", [0.0, 100.0, 70.0], 100.0);
        let (_, fire, _) = step(
            &mut d,
            &mut sim,
            &mut t,
            hostiles(vec![far.clone(), near.clone()]),
        );
        assert_eq!(fire, (true, true), "rising edge on the first tick");
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!((sim.pitch - (10.0f32 / 100.0).atan()).abs() < 1e-5);
        // The held trigger is not a new edge, and the target is kept when another gets closer.
        let nearer = seen("Nearer", [-30.0, 0.0, 70.0], 30.0);
        let (_, fire, execs) = step(
            &mut d,
            &mut sim,
            &mut t,
            hostiles(vec![far.clone(), near.clone(), nearer]),
        );
        assert_eq!(fire, (true, false));
        assert!(execs.is_empty());
        assert!(
            (sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5,
            "sticky target"
        );
        // Target lost: fire is released at once (UnFire exec) but the block holds for the linger.
        let (i, fire, execs) = step(&mut d, &mut sim, &mut t, hostiles(vec![]));
        assert_eq!(fire, (false, false));
        assert_eq!(execs, ["UnFire"]);
        assert_eq!(i.forward, 0.0, "script still blocked");
        assert!(d.wants_facts());
        // Reappearing inside the linger window resumes the same engagement.
        let mut resumed = false;
        for _ in 0..30 {
            let (_, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![far.clone()]));
            resumed |= fire == (true, true);
        }
        assert!(
            resumed,
            "the trigger is pressed again once the period has passed"
        );
        // Gone for a full second: the block ends.
        let mut ended = false;
        for _ in 0..90 {
            let (i, _, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![]));
            if !d.wants_facts() {
                ended = true;
                assert_eq!(i.forward, 0.0, "the 0.5 s event applies after the block");
                break;
            }
        }
        assert!(ended, "engage must end once nothing is visible for 1 s");
        // The event that was due 0.5 s into the script applies 0.5 s of script time later.
        let before = t;
        let mut applied = None;
        for _ in 0..120 {
            let (i, _, _) = step(&mut d, &mut sim, &mut t, Facts::default());
            if i.forward == 1.0 {
                applied = Some(t - TICK - before);
                break;
            }
        }
        let applied = applied.expect("forward 1 applies after the block");
        assert!(
            (applied - 0.5).abs() < 2.0 * TICK,
            "relative timing kept: {applied}"
        );
    }

    #[test]
    fn engage_without_a_target_waits_out_its_timeout_and_the_script_continues() {
        let s = Script::parse("t=1 engage 1000 2\nt=1.5 jump\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        let mut jump_at = None;
        let mut blocked_until = None;
        for _ in 0..600 {
            let at = t;
            let (i, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![]));
            assert_eq!(fire, (false, false), "never fires without a visible target");
            if blocked_until.is_none() && at > 1.1 && !d.wants_facts() {
                blocked_until = Some(at);
            }
            if i.jump {
                jump_at = Some(at);
                break;
            }
        }
        // Engage starts at 1.0, times out at 3.0 (never acquired: no early end), the jump that
        // was due 0.5 s after it comes 0.5 s later.
        let blocked_until = blocked_until.expect("the block timed out");
        assert!((blocked_until - 3.0).abs() < 3.0 * TICK, "{blocked_until}");
        let jump_at = jump_at.expect("jump applied");
        assert!((jump_at - 3.5).abs() < 3.0 * TICK, "{jump_at}");
    }

    #[test]
    fn engage_taps_at_the_period_holds_when_asked_and_empty_weapons_reload() {
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let target = seen("T", [100.0, 0.0, 70.0], 100.0);
        let facts = |clip: Option<u32>| Facts {
            hostiles: vec![target.clone()],
            weapon: Some(WeaponFact {
                clip,
                ..WeaponFact::default()
            }),
            ..Facts::default()
        };
        let s = Script::parse("t=0 engage 2000 20\n").unwrap();
        let mut d = Drive::new(&s);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        // Default: a one-tick tap every 0.25 s, each release dispatching UnFire.
        let (mut presses, mut releases, mut down_ticks) = (0, 0, 0);
        for _ in 0..120 {
            let (_, (held, edge), execs) = step(&mut d, &mut sim, &mut t, facts(Some(10)));
            presses += usize::from(edge);
            down_ticks += usize::from(held);
            releases += execs.iter().filter(|e| **e == "UnFire").count();
        }
        assert!(
            (7..=9).contains(&presses),
            "2 s at 0.25 s: {presses} presses"
        );
        assert_eq!(down_ticks, presses, "a tap is down for exactly one tick");
        assert!(releases + 1 >= presses && releases <= presses);
        // `hold=` keeps the button down longer: 0.1 s of every 0.5 s.
        let s2 = Script::parse("t=0 engage 2000 20 hold=0.1 period=0.5\n").unwrap();
        let mut d = Drive::new(&s2);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let (mut presses, mut down_ticks) = (0, 0);
        for _ in 0..120 {
            let (_, (held, edge), _) = step(&mut d, &mut sim, &mut t, facts(Some(10)));
            presses += usize::from(edge);
            down_ticks += usize::from(held);
        }
        assert!((3..=5).contains(&presses), "{presses}");
        assert!(
            (presses * 6..=presses * 8).contains(&down_ticks),
            "about 0.1 s (6-7 ticks) per press: {down_ticks} for {presses}"
        );
        // A route-owned held button is not taken over.
        let s3 = Script::parse("t=0 fire_hold 1\nt=0 engage 2000 20\n").unwrap();
        let mut d = Drive::new(&s3);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let mut edges = 0;
        for _ in 0..60 {
            let (_, (held, edge), _) = step(&mut d, &mut sim, &mut t, facts(Some(10)));
            edges += usize::from(edge);
            assert!(held);
        }
        assert_eq!(edges, 0, "engage must leave a fire_hold button alone");
        let mut d = Drive::new(&s);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        // Empty and nothing else to shoot: the trigger is released, one Reload is pressed and
        // not repeated, no weapon switch is possible, and after the reload had its time the
        // engagement ends with the script free to act (it cannot fire).
        let mut reloads = 0;
        let mut held_any = false;
        let mut ended_at = None;
        for i in 0..400 {
            let (_, (held, _), execs) = step(&mut d, &mut sim, &mut t, facts(Some(0)));
            reloads += execs.iter().filter(|e| **e == "Reload").count();
            held_any |= held;
            if !d.wants_facts() {
                ended_at = Some(i);
                break;
            }
        }
        assert_eq!(reloads, 1, "one Reload press per dry spell");
        assert!(!held_any, "no trigger on an empty magazine");
        let ended_at = ended_at.expect("out of ammo ends the engagement");
        assert!(
            (140..=170).contains(&ended_at),
            "gives the reload ~2.5 s first: {ended_at} ticks"
        );
    }

    #[test]
    fn engage_switches_to_a_carried_weapon_with_ammo_and_skips_fists_at_range() {
        let s = Script::parse(
            "t=0 engage 2000 30
",
        )
        .unwrap();
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        let target = seen("T", [100.0, 0.0, 70.0], 100.0);
        let with = |weapon: WeaponFact| Facts {
            hostiles: vec![target.clone()],
            weapon: Some(weapon),
            ..Facts::default()
        };
        let dry = WeaponFact {
            clip: Some(0),
            alternatives: vec![11, 3],
            ..WeaponFact::default()
        };
        let mut d = Drive::new(&s);
        step(&mut d, &mut sim, &mut t, Facts::default());
        let mut switches = Vec::new();
        let mut reloads = 0;
        for _ in 0..240 {
            step_all(
                &mut d,
                &mut sim,
                &mut t,
                with(dry.clone()),
                &mut switches,
                &mut reloads,
            );
        }
        // One Reload, then the first carried weapon with ammo is selected (retried every 1.5 s
        // while the old weapon is still reported dry, at most a few times).
        assert_eq!(reloads, switches.len() + 1, "one Reload per weapon tried");
        assert_eq!(switches.first(), Some(&WeaponInput::Switch(11)));
        assert!((1..=3).contains(&switches.len()), "{switches:?}");
        // Fists with a gun carried: switch at once, never fire the fists, no Reload.
        let fists = WeaponFact {
            melee: true,
            alternatives: vec![2],
            ..WeaponFact::default()
        };
        let mut d = Drive::new(&s);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let (mut switches, mut reloads) = (Vec::new(), 0);
        let mut held_any = false;
        for _ in 0..60 {
            held_any |= step_all(
                &mut d,
                &mut sim,
                &mut t,
                with(fists.clone()),
                &mut switches,
                &mut reloads,
            )
            .0;
        }
        assert!(!held_any, "no punching at range");
        assert_eq!(
            (reloads, switches.first()),
            (0, Some(&WeaponInput::Switch(2)))
        );
        // Fists and no gun: fire anyway (nothing else to do; the block is not abandoned).
        let bare = WeaponFact {
            melee: true,
            ..WeaponFact::default()
        };
        let mut d = Drive::new(&s);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let (mut switches, mut reloads) = (Vec::new(), 0);
        let mut presses = 0;
        for _ in 0..30 {
            let (_, edge) = step_all(
                &mut d,
                &mut sim,
                &mut t,
                with(bare.clone()),
                &mut switches,
                &mut reloads,
            );
            presses += usize::from(edge);
        }
        assert!(presses >= 1 && switches.is_empty());
    }

    #[test]
    fn engage_named_target_ends_when_it_dies_and_ignores_other_hostiles() {
        let s = Script::parse("t=0 engage Heli 3000 20\nt=0 jump\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let named = |state, visible| Facts {
            actors: vec![actor("heli", state, visible, [0.0, 400.0, 200.0])],
            hostiles: vec![seen("Soldier", [100.0, 0.0, 70.0], 100.0)],
            weapon: Some(WeaponFact::default()),
            ..Facts::default()
        };
        // Hidden target: no fire even though a hostile soldier is in view.
        let (_, fire, _) = step(&mut d, &mut sim, &mut t, named(ActorState::Alive, false));
        assert_eq!(fire, (false, false));
        // Visible: aims at the aim point, 400 UU to +Y and 140 above the eye.
        let (_, fire, _) = step(&mut d, &mut sim, &mut t, named(ActorState::Alive, true));
        assert_eq!(fire, (true, true));
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!((sim.pitch - (140.0f32 / 400.0).atan()).abs() < 1e-5);
        // Dead: the block ends at once and the trigger is released.
        let (i, fire, execs) = step(&mut d, &mut sim, &mut t, named(ActorState::Dead, false));
        assert_eq!(fire, (false, false));
        assert_eq!(execs, ["UnFire"]);
        assert!(!d.wants_facts());
        assert!(i.jump, "the event after the block applies in the same tick");
    }

    #[test]
    fn engage_pauses_goto_and_resumes_the_waypoint_afterwards() {
        let s = Script::parse("t=0 goto 1000 0\nt=0.1 engage 2000 20\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        let (i, _, _) = step(&mut d, &mut sim, &mut t, Facts::default());
        assert_eq!(i.forward, 1.0, "walking");
        for _ in 0..8 {
            step(&mut d, &mut sim, &mut t, Facts::default());
        }
        let target = seen("T", [0.0, 500.0, 70.0], 500.0);
        let (i, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![target]));
        assert_eq!(fire, (true, true));
        assert_eq!(i.forward, 0.0, "stands to shoot");
        assert!(
            (sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5,
            "goto must not steal the aim"
        );
        // Target gone for the linger: engagement ends and the player walks on toward the waypoint.
        let mut walking_again = false;
        for _ in 0..200 {
            let (i, _, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![]));
            if !d.wants_facts() && i.forward == 1.0 {
                walking_again = true;
                assert!(
                    sim.yaw.abs() < 1e-4,
                    "re-aimed along the waypoint, got {}",
                    sim.yaw
                );
                break;
            }
        }
        assert!(walking_again);
    }

    #[test]
    fn engage_does_nothing_while_input_is_suppressed() {
        let s = Script::parse("t=0 engage 2000 3\nt=0 jump\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let mut facts = hostiles(vec![seen("T", [100.0, 0.0, 70.0], 100.0)]);
        facts.suppressed = true;
        let yaw = sim.yaw;
        let (_, fire, _) = step(&mut d, &mut sim, &mut t, facts);
        assert_eq!(fire, (false, false));
        assert_eq!(sim.yaw, yaw, "no aiming during a cinematic");
        // The timeout still ends the block (3 s), so a cinematic cannot hang the script.
        let mut ended = false;
        for _ in 0..240 {
            let mut facts = hostiles(vec![]);
            facts.suppressed = true;
            step(&mut d, &mut sim, &mut t, facts);
            if !d.wants_facts() {
                ended = true;
                break;
            }
        }
        assert!(ended);
    }

    #[test]
    fn wait_until_dead_blocks_the_script_and_shifts_later_events() {
        let s = Script::parse("t=1 wait_until dead Foo 10\nt=1.5 yaw 90\nt=2 jump\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        let alive = Facts {
            actors: vec![actor("foo", ActorState::Alive, true, [0.0; 3])],
            ..Facts::default()
        };
        let dead = Facts {
            actors: vec![actor("foo", ActorState::Dead, false, [0.0; 3])],
            ..Facts::default()
        };
        // Blocked from t=1; at t=6 the target dies.
        let mut jump_at = None;
        for _ in 0..1200 {
            let now = t;
            let facts = if now >= 6.0 {
                dead.clone()
            } else {
                alive.clone()
            };
            let (i, _, _) = step(&mut d, &mut sim, &mut t, facts);
            if now < 5.9 {
                assert!(
                    sim.yaw.abs() < 1e-6,
                    "yaw 90 must wait for the condition ({now})"
                );
            }
            if i.jump {
                jump_at = Some(now);
                break;
            }
        }
        // The condition is seen at ~6.0; the jump was 1 s of script time after the wait.
        let jump_at = jump_at.expect("jump");
        assert!((jump_at - 7.0).abs() < 4.0 * TICK, "{jump_at}");
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    }

    #[test]
    fn wait_until_times_out_without_the_condition_and_missing_actors_are_not_dead() {
        let s = Script::parse("t=0 wait_until dead Ghost 2\nt=0 jump\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        let mut jump_at = None;
        for _ in 0..400 {
            let now = t;
            // A name the host never found is `Missing`, which is neither alive nor dead.
            let facts = Facts {
                actors: vec![actor("ghost", ActorState::Missing, false, [0.0; 3])],
                ..Facts::default()
            };
            let (i, _, _) = step(&mut d, &mut sim, &mut t, facts);
            if i.jump {
                jump_at = Some(now);
                break;
            }
        }
        let jump_at = jump_at.expect("the timeout releases the block");
        assert!((jump_at - 2.0).abs() < 3.0 * TICK, "{jump_at}");
    }

    #[test]
    fn wait_until_conditions_goal_health_visible_and_connectors() {
        let eval = |cond: &str, facts: Facts, goto: bool| -> bool {
            let s = Script::parse(&format!(
                "t=0 {}wait_until {cond}\nt=0 jump\n",
                if goto { "goto 5000 0\nt=0 " } else { "" }
            ))
            .unwrap();
            let mut d = Drive::new(&s);
            let mut sim = PlayerSim::new([0.0; 3], 0.0);
            let mut t = 0.0;
            step(&mut d, &mut sim, &mut t, Facts::default());
            // The block is evaluated from the second tick on.
            step(&mut d, &mut sim, &mut t, facts.clone());
            step(&mut d, &mut sim, &mut t, facts);
            !d.wants_facts()
        };
        let base = Facts {
            player_health: Some(35.0),
            goals: vec![false, true],
            actors: vec![
                actor("a", ActorState::Alive, true, [0.0; 3]),
                actor("b", ActorState::Alive, false, [0.0; 3]),
            ],
            hostiles: vec![seen("H", [0.0; 3], 400.0)],
            ..Facts::default()
        };
        assert!(eval("health_below 40", base.clone(), false));
        assert!(
            !eval("health_below 35", base.clone(), false),
            "strictly below"
        );
        assert!(eval("health_above 30", base.clone(), false));
        assert!(
            !eval("health_above 35", base.clone(), false),
            "strictly above"
        );
        assert!(eval("goal 1", base.clone(), false));
        assert!(!eval("goal 0", base.clone(), false));
        assert!(
            !eval("goal 7", base.clone(), false),
            "unknown objective is not complete"
        );
        assert!(!eval("goal -1", base.clone(), false));
        assert!(eval("visible A", base.clone(), false));
        assert!(!eval("visible B", base.clone(), false));
        assert!(eval("alive B", base.clone(), false));
        assert!(eval("not dead B", base.clone(), false));
        assert!(eval("hostile_visible 500", base.clone(), false));
        assert!(!eval("hostile_visible 300", base.clone(), false));
        assert!(eval("goal 0 or goal 1", base.clone(), false));
        assert!(!eval("goal 0 and goal 1", base.clone(), false));
        assert!(eval("goal 1 and visible A", base.clone(), false));
        assert!(
            !eval("arrived", base.clone(), true),
            "a waypoint is pending"
        );
        assert!(eval("arrived", base, false));
    }

    #[test]
    fn wait_until_travel_counts_from_the_start_of_the_block() {
        let s = Script::parse("t=0 wait_until travel 30\nt=0 jump\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        d.set_travel_count(2); // a travel before the block does not count
        step(&mut d, &mut sim, &mut t, Facts::default());
        step(&mut d, &mut sim, &mut t, Facts::default());
        step(&mut d, &mut sim, &mut t, Facts::default());
        assert!(d.wants_facts());
        d.set_travel_count(3);
        step(&mut d, &mut sim, &mut t, Facts::default());
        assert!(!d.wants_facts());
    }

    #[test]
    fn wait_until_event_scans_the_trace_incrementally_and_notices_a_disabled_trace() {
        let ev = |tick: u64, target: &str, function: &str| TraceEvent {
            tick,
            time: tick as f64,
            kind: TraceKind::Event {
                target: target.to_owned(),
                function: function.to_owned(),
                args: Vec::new(),
            },
        };
        let s = Script::parse("t=0 wait_until event CineTrigger2 .Trigger 30\nt=0 jump\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        let mut trace = vec![
            ev(1, "Other", "Trigger.Trigger"),
            ev(2, "CineTrigger2", "Actor.Touch"),
        ];
        step(&mut d, &mut sim, &mut t, Facts::default());
        d.observe_trace(&trace, true);
        step(&mut d, &mut sim, &mut t, Facts::default());
        assert!(
            d.wants_facts(),
            "wrong target / wrong function must not satisfy it"
        );
        // The matching event arrives later; only the new entries are scanned.
        trace.push(ev(3, "cinetrigger2", "CounterTrigger.Trigger"));
        d.observe_trace(&trace, true);
        step(&mut d, &mut sim, &mut t, Facts::default());
        assert!(!d.wants_facts());
        // An event that fired before the block began still counts (the block rescans).
        let s = Script::parse("t=0 wait_until event CineTrigger2\n").unwrap();
        let mut d = Drive::new(&s);
        step(&mut d, &mut sim, &mut t, Facts::default());
        d.observe_trace(&trace, true);
        step(&mut d, &mut sim, &mut t, Facts::default());
        assert!(!d.wants_facts());
        // Disabled VM trace: the condition cannot hold, and the block still times out.
        let s = Script::parse("t=0 wait_until event CineTrigger2 1\n").unwrap();
        let mut d = Drive::new(&s);
        step(&mut d, &mut sim, &mut t, Facts::default());
        for _ in 0..120 {
            d.observe_trace(&trace, false);
            step(&mut d, &mut sim, &mut t, Facts::default());
        }
        assert!(!d.wants_facts(), "timed out rather than hanging");
    }

    #[test]
    fn goto_until_ends_on_arrival_or_on_the_condition_and_keeps_walking_meanwhile() {
        let s = Script::parse("t=0 goto_until 1000 0 dead Foo 20\nt=0 jump\n").unwrap();
        let alive = Facts {
            actors: vec![actor("foo", ActorState::Alive, true, [0.0; 3])],
            ..Facts::default()
        };
        // Arrival ends it.
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], std::f32::consts::PI);
        let mut t = 0.0;
        let (i, _, _) = step(&mut d, &mut sim, &mut t, alive.clone());
        assert_eq!(i.forward, 1.0);
        assert!(sim.yaw.abs() < 1e-4, "steers to the waypoint");
        step(&mut d, &mut sim, &mut t, alive.clone());
        assert!(d.wants_facts(), "still walking");
        sim.location = [990.0, 0.0, 0.0];
        for _ in 0..4 {
            step(&mut d, &mut sim, &mut t, alive.clone());
        }
        assert!(!d.wants_facts(), "arrived");
        // The condition ends it early, and the waypoint is still pursued afterwards.
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, alive.clone());
        step(&mut d, &mut sim, &mut t, alive);
        let dead = Facts {
            actors: vec![actor("foo", ActorState::Dead, false, [0.0; 3])],
            ..Facts::default()
        };
        let (i, _, _) = step(&mut d, &mut sim, &mut t, dead);
        assert!(!d.wants_facts());
        assert_eq!(
            i.forward, 1.0,
            "goto steering continues after the condition"
        );
    }

    #[test]
    fn blocked_clock_survives_repeated_tiny_steps_and_back_to_back_blocks() {
        // Two consecutive blocks, then an event: every block adds its own wait.
        let s = Script::parse("t=1 wait_until goal 0 5\nt=1 wait_until goal 1 5\nt=1.25 jump\n")
            .unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        let facts = |g0: bool, g1: bool| Facts {
            goals: vec![g0, g1],
            ..Facts::default()
        };
        let mut jump_at = None;
        for _ in 0..3000 {
            let now = t;
            // Goal 0 completes at 4.0, goal 1 at 9.0.
            let (i, _, _) = step(&mut d, &mut sim, &mut t, facts(now >= 4.0, now >= 9.0));
            if i.jump {
                jump_at = Some(now);
                break;
            }
        }
        // 1 s run, wait until ~4.0, second block from ~4.0 until ~9.0, then 0.25 s of script.
        let jump_at = jump_at.expect("jump");
        assert!((jump_at - 9.25).abs() < 5.0 * TICK, "{jump_at}");
    }
    // ---- standing engagement (`engage_on` / `engage_off`) ------------------------------------

    fn seen_hp(name: &str, at: [f32; 3], distance: f32, health: f32) -> Seen {
        Seen {
            health: Some(health),
            ..seen(name, at, distance)
        }
    }

    #[test]
    fn parses_engage_on_and_off() {
        let s = Script::parse(
            "t=1 engage_on\nt=2 engage_on 2500 aim=0.9 period=0.1 hold=0.05\nt=3 engage_off\n\
             t=4 engage-on 900\n",
        )
        .unwrap();
        assert!(matches!(&s.events[0].command, Command::EngageOn(e)
            if e.target.is_none() && e.radius == 3000.0 && e.period == 0.25));
        assert!(matches!(&s.events[1].command, Command::EngageOn(e)
            if e.radius == 2500.0 && e.aim == 0.9 && e.period == 0.1 && e.hold == 0.05));
        assert_eq!(s.events[2].command, Command::EngageOff);
        assert!(matches!(&s.events[3].command, Command::EngageOn(e) if e.radius == 900.0));
        // A standing engagement has no named target and `engage_off` takes no arguments.
        for bad in [
            "t=0 engage_on Foe",
            "t=0 engage_on 1000 nonsense",
            "t=0 engage_off now",
            "t=0 engage_on -3",
        ] {
            assert!(Script::parse(bad).is_err(), "{bad:?} must not parse");
        }
        // It never blocks the script clock, so it adds nothing to the time budget.
        let plain = Script::parse("t=9 jump\n").unwrap();
        let with = Script::parse("t=1 engage_on 1000\nt=9 jump\nt=9.5 engage_off\n").unwrap();
        assert_eq!(plain.budget(), 9.0);
        assert_eq!(with.budget(), 9.5);
    }

    #[test]
    fn engage_on_shoots_beside_the_script_clock_and_engage_off_lets_go() {
        let s = Script::parse(
            "t=0 engage_on 2000 period=0.1\nt=1 forward 1\nt=2 engage_off\nt=2.5 jump\n",
        )
        .unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        let foe = seen("Foe", [0.0, 100.0, 70.0], 100.0);
        // Armed: the host is asked for facts although no command blocks.
        step(&mut d, &mut sim, &mut t, Facts::default());
        assert!(d.wants_facts());
        // Nothing visible: the script runs on its own clock and no trigger is pressed.
        let (mut presses, mut forward_at) = (0, None);
        while t < 1.2 {
            let now = t;
            let (i, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![]));
            presses += usize::from(fire.1);
            if i.forward == 1.0 && forward_at.is_none() {
                forward_at = Some(now);
            }
        }
        assert_eq!(presses, 0);
        let forward_at = forward_at.expect("the walk command applied on schedule");
        assert!((forward_at - 1.0).abs() < 2.0 * TICK, "{forward_at}");
        // A hostile shows up: aimed at and the trigger tapped at the period, while the script
        // keeps its own time.
        let mut edges = 0;
        let mut releases = 0;
        while t < 1.95 {
            let (_, fire, execs) = step(&mut d, &mut sim, &mut t, hostiles(vec![foe.clone()]));
            edges += usize::from(fire.1);
            releases += execs.iter().filter(|e| **e == "UnFire").count();
        }
        assert!(
            (edges as f32 - 7.5).abs() <= 1.5,
            "taps at the 0.1 s period: {edges}"
        );
        assert!(
            releases + 1 >= edges,
            "every tap is released again: {releases}"
        );
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        // `engage_off` at t=2 stops the shooting even though the hostile is still there, and the
        // later event still happens on schedule.
        let mut jumped_at = None;
        let mut late_edges = 0;
        while t < 3.0 {
            let now = t;
            let (i, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![foe.clone()]));
            late_edges += usize::from(fire.1 && now > 2.05);
            if i.jump {
                jumped_at = Some(now);
            }
        }
        assert_eq!(late_edges, 0);
        assert!(!d.wants_facts(), "no facts needed once it is off");
        let jumped_at = jumped_at.expect("jump");
        assert!((jumped_at - 2.5).abs() < 2.0 * TICK, "{jumped_at}");
    }

    #[test]
    fn standing_engagement_pauses_a_goto_while_a_target_is_shot_and_resumes_it() {
        let s = Script::parse("t=0 engage_on 2000\nt=0 goto 1000 0\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let (i, _, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![]));
        assert_eq!(i.forward, 1.0, "walking while nothing is in sight");
        let foe = seen("Foe", [0.0, 300.0, 70.0], 300.0);
        let (i, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![foe]));
        assert_eq!(i.forward, 0.0, "stands to shoot");
        assert!(fire.0);
        let (i, _, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![]));
        assert_eq!(
            i.forward, 1.0,
            "the waypoint resumes once the target is gone"
        );
        // The standing engagement never ends by itself: a later hostile is shot again.
        for _ in 0..200 {
            step(&mut d, &mut sim, &mut t, hostiles(vec![]));
        }
        let foe = seen("Foe2", [0.0, 300.0, 70.0], 300.0);
        let (_, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![foe]));
        assert!(fire.0, "armed for the whole run");
    }

    #[test]
    fn engage_turns_to_the_next_hostile_when_shots_do_not_hurt_the_current_one() {
        // `Cover` is nearest but its health never falls (the bullets go into cover the sight
        // line does not show); after 8 presses over 2.5 s the engagement turns to `Open`.
        let s = Script::parse("t=0 engage_on 2000 period=0.4\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let cover = seen_hp("Cover", [0.0, 100.0, 70.0], 100.0, 150.0);
        let open = seen_hp("Open", [500.0, 0.0, 70.0], 500.0, 150.0);
        let mut presses = 0;
        let mut turned_after = None;
        for _ in 0..400 {
            let (_, fire, _) = step(
                &mut d,
                &mut sim,
                &mut t,
                hostiles(vec![cover.clone(), open.clone()]),
            );
            presses += usize::from(fire.1);
            if sim.yaw.abs() < 1e-3 && turned_after.is_none() {
                turned_after = Some(presses);
            }
        }
        // 8 presses at the cover, the 9th at the open target.
        let turned_after = turned_after.expect("turned to the other hostile");
        assert!((8..=9).contains(&turned_after), "{turned_after}");
        // Nothing else visible: the ignored one is better than idling.
        let (_, fire, _) = step(&mut d, &mut sim, &mut t, hostiles(vec![cover.clone()]));
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
        assert!(fire.0 || presses > 0);
    }

    #[test]
    fn a_fast_weapon_is_not_judged_before_the_time_floor() {
        // Break-the-code counterpart: 8 presses of a 10-a-second weapon take 0.8 s, too soon to
        // call the target unhurt (the first rounds may simply miss).
        let s = Script::parse(
            "t=0 engage_on 2000 period=0.1
",
        )
        .unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let cover = seen_hp("Cover", [0.0, 100.0, 70.0], 100.0, 150.0);
        let open = seen_hp("Open", [500.0, 0.0, 70.0], 500.0, 150.0);
        let mut presses = 0;
        while t < 2.3 {
            let (_, fire, _) = step(
                &mut d,
                &mut sim,
                &mut t,
                hostiles(vec![cover.clone(), open.clone()]),
            );
            presses += usize::from(fire.1);
        }
        assert!(presses >= 12, "many presses already: {presses}");
        assert!(
            (sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5,
            "still on the nearest target after {presses} presses"
        );
        // Past the floor it turns away.
        while t < 3.0 {
            step(
                &mut d,
                &mut sim,
                &mut t,
                hostiles(vec![cover.clone(), open.clone()]),
            );
        }
        assert!(sim.yaw.abs() < 1e-3, "turned to the other hostile");
    }

    #[test]
    fn engage_keeps_a_target_whose_health_falls_even_slowly() {
        // Break-the-code counterpart: any damage resets the count, so a tough target is not
        // abandoned while the shots land.
        let s = Script::parse("t=0 engage_on 2000 period=0.1\n").unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let mut hp = 150.0;
        let open = seen_hp("Open", [500.0, 0.0, 70.0], 500.0, 150.0);
        for _ in 0..360 {
            let tough = seen_hp("Tough", [0.0, 100.0, 70.0], 100.0, hp);
            let (_, fire, _) = step(
                &mut d,
                &mut sim,
                &mut t,
                hostiles(vec![tough, open.clone()]),
            );
            if fire.1 {
                hp -= 1.0;
            }
        }
        assert!(
            (sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5,
            "still on the target that is being hurt"
        );
        // A target without a readable health (not a pawn) is never given up on.
        let mut d = Drive::new(&s);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let blind = seen("Blind", [0.0, 100.0, 70.0], 100.0);
        for _ in 0..360 {
            step(
                &mut d,
                &mut sim,
                &mut t,
                hostiles(vec![blind.clone(), open.clone()]),
            );
        }
        assert!((sim.yaw - std::f32::consts::FRAC_PI_2).abs() < 1e-5);
    }
    #[test]
    fn standing_engagement_with_an_empty_gun_lets_the_waypoint_walk_on() {
        // Measured on Amos01: the level's Beretta ran dry, the standing engagement kept the aim on a
        // distant soldier and the player stood there for the rest of the run.
        let s = Script::parse(
            "t=0 engage_on 2000
t=0 goto 1000 0
",
        )
        .unwrap();
        let mut d = Drive::new(&s);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        let mut t = 0.0;
        step(&mut d, &mut sim, &mut t, Facts::default());
        let foe = seen("Foe", [0.0, 300.0, 70.0], 300.0);
        let dry = |foe: &Seen| Facts {
            hostiles: vec![foe.clone()],
            weapon: Some(WeaponFact {
                clip: Some(0),
                melee: false,
                alternatives: vec![],
            }),
            ..Facts::default()
        };
        // While the reload is given its time the player stands and aims.
        let (i, _, execs) = step(&mut d, &mut sim, &mut t, dry(&foe));
        assert_eq!(i.forward, 0.0);
        assert!(execs.contains(&"Reload"));
        // Past the reload wait with nothing else to switch to: nothing can fire, the aim is dropped and
        // the pending waypoint walks on.
        let mut walked = false;
        while t < 6.0 {
            let (i, fire, _) = step(&mut d, &mut sim, &mut t, dry(&foe));
            assert!(!fire.0, "an empty gun is never pressed");
            walked |= i.forward == 1.0;
        }
        assert!(walked, "the waypoint resumes");
    }
}
