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
}

/// Runs the touch chain on a loaded set (`map` is the map package index).
pub fn run_touch_chain(set: &ScriptSet, map: usize, cfg: &RunConfig) -> Result<RunReport, String> {
    let mut vm = Vm::new(set, cfg.limits);
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
    vm.note(TraceKind::Note(
        "PreBeginPlay/BeginPlay not run (need GameInfo/mutators, not simulated); running PostBeginPlay + SetInitialState for the executed scope".into(),
    ));
    let mut error = None;
    'run: {
        for &id in &active {
            for ev in ["PostBeginPlay", "SetInitialState"] {
                if let Err(e) = vm.send_event(id, ev, Vec::new()) {
                    error = Some(e);
                    break 'run;
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
