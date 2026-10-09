//! Text input scripts for the deterministic `--play-script` driver.
//!
//! One command per line: `t=<seconds> <command> [args]`. Blank lines and `#` comments are
//! ignored. Commands are applied in time order at the start of the fixed tick whose elapsed
//! time reaches `t`; the resulting [`Input`] drives the same [`crate::play::sim::PlayerSim`]
//! the interactive window uses.
//!
//! Supported commands:
//! - `switch_weapon <byte>` / `next_weapon`: normal controller weapon selection execs.
//! - `forward <v>` / `back <v>`: set the forward axis (`back` negates `v`).
//! - `right <v>` / `left <v>`: set the strafe axis (`left` negates `v`).
//! - `walk <0|1|on|off>`: set the Shift walk modifier.
//! - `crouch <0|1|on|off>`: hold the crouch key (XIII `C=Duck`).
//! - `jump`: request one jump (edge-triggered, consumed by the next tick).
//! - `yaw <degrees>`: set absolute yaw (Unreal convention: 0 = +X, positive toward +Y).
//! - `turn <degrees>`: add to yaw.
//! - `pitch <degrees>`: set camera pitch.
//! - `track <actor|off>`: continuously aim at a named actor (resolved by the playback host).
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

use std::path::Path;

use crate::play::sim::{Input, PlayerSim};

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
    /// Continuously aim at an actor; `None` disables tracking.
    Track(Option<String>),
    /// Move the box centre to an absolute Unreal-unit position (harness bootstrap).
    Teleport([f32; 3]),
    /// Autopilot toward an Unreal-unit waypoint (the driver re-aims and walks; not a teleport).
    Goto([f32; 3]),
    /// Request one use/interact action (edge-triggered; the VM `Grab`/use chain). Ray-based: the
    /// host picks the actor in front of the camera.
    Use,
    /// Named use (edge-triggered); deco pickups require the game's current aimed TargetActor.
    /// Needed for invisible interaction
    /// doors (Plage01 `Porte1`); corpses and pickups require the game's aimed TargetActor.
    UseNamed(String),
    /// Request one fire action (edge-triggered; routed to the player's weapon, item14).
    Fire,
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
    /// Normal player controller NextWeapon exec.
    NextWeapon,
    /// item18 diagnostic command: give the local player control by running the game's own
    /// `XIIIPlayerController.EnterStartState` with `bOkForMoving = true` (the HUD's normal
    /// "first display done" transition). Needed because the decoded Plage01 intro leaves the
    /// controller frozen in `NoControl` when a diagnostic script does not play the authored
    /// cutscene sequence. Normal campaign play does not depend on this command.
    TakeControl,
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
                    Some(actor) => Command::Track(Some(actor.to_owned())),
                    None => return Err(format!("line {n}: track needs an actor name or off")),
                },
                "teleport" | "place" => {
                    let x = num(&mut it)?;
                    let y = num(&mut it)?;
                    let z = num(&mut it)?;
                    Command::Teleport([x, y, z])
                }
                "goto" => {
                    let x = num(&mut it)?;
                    let y = num(&mut it)?;
                    // The waypoint Z is optional; 0 is fine for the horizontal autopilot.
                    let z = it.next().and_then(|v| v.parse::<f32>().ok()).unwrap_or(0.0);
                    Command::Goto([x, y, z])
                }
                "use" | "grab" | "interact" => match it.next() {
                    Some(target) => Command::UseNamed(target.to_owned()),
                    None => Command::Use,
                },
                "fire" | "shoot" => Command::Fire,
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
                "next_weapon" => {
                    if it.next().is_some() {
                        return Err(format!("line {n}: next_weapon takes no arguments"));
                    }
                    Command::NextWeapon
                }
                "take_control" | "take-control" | "assume_control" => Command::TakeControl,
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

    /// Whether the script contains a `wait_travel` command (the host then keeps running until the
    /// game requests travel).
    pub fn has_wait_travel(&self) -> bool {
        self.events
            .iter()
            .any(|e| matches!(e.command, Command::WaitTravel))
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
    weapon_inputs: Vec<Option<u8>>,
    /// Weapons requested (`weapon <Package.Class>`) and not yet applied by the host.
    weapons: Vec<String>,
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
    tracking: Option<String>,
    track_location: Option<[f32; 3]>,
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
            use_named: Vec::new(),
            goto: None,
            waiting_travel: false,
            goals: Vec::new(),
            control_pending: false,
            tracking: None,
            track_location: None,
        }
    }

    /// Drains the `weapon` commands due so far (the host grants them through the VM). Kept out of
    /// [`Input`] because it is not a per-tick axis and carries a class path.
    pub fn take_weapons(&mut self) -> Vec<String> {
        std::mem::take(&mut self.weapons)
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

    /// Whether the driver is blocked on a `wait_travel` command.
    pub fn waiting_travel(&self) -> bool {
        self.waiting_travel
    }

    /// Releases a `wait_travel` block (the host observed the travel request).
    pub fn notify_travel(&mut self) {
        self.waiting_travel = false;
    }

    /// Drains normal weapon-selection inputs, preserving repeated commands in order.
    pub fn take_weapon_inputs(&mut self) -> Vec<Option<u8>> {
        std::mem::take(&mut self.weapon_inputs)
    }

    /// Applies every event due at or before `elapsed` and returns this tick's input.
    ///
    /// Yaw/pitch commands are applied directly to `sim` (they are orientation, not an axis).
    pub fn advance(&mut self, elapsed: f32, sim: &mut PlayerSim) -> Input {
        if self.waiting_travel {
            return Input::default();
        }
        while self.cursor < self.events.len() && self.events[self.cursor].t <= elapsed + 1e-6 {
            match &self.events[self.cursor].command {
                &Command::Forward(v) => self.forward = v,
                &Command::Right(v) => self.right = v,
                &Command::Walk(v) => self.walk = v,
                &Command::Crouch(v) => self.crouch = v,
                Command::Jump => self.jump_pending = true,
                &Command::Yaw(deg) => sim.yaw = deg.to_radians(),
                &Command::Turn(deg) => sim.yaw += deg.to_radians(),
                &Command::Pitch(deg) => sim.pitch = deg.to_radians(),
                Command::Track(actor) => {
                    self.tracking = actor.clone();
                    self.track_location = None;
                }
                &Command::Teleport(p) => {
                    sim.location = p;
                    sim.velocity = [0.0; 3];
                    sim.grounded = false;
                    self.goto = None;
                }
                &Command::Goto(p) => self.goto = Some(p),
                Command::Use => self.use_pending = true,
                Command::UseNamed(target) => self.use_named.push(target.clone()),
                Command::Fire => self.fire_pending = true,
                Command::Weapon(path) => self.weapons.push(path.clone()),
                &Command::SetGoal(n) => self.goals.push(n),
                Command::WaitTravel => {
                    // Stop here; the events after `wait_travel` wait for the reload.
                    self.waiting_travel = true;
                    self.cursor += 1;
                    break;
                }
                &Command::SwitchWeapon(group) => self.weapon_inputs.push(Some(group)),
                Command::NextWeapon => self.weapon_inputs.push(None),
                Command::TakeControl => self.control_pending = true,
            }
            self.cursor += 1;
        }
        if let Some(target) = self.track_location {
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
        if let Some(target) = self.goto {
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
        let script =
            Script::parse("t=1 switch_weapon 2\nt=1 next_weapon\nt=1 switch_weapon 20\n").unwrap();
        let mut drive = Drive::new(&script);
        let mut sim = PlayerSim::new([0.0; 3], 0.0);
        drive.advance(0.9, &mut sim);
        assert!(drive.take_weapon_inputs().is_empty());
        drive.advance(1.0, &mut sim);
        assert_eq!(drive.take_weapon_inputs(), [Some(2), None, Some(20)]);
        assert!(drive.take_weapon_inputs().is_empty());
        assert!(Script::parse("t=0 equip").is_err());
        assert!(Script::parse("t=0 next_weapon extra").is_err());
        for invalid in ["-1", "256", "NaN", "2 extra", ""] {
            assert!(Script::parse(&format!("t=0 switch_weapon {invalid}")).is_err());
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
    fn tracking_parses_aims_in_three_dimensions_and_off_releases_it() {
        let s = Script::parse("t=0 track Target\nt=1 track off\n").unwrap();
        assert_eq!(s.events[0].command, Command::Track(Some("Target".into())));
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
}
