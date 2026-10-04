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
        }
    }
}

pub const USAGE: &str = "\
xiii-app [--smoke] [--frames N] [--exit-after-secs S] [--screenshot PATH]
         [--no-vsync] [--size WxH]

  --smoke              Run the native window/GPU/input/audio smoke scene (default).
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
    }
}
