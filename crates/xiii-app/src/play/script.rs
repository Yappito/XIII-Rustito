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
}

/// Mutable playback state of a [`Script`].
pub struct Drive {
    events: Vec<Event>,
    cursor: usize,
    forward: f32,
    right: f32,
    walk: bool,
    jump_pending: bool,
    use_pending: bool,
    fire_pending: bool,
    /// Weapons requested (`weapon <Package.Class>`) and not yet applied by the host.
    weapons: Vec<String>,
    /// Active `goto` waypoint (Unreal units), if any.
    goto: Option<[f32; 3]>,
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
            jump_pending: false,
            use_pending: false,
            fire_pending: false,
            weapons: Vec::new(),
            goto: None,
        }
    }

    /// Drains the `weapon` commands due so far (the host grants them through the VM). Kept out of
    /// [`Input`] because it is not a per-tick axis and carries a class path.
    pub fn take_weapons(&mut self) -> Vec<String> {
        std::mem::take(&mut self.weapons)
    }

    /// Applies every event due at or before `elapsed` and returns this tick's input.
    ///
    /// Yaw/pitch commands are applied directly to `sim` (they are orientation, not an axis).
    pub fn advance(&mut self, elapsed: f32, sim: &mut PlayerSim) -> Input {
        while self.cursor < self.events.len() && self.events[self.cursor].t <= elapsed + 1e-6 {
            match &self.events[self.cursor].command {
                &Command::Forward(v) => self.forward = v,
                &Command::Right(v) => self.right = v,
                &Command::Walk(v) => self.walk = v,
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
    fn empty_script_is_valid() {
        let s = Script::parse("\n# nothing\n").unwrap();
        assert!(s.events.is_empty());
        assert_eq!(s.last_time(), 0.0);
    }
}
