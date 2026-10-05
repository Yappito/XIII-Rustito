//! Text input scripts for the deterministic `--play-script` driver.
//!
//! One command per line: `t=<seconds> <command> [args]`. Blank lines and `#` comments are
//! ignored. Commands are applied in time order at the start of the fixed tick whose elapsed
//! time reaches `t`; the resulting [`Input`] drives the same [`crate::play::sim::PlayerSim`]
//! the interactive window uses.
//!
//! Supported commands:
//! - `forward <v>` / `back <v>`: set the forward axis (`back` negates `v`).
//! - `right <v>` / `left <v>`: set the strafe axis (`left` negates `v`).
//! - `walk <0|1|on|off>`: set the Shift walk modifier.
//! - `crouch <0|1|on|off>`: hold the crouch key (XIII `C=Duck`).
//! - `jump`: request one jump (edge-triggered, consumed by the next tick).
//! - `yaw <degrees>`: set absolute yaw (Unreal convention: 0 = +X, positive toward +Y).
//! - `turn <degrees>`: add to yaw.
//! - `pitch <degrees>`: set camera pitch.
//! - `use` (alias `grab`/`interact`): request one use/interact action (edge-triggered); the
//!   host runs the VM's mover lock/unlock/open chain (`Session::use_mover`).
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
    /// Move the box centre to an absolute Unreal-unit position (harness bootstrap).
    Teleport([f32; 3]),
    /// Autopilot toward an Unreal-unit waypoint (the driver re-aims and walks; not a teleport).
    Goto([f32; 3]),
    /// Request one use/interact action (edge-triggered; the VM `Grab`/use chain).
    Use,
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
                "use" | "grab" | "interact" => Command::Use,
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
    /// Weapons requested (`weapon <Package.Class>`) and not yet applied by the host.
    weapons: Vec<String>,
    /// Active `goto` waypoint (Unreal units), if any.
    goto: Option<[f32; 3]>,
    /// Set by `wait_travel`; blocks further events until the host calls [`Drive::notify_travel`].
    waiting_travel: bool,
    /// Objective numbers requested (`set_goal <N>`) and not yet applied by the host.
    goals: Vec<i32>,
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
            weapons: Vec::new(),
            goto: None,
            waiting_travel: false,
            goals: Vec::new(),
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

    /// Whether the driver is blocked on a `wait_travel` command.
    pub fn waiting_travel(&self) -> bool {
        self.waiting_travel
    }

    /// Releases a `wait_travel` block (the host observed the travel request).
    pub fn notify_travel(&mut self) {
        self.waiting_travel = false;
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
                &Command::Teleport(p) => {
                    sim.location = p;
                    sim.velocity = [0.0; 3];
                    sim.grounded = false;
                    self.goto = None;
                }
                &Command::Goto(p) => self.goto = Some(p),
                Command::Use => self.use_pending = true,
                Command::Fire => self.fire_pending = true,
                Command::Weapon(path) => self.weapons.push(path.clone()),
                &Command::SetGoal(n) => self.goals.push(n),
                Command::WaitTravel => {
                    // Stop here; the events after `wait_travel` wait for the reload.
                    self.waiting_travel = true;
                    self.cursor += 1;
                    break;
                }
            }
            self.cursor += 1;
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
    fn empty_script_is_valid() {
        let s = Script::parse("\n# nothing\n").unwrap();
        assert!(s.events.is_empty());
        assert_eq!(s.last_time(), 0.0);
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
