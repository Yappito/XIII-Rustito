//! `xiii-tool script run`: headless harness that loads a map's actors into the
//! `xiii-script` interpreter, delivers one `Touch` event and ticks the world at a fixed step,
//! recording a behaviour trace (M2c).
//!
//! The executed scope is explicit: only the touched actor and actors of the listed classes
//! run script; calls into any other actor are recorded as deferred/unsupported by the VM.
//! `PreBeginPlay`/`BeginPlay` are not run because they need a GameInfo and mutators, which
//! the harness does not simulate; `PostBeginPlay` and `SetInitialState` are.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use xiii_package::Limits;
use xiii_script::linker::GlobalRef;
use xiii_script::physics::FlatPhysics;
use xiii_script::registry::NativeStatus;
use xiii_script::{
    ObjRef, ScriptLimits, ScriptPackage, ScriptSet, TraceEvent, TraceKind, Value, Vm, VmError,
    VmLimits,
};

use crate::corpus::tagged_files;
use crate::script_cmd::{dll_exec_symbols, load_install};

/// Default classes executed besides the touched actor.
pub const DEFAULT_ACTIVE_CLASSES: &[&str] = &["TouchTrigger", "XIIIDispatcher"];

/// Harness configuration.
#[derive(Debug, Clone)]
pub struct RunConfig {
    /// Actor receiving `Touch`.
    pub touch: String,
    /// Class names (or actor names) executed besides the touched actor.
    pub active: Vec<String>,
    /// Ticks to run.
    pub ticks: u64,
    /// Fixed step in seconds.
    pub dt: f32,
    /// Tick at which the touch is delivered (before that tick's update).
    pub touch_tick: u64,
    /// Interpreter limits.
    pub limits: VmLimits,
    /// Run the level-start lifecycle (`begin_play`) before the touch.
    pub begin_play: bool,
    /// GameInfo class for `--begin-play` (`Package.Class`); overrides `default_game`.
    pub game_class: Option<String>,
    /// `Default.ini` `[Engine.Engine] DefaultGame`, filled by the CLI from the install.
    pub default_game: Option<String>,
    /// Diagnostic survey: continue past unimplemented natives, counting them.
    pub survey: bool,
    /// Diagnostic physics provider: `flat:<z>` installs an infinite floor at Unreal Z.
    pub physics_flat_z: Option<f32>,
}

impl Default for RunConfig {
    fn default() -> Self {
        Self {
            touch: "TouchTrigger2".into(),
            active: DEFAULT_ACTIVE_CLASSES
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ticks: 60,
            dt: 1.0 / 30.0,
            touch_tick: 1,
            limits: VmLimits::default(),
            begin_play: false,
            game_class: None,
            default_game: None,
            survey: false,
            physics_flat_z: None,
        }
    }
}

/// One native used during the run, with its evidence.
#[derive(Debug, Clone)]
pub struct NativeUse {
    /// `Class.Function`.
    pub path: String,
    /// Declared native index.
    pub index: Option<u16>,
    /// Calls.
    pub calls: u64,
    /// Registry status text.
    pub status: String,
    /// Registry evidence text.
    pub evidence: String,
}

/// Result of a harness run.
#[derive(Debug)]
pub struct RunReport {
    /// Trace records.
    pub trace: Vec<TraceEvent>,
    /// Executed actors.
    pub active: Vec<String>,
    /// Final state per executed actor.
    pub final_states: BTreeMap<String, Option<String>>,
    /// Natives called.
    pub natives: Vec<NativeUse>,
    /// Actors instantiated from the map.
    pub actors_loaded: usize,
    /// Properties that could not be placed while loading.
    pub load_warnings: usize,
    /// Failure, if the run stopped on an error.
    pub error: Option<VmError>,
    /// Survey mode: distinct unimplemented natives, first-hit stack included.
    pub missing_natives: Vec<xiii_script::vm::MissingNative>,
}

/// Runs the touch chain on a loaded set (`map` is the map package index).
pub fn run_touch_chain(set: &ScriptSet, map: usize, cfg: &RunConfig) -> Result<RunReport, String> {
    let mut vm = Vm::new(set, cfg.limits);
    vm.survey = cfg.survey;
    if let Some(z) = cfg.physics_flat_z {
        vm.set_physics(Box::new(FlatPhysics::new(z)));
        vm.note(TraceKind::Note(format!(
            "diagnostic physics (flat floor at Unreal Z={z}), not the map"
        )));
    }
    let actors = vm
        .load_level(map, &Limits::default())
        .map_err(|e| e.to_string())?;
    let touched = vm
        .find_object(&cfg.touch)
        .ok_or_else(|| format!("no actor named {} in the map", cfg.touch))?;
    let mut active = vec![touched];
    for &id in &actors {
        let o = &vm.objects[id as usize];
        let class_name = set.packages[o.class.package]
            .ref_name(xiii_package::ObjectRef::Export(o.class.export))
            .to_owned();
        if cfg
            .active
            .iter()
            .any(|a| a.eq_ignore_ascii_case(&class_name) || a.eq_ignore_ascii_case(&o.name))
            && !active.contains(&id)
        {
            active.push(id);
        }
    }
    for &id in &active {
        vm.set_active(id, true);
    }
    let active_names: Vec<String> = active
        .iter()
        .map(|i| vm.objects[*i as usize].name.clone())
        .collect();
    vm.note(TraceKind::Note(format!(
        "loaded {} actors; executed scope: {}",
        actors.len(),
        active_names.join(", ")
    )));
    // Synthetic player pawn as the toucher (not executed).
    let player_class =
        find_class(set, "xiii", "XIIIPlayerPawn").ok_or("class XIII.XIIIPlayerPawn not loaded")?;
    let player = vm
        .spawn(player_class, "XIIIPlayerPawn(synthetic)")
        .map_err(|e| e.to_string())?;
    let mut error = None;
    'run: {
        if cfg.begin_play {
            let game_class = match cfg.game_class.as_deref() {
                Some(path) => resolve_class_path(set, path),
                None => cfg
                    .default_game
                    .as_deref()
                    .and_then(|path| resolve_class_path(set, path)),
            };
            let Some(game_class) = game_class else {
                return Err(
                    "--begin-play needs a GameInfo class: pass --game-class <Package.Class> \
                     (Default.ini [Engine.Engine] DefaultGame was not found)"
                        .into(),
                );
            };
            vm.note(TraceKind::Note(format!(
                "--begin-play: spawning GameInfo {} and running the level-start lifecycle {} for the executed scope ({})",
                vm.short_path(game_class),
                xiii_script::vm::LEVEL_START_LIFECYCLE.join(", "),
                active_names.join(", ")
            )));
            if let Err(e) = vm.begin_play_with_game_info(&active, game_class) {
                error = Some(e);
                break 'run;
            }
        } else {
            vm.note(TraceKind::Note(
                "PreBeginPlay/BeginPlay not run (need GameInfo/mutators, not simulated); running PostBeginPlay + SetInitialState for the executed scope".into(),
            ));
            for &id in &active {
                for ev in ["PostBeginPlay", "SetInitialState"] {
                    if let Err(e) = vm.send_event(id, ev, Vec::new()) {
                        error = Some(e);
                        break 'run;
                    }
                }
            }
        }
        for t in 1..=cfg.ticks {
            if t == cfg.touch_tick {
                vm.note(TraceKind::Note(format!(
                    "harness delivers Touch(XIIIPlayerPawn(synthetic)) to {} before tick {t}",
                    cfg.touch
                )));
                let arg = Value::Object(Some(ObjRef::Instance(player)));
                if let Err(e) = vm.send_event(touched, "Touch", vec![arg]) {
                    error = Some(e);
                    break 'run;
                }
            }
            if let Err(e) = vm.tick(cfg.dt) {
                error = Some(e);
                break 'run;
            }
        }
    }
    let final_states = active
        .iter()
        .map(|i| (vm.objects[*i as usize].name.clone(), vm.state_name(*i)))
        .collect();
    let natives = vm
        .natives_used
        .iter()
        .map(|(path, (index, calls))| {
            let def = vm.registry().get(&path.to_ascii_lowercase());
            NativeUse {
                path: path.clone(),
                index: *index,
                calls: *calls,
                status: def.map_or_else(
                    || "missing".into(),
                    |d| match d.status {
                        NativeStatus::Implemented => "implemented".into(),
                        NativeStatus::Partial(why) => format!("partial: {why}"),
                    },
                ),
                evidence: def.map_or_else(String::new, |d| d.evidence.to_owned()),
            }
        })
        .collect();
    Ok(RunReport {
        trace: vm.trace.clone(),
        active: active_names,
        final_states,
        natives,
        actors_loaded: actors.len(),
        load_warnings: vm.load_warnings.len(),
        error,
        missing_natives: vm.missing_natives.values().cloned().collect(),
    })
}

fn find_class(set: &ScriptSet, package: &str, path: &str) -> Option<GlobalRef> {
    let pi = set.package_index(package)?;
    let export = set.packages[pi].export_by_path(path)?;
    Some(GlobalRef {
        package: pi,
        export,
    })
}

/// Resolves a `Package.Class` (or bare `Class`) path to a loaded class.
pub fn resolve_class_path(set: &ScriptSet, path: &str) -> Option<GlobalRef> {
    match path.split_once('.') {
        Some((package, class)) => find_class(set, package, class),
        None => (0..set.packages.len()).find_map(|pi| {
            let export = set.packages[pi].export_by_path(path)?;
            Some(GlobalRef {
                package: pi,
                export,
            })
        }),
    }
}

/// Reads `[Engine.Engine] DefaultGame` from the installation's `Default.ini`.
pub fn default_game_from_ini(root: &Path) -> Option<String> {
    for name in [
        "Default.ini",
        "default.ini",
        "System/Default.ini",
        "system/Default.ini",
    ] {
        let path = root.join(name);
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        if let Some(v) = ini_value(&text, "Engine.Engine", "DefaultGame") {
            return Some(v);
        }
    }
    None
}

fn ini_value(text: &str, section: &str, key: &str) -> Option<String> {
    let mut in_section = false;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(s) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            in_section = s.eq_ignore_ascii_case(section);
            continue;
        }
        if !in_section {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            let v = v.split(';').next().unwrap_or(v).trim();
            if !v.is_empty() {
                return Some(v.to_owned());
            }
        }
    }
    None
}

/// Finds a map file by stem under the installation (case-insensitive).
pub fn find_map(root: &Path, map: &str) -> std::io::Result<Option<PathBuf>> {
    Ok(tagged_files(root)?.into_iter().map(|(_, p)| p).find(|p| {
        p.extension().is_some_and(|e| e.eq_ignore_ascii_case("unr"))
            && p.file_stem()
                .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(map))
    }))
}

/// Loads the installation's `.u` packages plus one map; returns the set and the map index.
pub fn load_with_map(root: &Path, map: &str) -> Result<(ScriptSet, usize), String> {
    let (mut set, failures) = load_install(root).map_err(|e| e.to_string())?;
    if let Some((rel, e)) = failures.first() {
        return Err(format!("{rel}: {e}"));
    }
    let path = find_map(root, map)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| format!("map {map} not found under {}", root.display()))?;
    let data = std::fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    let pkg = ScriptPackage::load(map, data, &ScriptLimits::default(), &Limits::default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let idx = set.add(pkg);
    Ok((set, idx))
}

/// Parses a `--physics` value. Only `flat:<unreal_z>` is supported (diagnostic provider).
pub fn parse_physics(spec: &str) -> Option<f32> {
    let z = spec.strip_prefix("flat:")?;
    z.trim().parse::<f32>().ok()
}

/// `xiii-tool script run ...`.
pub fn run_cmd(args: &[String]) -> ExitCode {
    let mut cfg = RunConfig::default();
    let (mut root, mut map, mut show_trace, mut natives) =
        (None, "Plage00".to_owned(), false, true);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = || it.next().cloned();
        match a.as_str() {
            "--game-dir" => root = val().map(PathBuf::from),
            "--map" => map = val().unwrap_or_default(),
            "--touch" => cfg.touch = val().unwrap_or_default(),
            "--ticks" => cfg.ticks = val().and_then(|v| v.parse().ok()).unwrap_or(cfg.ticks),
            "--dt" => cfg.dt = val().and_then(|v| v.parse().ok()).unwrap_or(cfg.dt),
            "--touch-tick" => {
                cfg.touch_tick = val().and_then(|v| v.parse().ok()).unwrap_or(cfg.touch_tick)
            }
            "--active" => {
                cfg.active = val()
                    .unwrap_or_default()
                    .split(',')
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            }
            "--budget" => {
                cfg.limits.max_steps = val()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(cfg.limits.max_steps)
            }
            "--begin-play" => cfg.begin_play = true,
            "--game-class" => cfg.game_class = val(),
            "--survey" => cfg.survey = true,
            "--physics" => {
                let v = val().unwrap_or_default();
                match parse_physics(&v) {
                    Some(z) => cfg.physics_flat_z = Some(z),
                    None => {
                        eprintln!(
                            "error: invalid --physics '{v}'; expected flat:<unreal_z>\n\n{}",
                            crate::script_cmd::USAGE
                        );
                        return ExitCode::from(2);
                    }
                }
            }
            "--trace" => show_trace = true,
            "--no-natives" => natives = false,
            other => {
                eprintln!(
                    "error: unknown option '{other}'\n\n{}",
                    crate::script_cmd::USAGE
                );
                return ExitCode::from(2);
            }
        }
    }
    let Some(root) = root else {
        eprintln!(
            "error: run needs --game-dir <root>\n\n{}",
            crate::script_cmd::USAGE
        );
        return ExitCode::from(2);
    };
    cfg.default_game = default_game_from_ini(&root);
    if cfg.survey {
        eprintln!(
            "warning: --survey is diagnostic only: unimplemented natives are counted and \
             skipped, so the run does NOT prove the chain works"
        );
    }
    let (set, map_idx) = match load_with_map(&root, &map) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let report = match run_touch_chain(&set, map_idx, &cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let dll = dll_exec_symbols(&root.join("system")).unwrap_or_default();
    let mut out = String::new();
    let _ = writeln!(
        out,
        "script run: map {map}, touch {} at tick {}, {} ticks of {:.4}s, {} actors loaded ({} unplaced properties)",
        cfg.touch, cfg.touch_tick, cfg.ticks, cfg.dt, report.actors_loaded, report.load_warnings
    );
    if show_trace {
        for e in &report.trace {
            if !natives && matches!(e.kind, TraceKind::Native { .. }) {
                continue;
            }
            let _ = writeln!(out, "{e}");
        }
    }
    let _ = writeln!(out, "final states:");
    for (a, s) in &report.final_states {
        let _ = writeln!(out, "  {a}: {}", s.as_deref().unwrap_or("<none>"));
    }
    let _ = writeln!(out, "natives used:");
    for n in &report.natives {
        let (class, func) = n.path.split_once('.').unwrap_or(("", &n.path));
        let sym = if dll.contains_key(&(class.to_owned(), func.to_owned())) {
            "dll symbol: yes"
        } else {
            "dll symbol: no"
        };
        let idx = n.index.map(|i| format!("#{i}")).unwrap_or_default();
        let _ = writeln!(
            out,
            "  {idx:>5} {:<32} x{:<3} {} | {sym} | {}",
            n.path, n.calls, n.status, n.evidence
        );
    }
    if cfg.survey {
        let mut missing = report.missing_natives.clone();
        missing.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.path.cmp(&b.path)));
        let _ = writeln!(
            out,
            "survey (DIAGNOSTIC ONLY, not success): {} distinct unimplemented natives",
            missing.len()
        );
        for (rank, m) in missing.iter().enumerate() {
            let idx = m.index.map(|i| format!("#{i}")).unwrap_or_default();
            let site = m
                .first_stack
                .last()
                .map_or_else(|| "-".to_owned(), |s| s.function.clone());
            let off = m.first_stack.last().map_or(0, |s| s.offset);
            let _ = writeln!(
                out,
                "  {:>3}. {idx:>5} {:<38} x{:<4} first at {site} code 0x{off:04X}",
                rank + 1,
                m.path,
                m.calls
            );
        }
    }
    let code = match &report.error {
        None => ExitCode::SUCCESS,
        Some(e) => {
            let _ = writeln!(out, "{e}");
            ExitCode::from(1)
        }
    };
    use std::io::Write as _;
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(out.as_bytes())
        .and_then(|()| stdout.flush());
    code
}
