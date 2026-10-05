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
    /// Diagnostic skinned-character viewer (M2b): `--model` + `--game-dir`.
    Skinned,
    /// First-person movement prototype: `--play` + `--map` + `--game-dir`.
    Play,
}

/// Viewer baked-lighting mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Lighting {
    /// Ignore the baked per-vertex colours (diagnostic unlit texture comparison).
    Off,
    /// Multiply textures by the decoded baked per-vertex colours (default).
    #[default]
    Baked,
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
    /// Skinned viewer: `Package.Mesh` (comma-separated for several side by side).
    pub model: Option<String>,
    /// Skinned viewer: animation sequence name to play (default: bind pose).
    pub anim: Option<String>,
    /// Skinned viewer: freeze the pose at this clip frame (comparisons).
    pub frame: Option<f32>,
    /// Run the headless ReachSpec navigation walk test on `--map` from `--game-dir`.
    pub reach_test: bool,
    /// Baked vertex-colour modulation (`--lighting off|baked`).
    pub lighting: Lighting,
    /// Play prototype: deterministic input script (headless without `--screenshot`).
    pub play_script: Option<PathBuf>,
    /// `--play` sound playback (`--audio off|on`, default on).
    pub audio: Audio,
    /// Print a per-system performance table every [`Options::perf_interval`] seconds and at exit.
    pub perf: bool,
    /// Seconds between `--perf` tables (default 5; the final table is always printed).
    pub perf_interval: f32,
    /// `--perf`: also time individual VM natives (adds overhead; top 10 reported).
    pub perf_natives: bool,
}

/// Audio playback toggle for `--play`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Audio {
    /// Do not resolve or play VM sound/music events.
    Off,
    /// Resolve sound events from the installation and play them (default).
    #[default]
    On,
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
            model: None,
            anim: None,
            frame: None,
            reach_test: false,
            lighting: Lighting::default(),
            play_script: None,
            audio: Audio::default(),
            perf: false,
            perf_interval: 5.0,
            perf_natives: false,
        }
    }
}

pub const USAGE: &str = "\
xiii-app [--smoke] [--frames N] [--exit-after-secs S] [--screenshot PATH]
         [--no-vsync] [--size WxH]
xiii-app --map NAME --game-dir DIR [--view x,y,z,yaw,pitch] [--dump]
         [--exit-after-secs S] [--screenshot PATH] [--size WxH] [--lighting off|baked]
xiii-app --map NAME --game-dir DIR --collision-test
xiii-app --map NAME --game-dir DIR --play [--play-script FILE] [--audio off|on]
         [--exit-after-secs S] [--screenshot PATH] [--size WxH]
xiii-app --model PKG.MESH[,PKG.MESH...] --game-dir DIR [--anim SEQ] [--frame N]
         [--exit-after-secs S] [--screenshot PATH] [--size WxH]

  --smoke              Run the native window/GPU/input/audio smoke scene (default).
  --map NAME           Diagnostic map viewer: import NAME (e.g. Plage00) from --game-dir.
  --game-dir DIR       Owned XIII installation (read-only).
  --play               First-person movement prototype (NOT gameplay) on --map.
  --audio off|on       Play resolved VM sound/music events (default on); `off` resolves
                       and plays nothing (still counts events in the overlay).
  --play-script FILE   Drive --play from a text input script; headless without --screenshot.
                       Lines: `t=<secs> forward|back|right|left V | walk on/off | jump |
                       yaw DEG | turn DEG | pitch DEG | use | teleport X Y Z` (teleport places
                       the box centre at Unreal-unit X,Y,Z; use is the door/mover interact key).
  --model PKG.MESH     Skinned-character viewer: decode a SkeletalMesh; several comma-
                       separated entries are placed side by side.
  --anim SEQ           Skinned viewer: play MeshAnimation sequence SEQ (default bind pose).
  --frame N            Skinned viewer: freeze the clip at frame N and the camera front-on.
  --view x,y,z,yaw,pitch  Viewer camera override (Bevy metres, degrees).
  --dump               Viewer: import, print counters, exit without a window.
  --collision-test     Headless swept-collision doorway test on --map (no window).
  --reach-test         Headless ReachSpec navigation walk test on --map (no window).
  --lighting MODE      Viewer: modulate textures by the baked per-vertex colours
                       (`baked`, default) or ignore them (`off`) for comparison.
  --find TEXT          With --dump: list objects whose path/texture contains TEXT.
  --frames N           Exit cleanly after N frames and print a report.
  --exit-after-secs S  Exit cleanly after S seconds and print a report.
  --screenshot PATH    Save a PNG of the window during an unattended run.
  --no-vsync           Use AutoNoVsync present mode.
  --perf               Print a per-system frame-time/CPU table every 5 s and at exit.
  --perf-interval S    Seconds between --perf tables (default 5).
  --perf-natives       --perf: also time individual VM natives (adds overhead).
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
            "--perf" => {
                opts.perf = true;
                if opts.mode == Mode::Smoke {
                    opts.mode = Mode::Viewer;
                }
            }
            "--perf-natives" => {
                opts.perf = true;
                opts.perf_natives = true;
                if opts.mode == Mode::Smoke {
                    opts.mode = Mode::Viewer;
                }
            }
            "--perf-interval" => {
                let v = value("--perf-interval")?;
                let s: f32 = v
                    .parse()
                    .map_err(|_| format!("invalid --perf-interval {v:?}"))?;
                if !(s.is_finite() && s > 0.0) {
                    return Err("--perf-interval must be a positive number".into());
                }
                opts.perf_interval = s;
            }
            "--play" => opts.mode = Mode::Play,
            "--play-script" => {
                opts.play_script = Some(PathBuf::from(value("--play-script")?));
                if opts.mode == Mode::Smoke {
                    opts.mode = Mode::Play;
                }
            }
            "--audio" => {
                let v = value("--audio")?;
                opts.audio = match v.to_ascii_lowercase().as_str() {
                    "off" => Audio::Off,
                    "on" => Audio::On,
                    other => {
                        return Err(format!("invalid --audio {other:?}, expected off|on"));
                    }
                };
            }
            "--map" => {
                opts.map = Some(value("--map")?);
                if opts.mode == Mode::Smoke {
                    opts.mode = Mode::Viewer;
                }
            }
            "--game-dir" => opts.game_dir = Some(PathBuf::from(value("--game-dir")?)),
            "--dump" => opts.dump = true,
            "--collision-test" => opts.collision_test = true,
            "--model" => {
                opts.model = Some(value("--model")?);
                opts.mode = Mode::Skinned;
            }
            "--anim" => opts.anim = Some(value("--anim")?),
            "--frame" => {
                let v = value("--frame")?;
                let n: f32 = v.parse().map_err(|_| format!("invalid --frame {v:?}"))?;
                if !(n.is_finite() && n >= 0.0) {
                    return Err("--frame must be a finite number >= 0".into());
                }
                opts.frame = Some(n);
            }
            "--reach-test" => opts.reach_test = true,
            "--lighting" => {
                let v = value("--lighting")?;
                opts.lighting = match v.to_ascii_lowercase().as_str() {
                    "off" => Lighting::Off,
                    "baked" => Lighting::Baked,
                    other => {
                        return Err(format!("invalid --lighting {other:?}, expected off|baked"));
                    }
                };
            }
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
    fn parses_audio_flag() {
        assert_eq!(p(&[]).unwrap().audio, Audio::On);
        assert_eq!(p(&["--audio", "off"]).unwrap().audio, Audio::Off);
        assert_eq!(p(&["--audio", "ON"]).unwrap().audio, Audio::On);
        assert!(p(&["--audio", "maybe"]).is_err());
        assert!(p(&["--audio"]).is_err());
    }

    #[test]
    fn parses_skinned_flags() {
        let o = p(&[
            "--model",
            "xiiipersos.XIIIM,XiiiPersos.MigA",
            "--game-dir",
            "G",
            "--anim",
            "Walk",
            "--frame",
            "7.5",
        ])
        .unwrap();
        assert_eq!(o.mode, Mode::Skinned);
        assert_eq!(o.model.as_deref(), Some("xiiipersos.XIIIM,XiiiPersos.MigA"));
        assert_eq!(o.anim.as_deref(), Some("Walk"));
        assert_eq!(o.frame, Some(7.5));
        // --anim/--frame alone stay in the default mode.
        assert_eq!(p(&["--anim", "Walk"]).unwrap().mode, Mode::Smoke);
        assert!(p(&["--frame", "-1"]).is_err());
        assert!(p(&["--frame", "x"]).is_err());
    }

    #[test]
    fn parses_perf_flags() {
        let o = p(&["--map", "Plage00", "--perf"]).unwrap();
        assert!(o.perf);
        assert!(!o.perf_natives);
        assert_eq!(o.perf_interval, 5.0);
        let o = p(&["--play", "--perf-natives", "--perf-interval", "2.5"]).unwrap();
        assert!(o.perf && o.perf_natives);
        assert_eq!(o.perf_interval, 2.5);
        assert_eq!(o.mode, Mode::Play);
        assert!(p(&["--perf-interval", "0"]).is_err());
        assert!(p(&["--perf-interval"]).is_err());
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
        // Baked is the default; `off` and `baked` both parse, anything else is rejected.
        assert_eq!(o.lighting, Lighting::Baked);
        assert_eq!(p(&["--lighting", "off"]).unwrap().lighting, Lighting::Off);
        assert_eq!(
            p(&["--lighting", "Baked"]).unwrap().lighting,
            Lighting::Baked
        );
        assert!(p(&["--lighting", "maybe"]).is_err());
    }
}
