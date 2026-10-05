//! `xiii-tool script run`: headless harness that loads a map's actors into the
//! `xiii-script` interpreter, delivers one `Touch` event and ticks the world at a fixed step,
//! recording a behaviour trace (M2c).
//!
//! The executed scope is explicit: only the touched actor and actors of the listed classes
//! run script; calls into any other actor are recorded as deferred/unsupported by the VM.
//! `PreBeginPlay`/`BeginPlay` are not run because they need a GameInfo and mutators, which
//! the harness does not simulate; `PostBeginPlay` and `SetInitialState` are.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;

use xiii_package::Limits;
use xiii_script::animation::{AnimationData, FixedAnimation, SeqInfo};
use xiii_script::linker::GlobalRef;
use xiii_script::navigation::NavigationData;
use xiii_script::physics::{FlatPhysics, WorldPhysics};
use xiii_script::registry::NativeStatus;
use xiii_script::{
    ObjRef, PresentationEvent, ScriptLimits, ScriptPackage, ScriptSet, TraceEvent, TraceKind,
    Value, Vm, VmError, VmLimits,
};
use xiii_world::PackageCache;
use xiii_world::animation::MapAnimationProvider;
use xiii_world::import_map;
use xiii_world::nav_provider::MapNavigationProvider;
use xiii_world::physics::WorldPhysicsAdapter;

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
    /// Real map physics: build the map collision with `xiii-world` and install
    /// `WorldPhysicsAdapter` (mutually exclusive with `physics_flat_z`).
    pub physics_map: bool,
    /// Diagnostic animation provider: `fixed:<frames>,<rate>` gives every sequence that length.
    pub anim_fixed: Option<(u32, f32)>,
    /// Real animation: install the decoded `MeshAnimation` provider from `xiii-world`.
    pub anim_map: bool,
    /// Real navigation: decode the map's `ReachSpec` graph and install the provider.
    pub nav_map: bool,
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
            physics_map: false,
            anim_fixed: None,
            anim_map: false,
            nav_map: false,
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
    /// Presentation events drained from the VM, with the tick they were drained at.
    pub events: Vec<(u64, PresentationEvent)>,
    /// Final `Location` (Unreal units) per executed actor that has one.
    pub final_locations: BTreeMap<String, [f32; 3]>,
}

/// Runs the touch chain on a loaded set (`map` is the map package index), without map
/// providers (diagnostic `flat:`/`fixed:` modes still apply through `cfg`).
pub fn run_touch_chain(set: &ScriptSet, map: usize, cfg: &RunConfig) -> Result<RunReport, String> {
    run_touch_chain_with_providers(set, map, cfg, None, None, None)
}

/// Runs the touch chain with optional pre-built world providers (`--physics map` /
/// `--anim map` / `--nav map`). `map_physics` replaces the diagnostic flat floor; `map_animation`
/// is installed instead of / in addition to a diagnostic provider (the map provider wins);
/// `map_navigation` installs the decoded `ReachSpec` graph for the Controller pathing natives.
pub fn run_touch_chain_with_providers(
    set: &ScriptSet,
    map: usize,
    cfg: &RunConfig,
    map_physics: Option<Box<dyn WorldPhysics>>,
    map_animation: Option<Box<dyn AnimationData>>,
    map_navigation: Option<Box<dyn NavigationData>>,
) -> Result<RunReport, String> {
    let mut vm = Vm::new(set, cfg.limits);
    vm.survey = cfg.survey;
    if let Some(provider) = map_physics {
        vm.set_physics(provider);
        vm.note(TraceKind::Note(
            "map physics (decoded map collision), not the diagnostic flat floor".into(),
        ));
    } else if let Some(z) = cfg.physics_flat_z {
        vm.set_physics(Box::new(FlatPhysics::new(z)));
        vm.note(TraceKind::Note(format!(
            "diagnostic physics (flat floor at Unreal Z={z}), not the map"
        )));
    }
    if let Some(provider) = map_navigation {
        let note = format!(
            "map navigation (decoded ReachSpec graph): {} points, {} edges",
            provider.points().len(),
            provider.edges().len()
        );
        vm.set_navigation(provider);
        vm.note(TraceKind::Note(note));
    }
    if let Some(provider) = map_animation {
        vm.set_animation_data(provider);
        vm.note(TraceKind::Note(
            "map animation (decoded MeshAnimation sequences), not the diagnostic fixed provider"
                .into(),
        ));
    } else if let Some((frames, rate)) = cfg.anim_fixed {
        vm.set_animation_data(Box::new(FixedAnimation::new(frames, rate)));
        vm.note(TraceKind::Note(format!(
            "diagnostic animation (every sequence has {frames} frames at {rate} fps), not the mesh"
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
    let mut events: Vec<(u64, PresentationEvent)> = Vec::new();
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
        // Presentation events emitted during the level-start lifecycle (tick 0).
        for ev in vm.drain_events() {
            events.push((vm.tick_count, ev));
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
            let tick = vm.tick_count;
            for ev in vm.drain_events() {
                events.push((tick, ev));
            }
        }
    }
    // Drain anything emitted before the first tick (level start) or on the tick that errored.
    let tick = vm.tick_count;
    for ev in vm.drain_events() {
        events.push((tick, ev));
    }
    events.sort_by_key(|e| e.0);
    let final_states = active
        .iter()
        .map(|i| (vm.objects[*i as usize].name.clone(), vm.state_name(*i)))
        .collect();
    let final_locations = active
        .iter()
        .filter_map(|i| {
            vm.vector_prop(*i, "Location")
                .map(|l| (vm.objects[*i as usize].name.clone(), l))
        })
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
        events,
        final_locations,
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

/// `--physics` value: the real map collision or the diagnostic flat floor.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PhysicsSpec {
    /// Build the map's decoded collision (`xiii-world`).
    Map,
    /// Diagnostic infinite floor at Unreal Z.
    Flat(f32),
}

/// `--anim` value: the decoded `MeshAnimation` data or the diagnostic fixed provider.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum AnimSpec {
    /// Install the decoded map animation provider (`xiii-world`).
    Map,
    /// Diagnostic: every sequence has this many frames at this rate.
    Fixed(u32, f32),
}

/// Parses a `--physics` value: `map` or `flat:<unreal_z>`.
pub fn parse_physics(spec: &str) -> Option<PhysicsSpec> {
    if spec.eq_ignore_ascii_case("map") {
        return Some(PhysicsSpec::Map);
    }
    let z = spec.strip_prefix("flat:")?;
    z.trim().parse::<f32>().ok().map(PhysicsSpec::Flat)
}

/// Parses an `--anim` value: `map` or `fixed:<frames>,<rate>`.
pub fn parse_anim(spec: &str) -> Option<AnimSpec> {
    if spec.eq_ignore_ascii_case("map") {
        return Some(AnimSpec::Map);
    }
    let rest = spec.strip_prefix("fixed:")?;
    let (frames, rate) = rest.split_once(',')?;
    Some(AnimSpec::Fixed(
        frames.trim().parse::<u32>().ok()?,
        rate.trim().parse::<f32>().ok()?,
    ))
}

/// One animation lookup recorded for the report.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimQuery {
    /// Animation source path queried (`Package.Object`).
    pub source: String,
    /// Sequence name requested.
    pub sequence: String,
    /// `found`, `not found` or `error: ...`.
    pub outcome: String,
}

/// Wraps an `AnimationData` provider and records every lookup, so the harness can report which
/// sequences were requested and whether they were found (requirement of this task).
pub struct LoggingAnim {
    inner: Box<dyn AnimationData>,
    log: Rc<RefCell<Vec<AnimQuery>>>,
}

impl AnimationData for LoggingAnim {
    fn sequence(&mut self, source: &str, seq: &str) -> Result<Option<SeqInfo>, String> {
        let result = self.inner.sequence(source, seq);
        let outcome = match &result {
            Ok(Some(info)) => format!(
                "found ({} frames, {} fps, {} notifies)",
                info.frames,
                info.rate,
                info.notifies.len()
            ),
            Ok(None) => "not found".to_owned(),
            Err(e) => format!("error: {e}"),
        };
        self.log.borrow_mut().push(AnimQuery {
            source: source.to_owned(),
            sequence: seq.to_owned(),
            outcome,
        });
        result
    }
}

/// Optional real-map providers constructed for `--physics map` / `--anim map` / `--nav map`.
type MapProviders = (
    Option<Box<dyn WorldPhysics>>,
    Option<Box<dyn AnimationData>>,
    Option<Box<dyn NavigationData>>,
);

/// Builds the optional real-map providers for `--physics map` / `--anim map` / `--nav map`. One
/// [`PackageCache`] is opened and reused.
fn build_map_providers(
    root: &Path,
    map: &str,
    cfg: &RunConfig,
    anim_log: &Rc<RefCell<Vec<AnimQuery>>>,
) -> Result<MapProviders, String> {
    if !cfg.physics_map && !cfg.anim_map && !cfg.nav_map {
        return Ok((None, None, None));
    }
    let mut cache = PackageCache::open(root)?;
    let physics = if cfg.physics_map {
        let scene = import_map(&mut cache, map)?;
        Some(Box::new(WorldPhysicsAdapter::from_scene(&scene)) as Box<dyn WorldPhysics>)
    } else {
        None
    };
    // Navigation is decoded before the animation provider consumes the cache.
    let navigation = if cfg.nav_map {
        let provider = MapNavigationProvider::from_cache(&mut cache, map)?;
        Some(Box::new(provider) as Box<dyn NavigationData>)
    } else {
        None
    };
    let animation = if cfg.anim_map {
        let provider = MapAnimationProvider::from_cache(cache);
        Some(Box::new(LoggingAnim {
            inner: Box::new(provider),
            log: anim_log.clone(),
        }) as Box<dyn AnimationData>)
    } else {
        None
    };
    Ok((physics, animation, navigation))
}

/// `xiii-tool script run ...`.
pub fn run_cmd(args: &[String]) -> ExitCode {
    let mut cfg = RunConfig::default();
    let (mut root, mut map, mut show_trace, mut natives) =
        (None, "Plage00".to_owned(), false, true);
    let mut show_events = false;
    let mut show_positions = false;
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
                    Some(PhysicsSpec::Flat(z)) => {
                        cfg.physics_flat_z = Some(z);
                        cfg.physics_map = false;
                    }
                    Some(PhysicsSpec::Map) => {
                        cfg.physics_map = true;
                        cfg.physics_flat_z = None;
                    }
                    None => {
                        eprintln!(
                            "error: invalid --physics '{v}'; expected map or flat:<unreal_z>\n\n{}",
                            crate::script_cmd::USAGE
                        );
                        return ExitCode::from(2);
                    }
                }
            }
            "--anim" => {
                let v = val().unwrap_or_default();
                match parse_anim(&v) {
                    Some(AnimSpec::Fixed(frames, rate)) => {
                        cfg.anim_fixed = Some((frames, rate));
                        cfg.anim_map = false;
                    }
                    Some(AnimSpec::Map) => {
                        cfg.anim_map = true;
                        cfg.anim_fixed = None;
                    }
                    None => {
                        eprintln!(
                            "error: invalid --anim '{v}'; expected map or fixed:<frames>,<rate>\n\n{}",
                            crate::script_cmd::USAGE
                        );
                        return ExitCode::from(2);
                    }
                }
            }
            "--trace" => show_trace = true,
            "--events" => show_events = true,
            "--positions" => show_positions = true,
            "--nav" => {
                let v = val().unwrap_or_default();
                if v.eq_ignore_ascii_case("map") {
                    cfg.nav_map = true;
                } else {
                    eprintln!(
                        "error: invalid --nav '{v}'; expected map\n\n{}",
                        crate::script_cmd::USAGE
                    );
                    return ExitCode::from(2);
                }
            }
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
    let anim_log = Rc::new(RefCell::new(Vec::new()));
    let (map_physics, map_animation, map_navigation) =
        match build_map_providers(&root, &map, &cfg, &anim_log) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("error: {e}");
                return ExitCode::from(1);
            }
        };
    let report = match run_touch_chain_with_providers(
        &set,
        map_idx,
        &cfg,
        map_physics,
        map_animation,
        map_navigation,
    ) {
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
    if show_positions {
        let _ = writeln!(
            out,
            "final positions (UU, Unreal Z-up; --positions DIAGNOSTIC): {} actors",
            report.final_locations.len()
        );
        for (a, l) in &report.final_locations {
            let _ = writeln!(out, "  {a}: [{:.1}, {:.1}, {:.1}]", l[0], l[1], l[2]);
        }
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
    if cfg.anim_map {
        let log = anim_log.borrow();
        let mut groups: BTreeMap<(String, String, String), u64> = BTreeMap::new();
        for q in log.iter() {
            *groups
                .entry((q.source.clone(), q.sequence.clone(), q.outcome.clone()))
                .or_default() += 1;
        }
        let _ = writeln!(
            out,
            "animation lookups (DIAGNOSTIC, --anim map): {} queries, {} distinct source/sequence/outcome",
            log.len(),
            groups.len()
        );
        for ((source, sequence, outcome), calls) in &groups {
            let _ = writeln!(out, "  x{calls:<4} {source} :: {sequence} -> {outcome}");
        }
        let anim_end = report
            .trace
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::AnimEnd { .. }))
            .count();
        let notify = report
            .trace
            .iter()
            .filter(|e| matches!(e.kind, TraceKind::AnimNotify { .. }))
            .count();
        let _ = writeln!(
            out,
            "animation events: {anim_end} AnimEnd, {notify} AnimNotify"
        );
    }
    if show_events {
        let _ = writeln!(
            out,
            "presentation events (--events): {} drained, by tick",
            report.events.len()
        );
        let mut last_tick = None;
        for (tick, ev) in &report.events {
            if Some(*tick) != last_tick {
                let _ = writeln!(out, "  tick {tick}:");
                last_tick = Some(*tick);
            }
            let _ = writeln!(out, "    [{:.3}s] {ev}", ev.time());
        }
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

/// Opt-in corpus tests (`XIII_GOG_DIR`); print `SKIPPED` without it.
#[cfg(test)]
mod local_tests {
    use super::*;

    fn gog_root() -> Option<PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = PathBuf::from(root);
        let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// The Plage00 dispatcher chain with real map physics and decoded animation creates one
    /// controller per active soldier and still reaches `Fin`. Possession then runs the soldiers'
    /// game AI, which stops at the first unimplemented AI native (reported, not stubbed).
    #[test]
    fn gog_plage00_map_providers_chain_ends_in_fin() {
        let Some(path) = gog_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let (set, map_idx) = load_with_map(&path, "Plage00").expect("load Plage00");
        let cfg = RunConfig {
            begin_play: true,
            physics_map: true,
            anim_map: true,
            nav_map: true,
            default_game: default_game_from_ini(&path),
            active: ["TouchTrigger", "XIIIDispatcher", "BaseSoldier"]
                .iter()
                .map(|s| (*s).to_owned())
                .collect(),
            ..RunConfig::default()
        };
        let anim_log = Rc::new(RefCell::new(Vec::new()));
        let (physics, animation, navigation) =
            build_map_providers(&path, "Plage00", &cfg, &anim_log).expect("build providers");
        assert!(physics.is_some() && animation.is_some() && navigation.is_some());
        let report =
            run_touch_chain_with_providers(&set, map_idx, &cfg, physics, animation, navigation)
                .expect("run");
        assert_eq!(report.actors_loaded, 371);
        assert_eq!(
            report.final_states["XIIIDispatcher0"].as_deref(),
            Some("Fin")
        );
        // `Pawn.PostBeginPlay` possession spawns one `IAController` per active soldier.
        let controllers = report
            .trace
            .iter()
            .filter(|e| matches!(&e.kind, TraceKind::Spawned { class, .. } if class.ends_with("IAController")))
            .count();
        assert_eq!(controllers, 2, "one controller per active soldier");
        // The run stops at the first unimplemented AI native (not a decode/provider error).
        if let Some(e) = &report.error {
            assert!(
                matches!(e.kind, xiii_script::VmErrorKind::UnimplementedNative { .. }),
                "{e}"
            );
        }
        assert!(report.natives.iter().all(|n| n.status != "missing"));
        println!(
            "Plage00 with map providers: {} actors, {} natives, {controllers} controllers, dispatcher Fin, {} animation queries, stop: {}",
            report.actors_loaded,
            report.natives.len(),
            anim_log.borrow().len(),
            report
                .error
                .as_ref()
                .map_or("none".to_owned(), |e| match &e.kind {
                    xiii_script::VmErrorKind::UnimplementedNative { path, .. } => path.clone(),
                    other => format!("{other:?}"),
                })
        );
    }
}
