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
    /// Front-end menu (`item16`): `--menu` + `--game-dir`, optional `--menu-script`.
    Menu,
    /// Bink cutscene playback: `--video FILE` + `--game-dir`.
    Video,
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
    /// User-writable checkpoint directory; defaults to the platform application-data location.
    pub save_dir: Option<PathBuf>,
    /// Load this numbered save slot before starting play.
    pub load: Option<u32>,
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
    /// Front-end menu: deterministic input script selecting entries (`--menu-script`).
    pub menu_script: Option<PathBuf>,
    /// Bink 1 cutscene to play (`--video FILE`).
    pub video: Option<PathBuf>,
    /// `--play` sound playback (`--audio off|on`, default on).
    pub audio: Audio,
    /// Particle level-start state (`--particles default|all`, default default).
    pub particles: Particles,
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

/// Particle level-start state handling.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Particles {
    /// Honour the level-start state: triggered emitters (`TrigerredEmitter`, ...) start inactive.
    #[default]
    Default,
    /// Force every emitter active (inspection of triggered effects).
    All,
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
            save_dir: None,
            load: None,
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
            menu_script: None,
            video: None,
            audio: Audio::default(),
            particles: Particles::default(),
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
         [--save-dir DIR] [--load SLOT]
         [--exit-after-secs S] [--screenshot PATH] [--size WxH]
xiii-app --menu --game-dir DIR [--menu-script FILE]
         [--exit-after-secs S] [--screenshot PATH] [--size WxH]
xiii-app --video FILE --game-dir DIR
         [--exit-after-secs S] [--screenshot PATH] [--size WxH]
xiii-app --model PKG.MESH[,PKG.MESH...] --game-dir DIR [--anim SEQ] [--frame N]
         [--exit-after-secs S] [--screenshot PATH] [--size WxH]

  --smoke              Run the native window/GPU/input/audio smoke scene (default).
  --map NAME           Diagnostic map viewer: import NAME (e.g. Plage00) from --game-dir.
  --game-dir DIR       Owned XIII installation (read-only).
  --save-dir DIR       User-writable checkpoint directory (default: app data/xiii-rustito/saves).
  --load SLOT          Restore a checkpoint save slot before starting --play.
  --play               First-person movement prototype (NOT gameplay) on --map.
  --audio off|on       Play resolved VM sound/music events (default on); `off` resolves
                       and plays nothing (still counts events in the overlay).
  --particles MODE     Particle level-start state: `default` honours the level-start state
                       (triggered emitters start inactive); `all` forces every emitter on
                       (inspection). Default `default`.
  --video FILE         Play a Bink 1 cutscene with the clean-room decoder at the file's fps.
                       Uses --game-dir for the binkw32.dll tables, or the installation
                       containing the file's Video folder when omitted. Exits after
                       --exit-after-secs and/or writes --screenshot.
  --menu               Front-end menu: load the entry map and run the game's menu classes
                       (`XIDInterf.XIIIRootWindow` / `XIIIMenu`) through the VM, draw them
                       through the Canvas path. Requires --game-dir.
  --menu-script FILE   Deterministic menu input script (headless without --screenshot).
                       Lines: `t=<secs> key <up|down|left|right|enter|escape>` |
                       `focus N` | `click N` | `open Package.Class` |
                       `newgame` (focus + Enter on the New game entry).
                       Selecting New game reaches the game's ClientTravel to Plage00 request;
                       the host then starts --play on the requested map.
   --play-script FILE   Drive --play from a text input script; headless without --screenshot.
                        Lines: `t=<secs> forward|back|right|left V | walk on/off | crouch on/off |
                        jump | yaw DEG | turn DEG | pitch DEG | use | teleport X Y Z` (teleport
                        places the box centre at Unreal-unit X,Y,Z; use is the door/mover key).
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
            "--video" => {
                opts.video = Some(PathBuf::from(value("--video")?));
                opts.mode = Mode::Video;
            }
            "--menu" => opts.mode = Mode::Menu,
            "--menu-script" => {
                opts.menu_script = Some(PathBuf::from(value("--menu-script")?));
                if opts.mode == Mode::Smoke {
                    opts.mode = Mode::Menu;
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
            "--particles" => {
                let v = value("--particles")?;
                opts.particles = match v.to_ascii_lowercase().as_str() {
                    "default" => Particles::Default,
                    "all" => Particles::All,
                    other => {
                        return Err(format!(
                            "invalid --particles {other:?}, expected default|all"
                        ));
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
            "--save-dir" => opts.save_dir = Some(PathBuf::from(value("--save-dir")?)),
            "--load" => {
                let v = value("--load")?;
                opts.load = Some(
                    v.parse()
                        .map_err(|_| format!("invalid --load slot {v:?}"))?,
                );
                opts.mode = Mode::Play;
            }
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
    fn parses_particles_flag() {
        assert_eq!(p(&[]).unwrap().particles, Particles::Default);
        assert_eq!(
            p(&["--particles", "all"]).unwrap().particles,
            Particles::All
        );
        assert_eq!(
            p(&["--particles", "Default"]).unwrap().particles,
            Particles::Default
        );
        assert!(p(&["--particles", "maybe"]).is_err());
        assert!(p(&["--particles"]).is_err());
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
    fn parses_menu_flags() {
        let o = p(&["--menu", "--game-dir", "G"]).unwrap();
        assert_eq!(o.mode, Mode::Menu);
        assert!(o.menu_script.is_none());
        let o = p(&["--menu-script", "s.txt"]).unwrap();
        assert_eq!(o.mode, Mode::Menu);
        assert_eq!(
            o.menu_script.as_deref(),
            Some(std::path::Path::new("s.txt"))
        );
        assert!(p(&["--menu-script"]).is_err());
    }

    #[test]
    fn parses_load_save_directory_options() {
        let o = p(&["--play", "--load", "3", "--save-dir", "user/saves"]).unwrap();
        assert_eq!(o.mode, Mode::Play);
        assert_eq!(o.load, Some(3));
        assert_eq!(
            o.save_dir.as_deref(),
            Some(std::path::Path::new("user/saves"))
        );
        assert!(p(&["--load", "-1"]).is_err());
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
