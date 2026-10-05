//! Minimal command-line parsing for the runtime binary.
//!
//! Kept dependency-free on purpose; replace with a real parser once the
//! runtime grows real modes (e.g. `--game-dir`, `--map`).

use std::path::PathBuf;

/// Top-level run mode. Only the diagnostic smoke scene exists today.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Native window / GPU / input / audio diagnostic scene (M0).
    Smoke,
    /// Diagnostic map viewer (M2a): `--map` + `--game-dir`.
    Viewer,
}

/// Parsed command-line options.
#[derive(Debug, Clone)]
pub struct Options {
    pub mode: Mode,
    /// Exit after this many rendered frames (unattended run).
    pub frames: Option<u32>,
    /// Exit after this many seconds of wall-clock time (unattended run).
    pub exit_after_secs: Option<f32>,
    /// Save one PNG screenshot of the primary window during an unattended run.
    pub screenshot: Option<PathBuf>,
    /// Disable vsync (useful for frame-time measurement).
    pub no_vsync: bool,
    /// Initial window size in logical pixels.
    pub width: u32,
    pub height: u32,
    /// Map name for the viewer (e.g. `Plage00`).
    pub map: Option<String>,
    /// Installation root (read-only).
    pub game_dir: Option<PathBuf>,
    /// Viewer camera override: x, y, z (Bevy metres), yaw, pitch (degrees).
    pub view: Option<[f32; 5]>,
    /// Import the map, print the counters and exit without opening a window.
    pub dump: bool,
    /// With `--dump`: list objects whose path or texture contains this text.
    pub find: Option<String>,
    /// Run the headless doorway collision test on `--map` from `--game-dir`.
    pub collision_test: bool,
}

impl Options {
    /// True when the app should exit on its own and print a report.
    pub fn unattended(&self) -> bool {
        self.frames.is_some() || self.exit_after_secs.is_some()
    }
}

impl Default for Options {
    fn default() -> Self {
        Self {
            mode: Mode::Smoke,
            frames: None,
            exit_after_secs: None,
            screenshot: None,
            no_vsync: false,
            width: 1280,
            height: 720,
            map: None,
            game_dir: None,
            view: None,
            dump: false,
            find: None,
            collision_test: false,
        }
    }
}

pub const USAGE: &str = "\
xiii-app [--smoke] [--frames N] [--exit-after-secs S] [--screenshot PATH]
         [--no-vsync] [--size WxH]
xiii-app --map NAME --game-dir DIR [--view x,y,z,yaw,pitch] [--dump]
         [--exit-after-secs S] [--screenshot PATH] [--size WxH]
xiii-app --map NAME --game-dir DIR --collision-test

  --smoke              Run the native window/GPU/input/audio smoke scene (default).
  --map NAME           Diagnostic map viewer: import NAME (e.g. Plage00) from --game-dir.
  --game-dir DIR       Owned XIII installation (read-only).
  --view x,y,z,yaw,pitch  Viewer camera override (Bevy metres, degrees).
  --dump               Viewer: import, print counters, exit without a window.
  --collision-test     Headless swept-collision doorway test on --map (no window).
  --find TEXT          With --dump: list objects whose path/texture contains TEXT.
  --frames N           Exit cleanly after N frames and print a report.
  --exit-after-secs S  Exit cleanly after S seconds and print a report.
  --screenshot PATH    Save a PNG of the window during an unattended run.
  --no-vsync           Use AutoNoVsync present mode.
  --size WxH           Initial logical window size (default 1280x720).
  -h, --help           Print this help.

Controls: WASD/QE move, Shift fast, hold right mouse to look,
          Space plays a generated tone, Esc quits.";

/// Parses `args` (excluding the program name).
pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Options, String> {
    let mut opts = Options::default();
    let mut it = args.into_iter();
    while let Some(arg) = it.next() {
        let mut value = |name: &str| it.next().ok_or_else(|| format!("{name} needs a value"));
        match arg.as_str() {
            "--smoke" => opts.mode = Mode::Smoke,
            "--frames" => {
                let v = value("--frames")?;
                let n: u32 = v.parse().map_err(|_| format!("invalid --frames {v:?}"))?;
                if n == 0 {
                    return Err("--frames must be > 0".into());
                }
                opts.frames = Some(n);
            }
            "--exit-after-secs" => {
                let v = value("--exit-after-secs")?;
                let s: f32 = v
                    .parse()
                    .map_err(|_| format!("invalid --exit-after-secs {v:?}"))?;
                if !(s.is_finite() && s > 0.0) {
                    return Err("--exit-after-secs must be a positive number".into());
                }
                opts.exit_after_secs = Some(s);
            }
            "--screenshot" => opts.screenshot = Some(PathBuf::from(value("--screenshot")?)),
            "--no-vsync" => opts.no_vsync = true,
            "--map" => {
                opts.map = Some(value("--map")?);
                opts.mode = Mode::Viewer;
            }
            "--game-dir" => opts.game_dir = Some(PathBuf::from(value("--game-dir")?)),
            "--dump" => opts.dump = true,
            "--collision-test" => opts.collision_test = true,
            "--find" => opts.find = Some(value("--find")?.to_ascii_lowercase()),
            "--view" => {
                let v = value("--view")?;
                let parts: Vec<f32> = v
                    .split(',')
                    .map(|x| x.trim().parse::<f32>())
                    .collect::<Result<_, _>>()
                    .map_err(|_| format!("invalid --view {v:?}"))?;
                let arr: [f32; 5] = parts
                    .try_into()
                    .map_err(|_| format!("--view needs 5 numbers, got {v:?}"))?;
                opts.view = Some(arr);
            }
            "--size" => {
                let v = value("--size")?;
                let (w, h) = v
                    .split_once(['x', 'X'])
                    .and_then(|(w, h)| Some((w.parse().ok()?, h.parse().ok()?)))
                    .filter(|&(w, h): &(u32, u32)| w > 0 && h > 0)
                    .ok_or_else(|| format!("invalid --size {v:?}, expected WxH"))?;
                opts.width = w;
                opts.height = h;
            }
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown argument {other:?}")),
        }
    }
    Ok(opts)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Options, String> {
        parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_are_interactive_smoke() {
        let o = p(&[]).unwrap();
        assert_eq!(o.mode, Mode::Smoke);
        assert!(!o.unattended());
    }

    #[test]
    fn parses_unattended_flags() {
        let o = p(&[
            "--frames",
            "300",
            "--size",
            "800x600",
            "--screenshot",
            "a.png",
        ])
        .unwrap();
        assert_eq!(o.frames, Some(300));
        assert_eq!((o.width, o.height), (800, 600));
        assert!(o.unattended());
        assert!(p(&["--exit-after-secs", "2.5"]).unwrap().unattended());
    }

    #[test]
    fn rejects_bad_values() {
        assert!(p(&["--frames"]).is_err());
        assert!(p(&["--frames", "0"]).is_err());
        assert!(p(&["--exit-after-secs", "-1"]).is_err());
        assert!(p(&["--size", "800"]).is_err());
        assert!(p(&["--bogus"]).is_err());
        assert!(p(&["--view", "1,2,3"]).is_err());
    }

    #[test]
    fn parses_viewer_flags() {
        let o = p(&[
            "--map",
            "Plage00",
            "--game-dir",
            "G",
            "--view",
            "1,2,3,90,-10",
        ])
        .unwrap();
        assert_eq!(o.mode, Mode::Viewer);
        assert_eq!(o.map.as_deref(), Some("Plage00"));
        assert_eq!(o.view, Some([1.0, 2.0, 3.0, 90.0, -10.0]));
    }
}
