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

use std::path::Path;

use crate::play::sim::{Input, PlayerSim};

/// One parsed command.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Event {
    /// Trigger time in seconds.
    pub t: f32,
    /// The command.
    pub command: Command,
}

/// A parsed script command.
#[derive(Debug, Clone, Copy, PartialEq)]
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
        }
    }

    /// Applies every event due at or before `elapsed` and returns this tick's input.
    ///
    /// Yaw/pitch commands are applied directly to `sim` (they are orientation, not an axis).
    pub fn advance(&mut self, elapsed: f32, sim: &mut PlayerSim) -> Input {
        while self.cursor < self.events.len() && self.events[self.cursor].t <= elapsed + 1e-6 {
            match self.events[self.cursor].command {
                Command::Forward(v) => self.forward = v,
                Command::Right(v) => self.right = v,
                Command::Walk(v) => self.walk = v,
                Command::Jump => self.jump_pending = true,
                Command::Yaw(deg) => sim.yaw = deg.to_radians(),
                Command::Turn(deg) => sim.yaw += deg.to_radians(),
                Command::Pitch(deg) => sim.pitch = deg.to_radians(),
            }
            self.cursor += 1;
        }
        let jump = std::mem::take(&mut self.jump_pending);
        Input {
            forward: self.forward,
            right: self.right,
            jump,
            walk: self.walk,
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
