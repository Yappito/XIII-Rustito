//! `xiii-tool campaign`: whole-campaign headless sweep.
//!
//! Imports every campaign map without Bevy and measures, per map: import counters and time;
//! collision soup sizes and BVH build time; navigation decode counts; the ReachSpec reach-walk
//! results; and a full-actor `--begin-play --survey` script run against the real map providers.
//! Each map runs isolated: a panic, a decode error or a script abort is recorded and the sweep
//! continues. Nothing from the game is written; the `--json` report is metadata only.
//!
//! The campaign order is found with evidence: the map `MapInfo` actor carries a
//! `NextMapLevelWithUnr` string (a tagged property) that links each mission to the next, and the
//! `XIDMaps.MapNN_*` classes confirm the chapter naming. If no order is found, all maps under
//! `Maps/` except multiplayer ones (name-prefix classification) are used, and that is stated.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::rc::Rc;
use std::time::Instant;

use serde_json::{Value, json};

use xiii_collision::CollisionWorld;
use xiii_install::{Installation, OpenOptions, PackageKind};
use xiii_package::{Limits, Package, PropertyValue};
use xiii_script::vm::MissingNative;
use xiii_script::{PresentationEvent, Vm, VmErrorKind, VmLimits};
use xiii_world::reach::{self, ReachReport};
use xiii_world::runtime::{self, ProviderSpec};
use xiii_world::{ClassDefaults, PackageCache, WorldScene, import_map};

/// Ticks run in the script survey after the level-start lifecycle.
const SURVEY_TICKS: u64 = 30;
/// Fixed step for the script survey.
const SURVEY_DT: f32 = 1.0 / 30.0;

/// Usage text for `xiii-tool campaign`.
pub const USAGE: &str = "\
xiii-tool campaign (<game-dir> | --root-env <VAR>) [--maps a,b,..] [--json <out>] [--md <out>]
    Headless sweep of the whole campaign. Finds the campaign order from each map's
    MapInfo.NextMapLevelWithUnr link (falling back to every non-multiplayer map), then
    per map, isolated: imports it (counters/time), builds the box and line collision
    soups (sizes/BVH time), decodes navigation, runs the ReachSpec reach-walk, and runs
    the script level-start lifecycle with ALL actors active in --survey mode against the
    real physics/animation/navigation providers. A panic or error in one map is recorded
    and the sweep continues.
    --root-env <VAR>  take the installation root from environment variable VAR, so a
                      protected path never has to be typed on the command line
                      (e.g. --root-env XIII_STEAM_DIR). Cannot be combined with <game-dir>.
    --maps a,b,..  explicit map list (case-insensitive), overriding discovery.
    --json <out>   write a metadata-only JSON report (refused inside <game-dir>).
    --md <out>     write the Markdown summary (refused inside <game-dir>).
    Nothing containing game bytes is written; only counters, names and timings.";

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn emit(text: &str) {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let _ = out.write_all(text.as_bytes()).and_then(|()| out.flush());
}

/// True when `out` would be written inside `root` (case-insensitive, canonicalized parents).
fn inside(out: &Path, root: &Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let lower = |p: &Path| p.to_string_lossy().to_lowercase();
    lower(&parent).starts_with(&lower(&root))
}

// -------------------------------------------------------------------------------------------
// Map discovery (evidence-based campaign order)
// -------------------------------------------------------------------------------------------

/// How the map list was found.
#[derive(Debug, Clone)]
pub struct MapDiscovery {
    /// Map file stems, in sweep order.
    pub maps: Vec<String>,
    /// Short label of the method.
    pub source: String,
    /// Human-readable evidence/classification detail.
    pub detail: String,
}

/// Result of `xiii-tool campaign`.
pub struct CampaignReport {
    /// Installation root.
    pub root: String,
    /// How the map list was found.
    pub discovery: MapDiscovery,
    /// Per-map results, in sweep order.
    pub maps: Vec<MapResult>,
    /// Campaign-wide aggregates.
    pub aggregate: Aggregate,
}

/// Status of one map's sweep.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MapStatus {
    /// Every measurement completed.
    Ok,
    /// A decode/provider/script error was recorded; other maps continued.
    Error,
    /// A panic was caught; other maps continued.
    Panic,
}

impl MapStatus {
    fn name(self) -> &'static str {
        match self {
            MapStatus::Ok => "ok",
            MapStatus::Error => "error",
            MapStatus::Panic => "panic",
        }
    }
}

/// One failed/suspended actor's error, grouped by error kind.
#[derive(Debug, Clone)]
pub struct SuspendedActor {
    /// Actor display name.
    pub actor: String,
    /// Error kind name (`UnimplementedNative`, `Unresolved`, ...).
    pub kind: String,
    /// Full error text (metadata only; no game bytes).
    pub error: String,
}

/// One missing native from the survey, with its first-hit site.
#[derive(Debug, Clone)]
pub struct MissingNativeRow {
    /// `Class.Function` (`#index` when unregistered).
    pub path: String,
    /// Declared native index, when any.
    pub index: Option<u16>,
    /// Calls seen.
    pub calls: u64,
    /// Function that first called it.
    pub first_site: String,
    /// Actor that first called it.
    pub first_actor: String,
    /// Bytecode offset of the first call.
    pub first_offset: u32,
}

/// Reach-walk summary for one map.
#[derive(Debug, Clone, Default)]
pub struct ReachSummary {
    /// Navigation points.
    pub nav_points: usize,
    /// Total edges.
    pub edges: usize,
    /// Edges requiring walking.
    pub walking_edges: usize,
    /// Walking edges wide/tall enough for the player.
    pub eligible: usize,
    /// Eligible edges that passed.
    pub passes: usize,
    /// Eligible edges that failed.
    pub failures: usize,
    /// Edges whose start placement failed.
    pub spawn_failures: usize,
    /// Failure count per heuristic cause.
    pub groups: BTreeMap<String, usize>,
}

/// Script survey summary for one map.
#[derive(Debug, Clone, Default)]
pub struct ScriptSummary {
    /// Actors instantiated from the map.
    pub actors_loaded: usize,
    /// Properties the loader could not place.
    pub load_warnings: usize,
    /// Whether a GameInfo was found and begin-play ran.
    pub begin_play: bool,
    /// Missing natives with first-hit location.
    pub missing_natives: Vec<MissingNativeRow>,
    /// Suspended actors with their error kind.
    pub suspended: Vec<SuspendedActor>,
    /// Suspended actor count per error kind.
    pub suspended_by_kind: BTreeMap<String, usize>,
    /// Presentation events drained, total.
    pub events_total: usize,
    /// Presentation events by kind.
    pub events_by_kind: BTreeMap<String, usize>,
    /// Natives called, by registry status.
    pub natives_used: BTreeMap<String, usize>,
    /// A run-level error (e.g. map load failed), if any.
    pub error: Option<String>,
}

/// All measurements for one map.
#[derive(Debug, Clone)]
pub struct MapResult {
    /// Map name.
    pub map: String,
    /// Overall status.
    pub status: MapStatus,
    /// First error that stopped this map (if any).
    pub error: Option<String>,
    /// Total wall time for this map's sweep, milliseconds.
    pub total_ms: f64,
    // import
    /// Import time, milliseconds.
    pub import_ms: f64,
    /// Decoded object count.
    pub objects: usize,
    /// Decoded mesh count.
    pub meshes: usize,
    /// Decoded texture count.
    pub textures: usize,
    /// Every importer counter.
    pub counters: BTreeMap<String, usize>,
    /// Sum of `fail.*` counters.
    pub import_fail: usize,
    /// Sum of `skip.*` counters.
    pub import_skip: usize,
    /// Sum of every `fail.*`+`skip.*` counter.
    pub import_problem: usize,
    /// First example per problem counter.
    pub examples: BTreeMap<String, String>,
    // collision
    /// Collision triangles built into the box world.
    pub collision_tris: usize,
    /// Box soup entries (before degenerate drops).
    pub collision_box_entries: usize,
    /// Line soup entries (before degenerate drops).
    pub collision_line_entries: usize,
    /// Box BVH build time, milliseconds.
    pub box_bvh_ms: f64,
    /// Box BVH node count.
    pub box_bvh_nodes: usize,
    /// Box world degenerate triangles dropped.
    pub box_degenerate: usize,
    /// Line BVH build time, milliseconds.
    pub line_bvh_ms: f64,
    /// Line BVH node count.
    pub line_bvh_nodes: usize,
    // navigation
    /// Navigation points.
    pub nav_points: usize,
    /// Navigation edges.
    pub nav_edges: usize,
    /// Navigation exports whose property block failed.
    pub nav_property_failures: usize,
    /// Navigation exports whose PathList tail failed.
    pub nav_tail_failures: usize,
    /// Navigation-looking exports with an unresolvable class.
    pub nav_unresolved_class: usize,
    /// Edges whose End did not resolve to a navigation point.
    pub nav_unresolved_end_edges: usize,
    // reach
    /// Reach-walk summary, when it ran.
    pub reach: Option<ReachSummary>,
    /// Reach-walk error, when it did not run.
    pub reach_error: Option<String>,
    // script
    /// Script survey summary, when it ran.
    pub script: Option<ScriptSummary>,
    /// Script survey error, when it did not run.
    pub script_error: Option<String>,
}

impl MapResult {
    fn new(map: &str) -> Self {
        Self {
            map: map.to_owned(),
            status: MapStatus::Ok,
            error: None,
            total_ms: 0.0,
            import_ms: 0.0,
            objects: 0,
            meshes: 0,
            textures: 0,
            counters: BTreeMap::new(),
            import_fail: 0,
            import_skip: 0,
            import_problem: 0,
            examples: BTreeMap::new(),
            collision_tris: 0,
            collision_box_entries: 0,
            collision_line_entries: 0,
            box_bvh_ms: 0.0,
            box_bvh_nodes: 0,
            box_degenerate: 0,
            line_bvh_ms: 0.0,
            line_bvh_nodes: 0,
            nav_points: 0,
            nav_edges: 0,
            nav_property_failures: 0,
            nav_tail_failures: 0,
            nav_unresolved_class: 0,
            nav_unresolved_end_edges: 0,
            reach: None,
            reach_error: None,
            script: None,
            script_error: None,
        }
    }
}

fn stem_lower(s: &str) -> String {
    let s = s.rsplit(['/', '\\']).next().unwrap_or(s);
    let s = s
        .strip_suffix(".unr")
        .or_else(|| s.strip_suffix(".UNR"))
        .unwrap_or(s);
    s.to_ascii_lowercase()
}

/// Every map package of an installation (exact stem, path), sorted case-insensitively.
fn map_entries(root: &Path) -> Result<Vec<(String, PathBuf)>, String> {
    let install = Installation::open(root, &OpenOptions::default()).map_err(|e| e.to_string())?;
    let mut v: Vec<(String, PathBuf)> = install
        .packages()
        .filter(|e| e.kind == PackageKind::Map)
        .map(|e| (e.name.clone(), e.path.clone()))
        .collect();
    v.sort_by_key(|a| a.0.to_ascii_lowercase());
    Ok(v)
}

/// Reads a map package's `NextMapLevelWithUnr` tagged-property string, if present. Scans every
/// export's property block (only the map's `MapInfo` actor carries it).
fn read_next_map(path: &Path) -> Result<Option<String>, String> {
    let data = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let pkg = Package::parse(&data, &Limits::default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    for i in 0..pkg.exports().len() {
        if pkg.exports()[i].serial_size == 0 {
            continue;
        }
        let Ok(op) = pkg.read_object_properties(&data, i, &Limits::default()) else {
            continue;
        };
        for p in &op.block.properties {
            if pkg
                .property_name(p)
                .eq_ignore_ascii_case("NextMapLevelWithUnr")
                && let PropertyValue::Str(s) = &p.value
                && !s.is_empty()
            {
                return Ok(Some(s.clone()));
            }
        }
    }
    Ok(None)
}

/// Builds the ordered map chain starting at `start` (a lower-case stem), following the
/// `NextMapLevelWithUnr` links. Stops at a missing/repeated link.
fn chain_from(
    start: &str,
    exact_by_stem: &BTreeMap<String, String>,
    next_by_stem: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut chain = Vec::new();
    let mut seen = BTreeSet::new();
    let mut cur = start.to_owned();
    loop {
        if !seen.insert(cur.clone()) {
            break;
        }
        let Some(exact) = exact_by_stem.get(&cur) else {
            break;
        };
        chain.push(exact.clone());
        let Some(raw) = next_by_stem.get(&cur) else {
            break;
        };
        let nstem = stem_lower(raw);
        if !exact_by_stem.contains_key(&nstem) {
            break;
        }
        cur = nstem;
    }
    chain
}

/// Multiplayer/utility map name prefixes and exact stems excluded when no order is found.
fn is_multiplayer_map(stem: &str) -> bool {
    let s = stem.to_ascii_lowercase();
    if ["dm_", "ctf_", "sb_"].iter().any(|p| s.starts_with(p)) {
        return true;
    }
    matches!(
        s.as_str(),
        "entry" | "empty" | "mapmenu" | "mapcredits" | "dm_testpath" | "credits"
    )
}

/// Discovers the campaign map order, with evidence.
pub fn discover_maps(root: &Path, explicit: Option<&str>) -> Result<MapDiscovery, String> {
    let entries = map_entries(root)?;
    let exact_by_stem: BTreeMap<String, String> = entries
        .iter()
        .map(|(n, _)| (stem_lower(n), n.clone()))
        .collect();

    if let Some(list) = explicit {
        let mut maps = Vec::new();
        for part in list.split(',') {
            let part = part.trim();
            if part.is_empty() {
                continue;
            }
            let stem = stem_lower(part);
            let Some(exact) = exact_by_stem.get(&stem) else {
                return Err(format!(
                    "--maps {part:?} is not a map in {}",
                    root.display()
                ));
            };
            maps.push(exact.clone());
        }
        if maps.is_empty() {
            return Err("--maps listed no map".into());
        }
        let n = maps.len();
        return Ok(MapDiscovery {
            maps,
            source: "explicit --maps".into(),
            detail: format!("{n} maps given on the command line"),
        });
    }

    // Read every map's NextMapLevelWithUnr link.
    let mut next_by_stem: BTreeMap<String, String> = BTreeMap::new();
    let mut read_errors = 0usize;
    for (name, path) in &entries {
        match read_next_map(path) {
            Ok(Some(n)) => {
                next_by_stem.insert(stem_lower(name), n);
            }
            Ok(None) => {}
            Err(_) => read_errors += 1,
        }
    }

    if !next_by_stem.is_empty() {
        let target_stems: BTreeSet<String> = next_by_stem.values().map(|n| stem_lower(n)).collect();
        let sources: Vec<String> = next_by_stem
            .keys()
            .filter(|k| !target_stems.contains(*k))
            .cloned()
            .collect();
        // Choose the source whose chain is longest (a campaign has one start; if the data ever
        // had several, the longest is the campaign and the rest are reported in the detail).
        let mut candidates: Vec<String> = if sources.is_empty() {
            next_by_stem.keys().cloned().collect()
        } else {
            sources.clone()
        };
        candidates.sort();
        let mut best: Vec<String> = Vec::new();
        for c in &candidates {
            let chain = chain_from(c, &exact_by_stem, &next_by_stem);
            if chain.len() > best.len() {
                best = chain;
            }
        }
        if !best.is_empty() {
            let start = exact_by_stem
                .get(&stem_lower(&best[0]))
                .cloned()
                .unwrap_or_else(|| best[0].clone());
            let mut detail = format!(
                "NextMapLevelWithUnr chain: start {start} ({} map(s) have a next link and are not a target{}); {} maps; Map01_Plage..Map17_Epilogue chapter classes confirm the naming",
                sources.len(),
                if sources.len() == 1 {
                    ""
                } else {
                    "; longest chain chosen"
                },
                best.len()
            );
            if read_errors > 0 {
                let _ = write!(detail, "; {read_errors} map(s) could not be read for links");
            }
            return Ok(MapDiscovery {
                maps: best,
                source: "NextMapLevelWithUnr chain (map MapInfo actor)".into(),
                detail,
            });
        }
    }

    // No order found: every map except multiplayer/utility ones, by name-prefix classification.
    let maps: Vec<String> = entries
        .iter()
        .filter(|(n, _)| !is_multiplayer_map(n))
        .map(|(n, _)| n.clone())
        .collect();
    Ok(MapDiscovery {
        source: "no campaign order found; all non-multiplayer maps".into(),
        detail: format!(
            "no map carries NextMapLevelWithUnr; classified multiplayer/utility by filename prefix dm_/ctf_/sb_ and the exact stems entry/empty/mapmenu/mapcredits/dm_testpath; {} of {} maps kept",
            maps.len(),
            entries.len()
        ),
        maps,
    })
}

// -------------------------------------------------------------------------------------------
// Per-map measurement
// -------------------------------------------------------------------------------------------

/// Runs one map's whole sweep, catching panics. The map is isolated: a failure is recorded.
pub fn run_one(root: &Path, map: &str) -> MapResult {
    let mut result = MapResult::new(map);
    let started = Instant::now();
    let outcome =
        std::panic::catch_unwind(AssertUnwindSafe(|| measure_map(root, map, &mut result)));
    match outcome {
        Ok(Ok(())) => result.status = MapStatus::Ok,
        Ok(Err(e)) => {
            result.status = MapStatus::Error;
            if result.error.is_none() {
                result.error = Some(e);
            }
        }
        Err(payload) => {
            result.status = MapStatus::Panic;
            result.error = Some(panic_text(payload));
        }
    }
    result.total_ms = started.elapsed().as_secs_f64() * 1000.0;
    result
}

fn panic_text(payload: Box<dyn std::any::Any + Send>) -> String {
    let msg = payload
        .downcast_ref::<&str>()
        .map(|s| (*s).to_owned())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "<non-string panic payload>".to_owned());
    format!("panic: {msg}")
}

/// Measures one map. The world is imported once and shared with the reach-walk; the script
/// survey runs afterwards. Returns `Err` only for a failure before any measurement could run.
fn measure_map(root: &Path, map: &str, r: &mut MapResult) -> Result<(), String> {
    // ---- import -------------------------------------------------------------------------
    let import_started = Instant::now();
    let mut cache = PackageCache::open(root)?;
    let scene: WorldScene = import_map(&mut cache, map)?;
    r.import_ms = import_started.elapsed().as_secs_f64() * 1000.0;
    r.objects = scene.objects.len();
    r.meshes = scene.meshes.len();
    r.textures = scene.textures.len();
    for (k, v) in &scene.counters {
        r.counters.insert(k.clone(), *v);
        if k.starts_with("fail.") {
            r.import_fail += v;
        }
        if k.starts_with("skip.") {
            r.import_skip += v;
        }
    }
    r.import_problem = r.import_fail + r.import_skip;
    for (k, e) in &scene.examples {
        if k.starts_with("fail.") || k.starts_with("skip.") {
            r.examples.insert(k.clone(), e.clone());
        }
    }

    // ---- collision soups and BVH build time ---------------------------------------------
    r.collision_box_entries = scene.collision_box.len();
    r.collision_line_entries = scene.collision_line.len();
    let box_started = Instant::now();
    let box_world = CollisionWorld::new(scene.box_collision());
    r.box_bvh_ms = box_started.elapsed().as_secs_f64() * 1000.0;
    r.box_bvh_nodes = box_world.bvh_node_count();
    r.box_degenerate = box_world.degenerate_count();
    r.collision_tris = box_world.triangle_count();
    let line_started = Instant::now();
    let line_world = CollisionWorld::new(scene.line_collision());
    r.line_bvh_ms = line_started.elapsed().as_secs_f64() * 1000.0;
    r.line_bvh_nodes = line_world.bvh_node_count();

    // ---- navigation ---------------------------------------------------------------------
    let mut defaults = ClassDefaults::open(root)?;
    let nav = xiii_world::navigation::decode_navigation(&mut cache, &mut defaults, map)?;
    r.nav_points = nav.points.len();
    r.nav_edges = nav.edges.len();
    r.nav_property_failures = nav.property_failures.len();
    r.nav_tail_failures = nav.tail_failures.len();
    r.nav_unresolved_class = nav.unresolved_class.len();
    r.nav_unresolved_end_edges = nav.unresolved_end_edges;

    // ---- reach walk (reuses the imported scene and decoded navigation) -------------------
    match reach::analyze_with(map, root, Some(&scene), Some(&nav)) {
        Ok(rep) => r.reach = Some(reach_summary(&rep)),
        Err(e) => r.reach_error = Some(e),
    }

    // ---- script survey ------------------------------------------------------------------
    match survey_script(root, map) {
        Ok(s) => r.script = Some(s),
        Err(e) => r.script_error = Some(e),
    }
    Ok(())
}

fn reach_summary(rep: &ReachReport) -> ReachSummary {
    ReachSummary {
        nav_points: rep.nav_points,
        edges: rep.edges,
        walking_edges: rep.walking_edges,
        eligible: rep.eligible,
        passes: rep.passes,
        failures: rep.failures.len(),
        spawn_failures: rep.spawn_failures,
        groups: rep.groups.clone(),
    }
}

/// Natives called into the VM, mapped to their registry status.
fn native_status(vm: &Vm) -> BTreeMap<String, usize> {
    let mut out = BTreeMap::new();
    for (path, (_, calls)) in &vm.natives_used {
        let key = match vm.registry().get(&path.to_ascii_lowercase()) {
            Some(def) => match def.status {
                xiii_script::registry::NativeStatus::Implemented => "implemented",
                xiii_script::registry::NativeStatus::Partial(_) => "partial",
            }
            .to_owned(),
            None => "missing".to_owned(),
        };
        *out.entry(key).or_default() += *calls as usize;
    }
    out
}

fn event_kind(e: &PresentationEvent) -> &'static str {
    match e {
        PresentationEvent::PlaySound(_) => "PlaySound",
        PresentationEvent::PlayMusic(_) => "PlayMusic",
        PresentationEvent::PlayRolloffSound(_) => "PlayRolloffSound",
        PresentationEvent::ReplaceTexture { .. } => "ReplaceATextureByAnOther",
        PresentationEvent::RefreshDisplaying { .. } => "RefreshDisplaying",
        PresentationEvent::SetInjuredEffect { .. } => "SetInjuredEffect",
        PresentationEvent::ProjectorAttach { .. } => "ProjectorAttach",
        PresentationEvent::ProjectorDetach { .. } => "ProjectorDetach",
        PresentationEvent::ProjectorAbandon { .. } => "ProjectorAbandon",
        PresentationEvent::StopVoice { .. } => "StopVoice",
        PresentationEvent::StopSound { .. } => "StopSound",
        PresentationEvent::PlaySndPNJOno { .. } => "PlaySndPNJOno",
        PresentationEvent::Dialogue(_) => "Dialogue",
        PresentationEvent::RenderTarget(_) => "RenderTargetMaterial.Update",
        PresentationEvent::SaveCheckpoint(_) => "SaveAtCheckpoint",
    }
}

/// `xiii-tool campaign`'s script survey: level start with all actors active in survey mode,
/// against the real map providers. Unlike the touch harness, this does not require a Touch
/// trigger; it exercises the whole map.
fn survey_script(root: &Path, map: &str) -> Result<ScriptSummary, String> {
    let (set, map_idx) = runtime::load_with_map(root, map)?;
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.survey = true;
    let anim_log = Rc::new(RefCell::new(Vec::new()));
    let providers = runtime::build_map_providers(
        root,
        map,
        &ProviderSpec {
            physics: true,
            animation: true,
            navigation: true,
        },
        &anim_log,
    )?;
    providers.install(&mut vm, |_| {});
    let ids = vm
        .load_level(map_idx, &Limits::default())
        .map_err(|e| format!("load_level: {e}"))?;
    for &id in &ids {
        vm.set_active(id, true);
    }
    let mut s = ScriptSummary {
        actors_loaded: ids.len(),
        load_warnings: vm.load_warnings.len(),
        ..ScriptSummary::default()
    };

    let default_game = runtime::default_game_from_ini(root);
    let game_class = default_game
        .as_deref()
        .and_then(|p| runtime::resolve_class_path(&set, p));
    if let Some(game_class) = game_class {
        s.begin_play = true;
        // `begin_play_all` runs the level-start lifecycle tolerantly: an actor whose code fails
        // is suspended and the rest still run. Its suspended errors are strings, so the kind is
        // parsed from the Display prefix (labels the same kinds as `error_kind_name`).
        let bp = runtime::begin_play_all(&mut vm, &ids, game_class);
        for (actor, err) in bp.suspended {
            s.suspended.push(SuspendedActor {
                actor,
                kind: kind_from_display(&err),
                error: err,
            });
        }
    } else {
        // No GameInfo class in Default.ini: run the post-begin lifecycle directly, still
        // tolerantly, so the map is measured rather than skipped.
        for &id in &ids {
            for ev in ["PostBeginPlay", "SetInitialState"] {
                if let Err(e) = vm.send_event(id, ev, Vec::new()) {
                    let actor = vm.objects[id as usize].name.clone();
                    s.suspended.push(SuspendedActor {
                        actor,
                        kind: error_kind_name(&e.kind),
                        error: e.to_string(),
                    });
                    vm.set_active(id, false);
                }
            }
        }
    }

    for _ in 0..SURVEY_TICKS {
        for (id, e) in vm.tick_suspending(SURVEY_DT) {
            s.suspended.push(SuspendedActor {
                actor: vm.objects[id as usize].name.clone(),
                kind: error_kind_name(&e.kind),
                error: e.to_string(),
            });
        }
    }

    let mut events_events = 0usize;
    for ev in vm.drain_events() {
        events_events += 1;
        *s.events_by_kind
            .entry(event_kind(&ev).to_owned())
            .or_default() += 1;
    }
    s.events_total = events_events;

    s.missing_natives = vm.missing_natives.values().map(missing_row).collect();
    s.missing_natives
        .sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.path.cmp(&b.path)));
    for a in &s.suspended {
        *s.suspended_by_kind.entry(a.kind.clone()).or_default() += 1;
    }
    s.natives_used = native_status(&vm);
    Ok(s)
}

fn missing_row(m: &MissingNative) -> MissingNativeRow {
    let top = m.first_stack.last();
    MissingNativeRow {
        path: m.path.clone(),
        index: m.index,
        calls: m.calls,
        first_site: top.map_or_else(|| "-".to_owned(), |s| s.function.clone()),
        first_actor: top.map_or_else(|| "-".to_owned(), |s| s.object.clone()),
        first_offset: top.map_or(0, |s| s.offset),
    }
}

/// Extracts the error kind identifier from a `VmError` display string (`script error: Kind {...}`).
fn kind_from_display(s: &str) -> String {
    let rest = s.strip_prefix("script error: ").unwrap_or(s);
    rest.split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .filter(|x| !x.is_empty())
        .unwrap_or("Other")
        .to_owned()
}

/// `(representative cause, script site)` from a suspended actor's error text. The display is
/// `script error: Kind { .. }\n  at Package.Class.Function [Actor] code 0x..`; the site is the
/// `Package.Class.Function` token after the first `at `.
fn suspension_location(error: &str) -> (String, String) {
    let mut lines = error.lines();
    let cause = lines.next().unwrap_or(error).trim().to_owned();
    let site = lines
        .map(str::trim)
        .find_map(|l| l.strip_prefix("at "))
        .map(|rest| rest.split_whitespace().next().unwrap_or(rest).to_owned())
        .unwrap_or_else(|| "-".to_owned());
    (cause, site)
}

/// Actor class (`Package.Class`) from a `Package.Class.Function[.State]` site.
fn class_from_site(site: &str) -> String {
    let mut parts = site.split('.');
    match (parts.next(), parts.next()) {
        (Some(pkg), Some(class)) => format!("{pkg}.{class}"),
        _ => "-".to_owned(),
    }
}

/// The `VmErrorKind` variant name (`UnimplementedNative`, `NoPhysicsProvider`, ...).
pub fn error_kind_name(k: &VmErrorKind) -> String {
    let name = match k {
        VmErrorKind::UnsupportedToken { .. } => "UnsupportedToken",
        VmErrorKind::UnimplementedNative { .. } => "UnimplementedNative",
        VmErrorKind::UnregisteredNative { .. } => "UnregisteredNative",
        VmErrorKind::AmbiguousNative { .. } => "AmbiguousNative",
        VmErrorKind::BudgetExceeded { .. } => "BudgetExceeded",
        VmErrorKind::CallDepthExceeded { .. } => "CallDepthExceeded",
        VmErrorKind::Unresolved { .. } => "Unresolved",
        VmErrorKind::TypeMismatch { .. } => "TypeMismatch",
        VmErrorKind::ArrayIndex { .. } => "ArrayIndex",
        VmErrorKind::BadJumpTarget { .. } => "BadJumpTarget",
        VmErrorKind::UnsupportedValue { .. } => "UnsupportedValue",
        VmErrorKind::NotAPlace { .. } => "NotAPlace",
        VmErrorKind::LatentOutsideState { .. } => "LatentOutsideState",
        VmErrorKind::DeferredWithReturnValue { .. } => "DeferredWithReturnValue",
        VmErrorKind::AssertionFailed { .. } => "AssertionFailed",
        VmErrorKind::NoPhysicsProvider { .. } => "NoPhysicsProvider",
        VmErrorKind::NoAnimationProvider { .. } => "NoAnimationProvider",
        VmErrorKind::NoNavProvider { .. } => "NoNavProvider",
        VmErrorKind::NoLocalizationProvider { .. } => "NoLocalizationProvider",
        VmErrorKind::UnknownAnimation { .. } => "UnknownAnimation",
        VmErrorKind::AnimationDataError { .. } => "AnimationDataError",
        VmErrorKind::NewOnActor { .. } => "NewOnActor",
        VmErrorKind::StateCodeEnded => "StateCodeEnded",
        VmErrorKind::NoSuchFunction { .. } => "NoSuchFunction",
        VmErrorKind::DivisionByZero => "DivisionByZero",
        VmErrorKind::Other(_) => "Other",
    };
    name.to_owned()
}

// -------------------------------------------------------------------------------------------
// Aggregation
// -------------------------------------------------------------------------------------------

/// One row of the campaign-wide missing-native ranking.
#[derive(Debug, Clone)]
pub struct MissingRank {
    /// `Class.Function`.
    pub path: String,
    /// Declared native index.
    pub index: Option<u16>,
    /// Number of maps that hit it.
    pub maps: usize,
    /// Total calls across the campaign.
    pub hits: u64,
    /// First map (in sweep order) that hit it.
    pub first_map: String,
    /// First-hit function.
    pub first_site: String,
}

/// One row of the import problem ranking.
#[derive(Debug, Clone)]
pub struct CounterRank {
    /// Counter key.
    pub key: String,
    /// Number of maps with a non-zero count.
    pub maps: usize,
    /// Total count across the campaign.
    pub count: usize,
}

/// One campaign-wide suspension cause: an error kind at a script location.
#[derive(Debug, Clone)]
pub struct SuspensionRank {
    /// Error kind name (`UnsupportedValue`, `TypeMismatch`, ...).
    pub kind: String,
    /// Representative error summary (first line, map-specific object name kept).
    pub cause: String,
    /// Script location that suspended (`Package.Class.Function`), or `-`.
    pub site: String,
    /// Actor class derived from `site` (`Package.Class`), or `-`.
    pub actor_class: String,
    /// Number of maps that hit it.
    pub maps: usize,
    /// Total suspended occurrences across the campaign.
    pub count: usize,
    /// First map (in sweep order) that hit it.
    pub first_map: String,
}

/// Campaign-wide aggregates.
#[derive(Debug, Clone, Default)]
pub struct Aggregate {
    /// Map count.
    pub maps: usize,
    /// Maps with status `ok`.
    pub ok: usize,
    /// Maps with an error.
    pub errors: usize,
    /// Maps that panicked.
    pub panics: usize,
    /// Total measured wall time, milliseconds.
    pub total_ms: f64,
    /// Missing-native ranking by (maps, hits).
    pub missing_rank: Vec<MissingRank>,
    /// Campaign-wide suspension causes, by (maps, count).
    pub suspension_rank: Vec<SuspensionRank>,
    /// Import problem counters by total count.
    pub import_rank: Vec<CounterRank>,
    /// Maps sorted by total measured time (slowest first).
    pub slowest: Vec<(String, f64)>,
}

/// Per-native aggregation tuple: maps that hit it, total hits, first map, first site, index.
type MissingAcc = (BTreeSet<String>, u64, String, String, Option<u16>);

/// Per-suspension aggregation tuple: maps that hit it, occurrences, first map, cause.
type SuspensionAcc = (BTreeSet<String>, usize, String, String);

/// Builds the aggregate from per-map results.
pub fn aggregate(results: &[MapResult]) -> Aggregate {
    let mut a = Aggregate {
        maps: results.len(),
        ..Aggregate::default()
    };
    // Missing natives: path -> (set of maps, hits, first map, first site, index).
    let mut missing: BTreeMap<String, MissingAcc> = BTreeMap::new();
    // Suspensions: (kind, site) -> (set of maps, count, first map, representative cause).
    let mut suspensions: BTreeMap<(String, String), SuspensionAcc> = BTreeMap::new();
    let mut counters: BTreeMap<String, (BTreeSet<String>, usize)> = BTreeMap::new();
    for m in results {
        a.total_ms += m.total_ms;
        match m.status {
            MapStatus::Ok => a.ok += 1,
            MapStatus::Error => a.errors += 1,
            MapStatus::Panic => a.panics += 1,
        }
        for (k, v) in &m.counters {
            if (k.starts_with("fail.") || k.starts_with("skip.")) && *v > 0 {
                let e = counters.entry(k.clone()).or_default();
                e.0.insert(m.map.clone());
                e.1 += v;
            }
        }
        if let Some(s) = &m.script {
            for n in &s.missing_natives {
                let e = missing.entry(n.path.clone()).or_insert_with(|| {
                    (
                        BTreeSet::new(),
                        0,
                        m.map.clone(),
                        n.first_site.clone(),
                        n.index,
                    )
                });
                e.0.insert(m.map.clone());
                e.1 += n.calls;
                if e.3 == "-" && n.first_site != "-" {
                    e.3 = n.first_site.clone();
                }
            }
            for a in &s.suspended {
                let (cause, site) = suspension_location(&a.error);
                let e = suspensions
                    .entry((a.kind.clone(), site))
                    .or_insert_with(|| (BTreeSet::new(), 0, m.map.clone(), cause));
                e.0.insert(m.map.clone());
                e.1 += 1;
            }
        }
    }
    a.missing_rank = missing
        .into_iter()
        .map(
            |(path, (maps, hits, first_map, first_site, index))| MissingRank {
                path,
                index,
                maps: maps.len(),
                hits,
                first_map,
                first_site,
            },
        )
        .collect();
    a.missing_rank.sort_by(|x, y| {
        y.maps
            .cmp(&x.maps)
            .then(y.hits.cmp(&x.hits))
            .then(x.path.cmp(&y.path))
    });
    a.suspension_rank = suspensions
        .into_iter()
        .map(
            |((kind, site), (maps, count, first_map, cause))| SuspensionRank {
                kind,
                cause,
                actor_class: class_from_site(&site),
                site,
                maps: maps.len(),
                count,
                first_map,
            },
        )
        .collect();
    a.suspension_rank.sort_by(|x, y| {
        y.maps
            .cmp(&x.maps)
            .then(y.count.cmp(&x.count))
            .then(x.site.cmp(&y.site))
    });
    a.import_rank = counters
        .into_iter()
        .map(|(key, (maps, count))| CounterRank {
            key,
            maps: maps.len(),
            count,
        })
        .collect();
    a.import_rank
        .sort_by(|x, y| y.count.cmp(&x.count).then(x.key.cmp(&y.key)));
    a.slowest = results
        .iter()
        .map(|m| (m.map.clone(), m.total_ms))
        .collect();
    a.slowest.sort_by(|x, y| y.1.total_cmp(&x.1));
    a
}

// -------------------------------------------------------------------------------------------
// JSON
// -------------------------------------------------------------------------------------------

fn opt_index(i: Option<u16>) -> Value {
    match i {
        Some(v) => json!(v),
        None => Value::Null,
    }
}

impl MapResult {
    /// Metadata-only JSON object.
    pub fn to_json(&self) -> Value {
        json!({
            "map": self.map,
            "status": self.status.name(),
            "error": self.error,
            "total_ms": self.total_ms,
            "import": {
                "ms": self.import_ms,
                "objects": self.objects, "meshes": self.meshes, "textures": self.textures,
                "fail": self.import_fail, "skip": self.import_skip, "problem": self.import_problem,
                "counters": self.counters, "examples": self.examples,
            },
            "collision": {
                "triangles": self.collision_tris,
                "box_entries": self.collision_box_entries,
                "line_entries": self.collision_line_entries,
                "box_bvh_ms": self.box_bvh_ms, "box_bvh_nodes": self.box_bvh_nodes,
                "box_degenerate": self.box_degenerate,
                "line_bvh_ms": self.line_bvh_ms, "line_bvh_nodes": self.line_bvh_nodes,
            },
            "navigation": {
                "points": self.nav_points, "edges": self.nav_edges,
                "property_failures": self.nav_property_failures,
                "tail_failures": self.nav_tail_failures,
                "unresolved_class": self.nav_unresolved_class,
                "unresolved_end_edges": self.nav_unresolved_end_edges,
            },
            "reach": self.reach.as_ref().map(|r| json!({
                "nav_points": r.nav_points, "edges": r.edges,
                "walking_edges": r.walking_edges, "eligible": r.eligible,
                "passes": r.passes, "failures": r.failures,
                "spawn_failures": r.spawn_failures, "groups": r.groups,
            })),
            "reach_error": self.reach_error,
            "script": self.script.as_ref().map(|s| json!({
                "actors_loaded": s.actors_loaded, "load_warnings": s.load_warnings,
                "begin_play": s.begin_play,
                "missing_natives": s.missing_natives.iter().map(|n| json!({
                    "path": n.path, "index": opt_index(n.index), "calls": n.calls,
                    "first_site": n.first_site, "first_actor": n.first_actor,
                    "first_offset": n.first_offset,
                })).collect::<Vec<_>>(),
                "suspended": s.suspended.iter().map(|x| json!({
                    "actor": x.actor, "kind": x.kind, "error": x.error,
                })).collect::<Vec<_>>(),
                "suspended_by_kind": s.suspended_by_kind,
                "events_total": s.events_total, "events_by_kind": s.events_by_kind,
                "natives_used": s.natives_used,
                "error": s.error,
            })),
            "script_error": self.script_error,
        })
    }
}

impl Aggregate {
    /// Metadata-only JSON object.
    pub fn to_json(&self) -> Value {
        json!({
            "maps": self.maps, "ok": self.ok, "errors": self.errors, "panics": self.panics,
            "total_ms": self.total_ms,
            "missing_native_ranking": self.missing_rank.iter().map(|m| json!({
                "path": m.path, "index": opt_index(m.index), "maps": m.maps, "hits": m.hits,
                "first_map": m.first_map, "first_site": m.first_site,
            })).collect::<Vec<_>>(),
            "suspension_ranking": self.suspension_rank.iter().map(|s| json!({
                "kind": s.kind, "cause": s.cause, "site": s.site, "actor_class": s.actor_class,
                "maps": s.maps, "count": s.count, "first_map": s.first_map,
            })).collect::<Vec<_>>(),
            "import_problem_ranking": self.import_rank.iter().map(|c| json!({
                "key": c.key, "maps": c.maps, "count": c.count,
            })).collect::<Vec<_>>(),
            "slowest_maps": self.slowest.iter().map(|(m, t)| json!({"map": m, "total_ms": t})).collect::<Vec<_>>(),
        })
    }
}

/// Full metadata-only report.
pub fn report_json(r: &CampaignReport) -> Value {
    json!({
        "schema": 1,
        "tool": "xiii-tool campaign (crates/xiii-tool)",
        "method": "Per map, isolated (panic caught): xiii-world import_map counters/time; xiii-collision box+line soups and BVH build; xiii-world navigation decode; xiii-world reach::analyze_with walk; xiii-script VM level start with all map actors active in survey mode against the real physics/animation/navigation providers, then 30 fixed ticks. Metadata only: counters, object/class/native names, timings. No game bytes.",
        "root": r.root,
        "map_discovery": {
            "source": r.discovery.source,
            "detail": r.discovery.detail,
            "maps": r.discovery.maps,
        },
        "maps": r.maps.iter().map(MapResult::to_json).collect::<Vec<_>>(),
        "aggregate": r.aggregate.to_json(),
    })
}

// -------------------------------------------------------------------------------------------
// Text / Markdown report
// -------------------------------------------------------------------------------------------

/// Renders the per-map table and campaign-wide rankings as Markdown.
pub fn report_markdown(r: &CampaignReport) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "# Campaign sweep: {}", r.root);
    let _ = writeln!(out);
    let _ = writeln!(out, "Map discovery: **{}**", r.discovery.source);
    let _ = writeln!(out);
    let _ = writeln!(out, "{}", r.discovery.detail);
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "Maps: {} (ok {}, error {}, panic {}), wall {:.1} s",
        r.aggregate.maps,
        r.aggregate.ok,
        r.aggregate.errors,
        r.aggregate.panics,
        r.aggregate.total_ms / 1000.0
    );
    let _ = writeln!(out);
    let _ = writeln!(out, "## Per-map measurements");
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| map | st | import ms | fail | skip | tris | box bvh ms | nav pts | nav edges | reach pass/elig | script actors | missing | susp | events | total ms |"
    );
    let _ = writeln!(
        out,
        "|---|---|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|---:|---:|"
    );
    for m in &r.maps {
        let reach = m.reach.as_ref().map_or_else(
            || "—".to_owned(),
            |x| format!("{}/{}", x.passes, x.eligible),
        );
        let script_actors = m
            .script
            .as_ref()
            .map_or_else(|| "—".to_owned(), |s| s.actors_loaded.to_string());
        let missing = m
            .script
            .as_ref()
            .map_or_else(|| "—".to_owned(), |s| s.missing_natives.len().to_string());
        let susp = m
            .script
            .as_ref()
            .map_or_else(|| "—".to_owned(), |s| s.suspended.len().to_string());
        let events = m
            .script
            .as_ref()
            .map_or_else(|| "—".to_owned(), |s| s.events_total.to_string());
        let _ = writeln!(
            out,
            "| {} | {} | {:.0} | {} | {} | {} | {:.1} | {} | {} | {} | {} | {} | {} | {} | {:.0} |",
            m.map,
            m.status.name(),
            m.import_ms,
            m.import_fail,
            m.import_skip,
            m.collision_tris,
            m.box_bvh_ms,
            m.nav_points,
            m.nav_edges,
            reach,
            script_actors,
            missing,
            susp,
            events,
            m.total_ms
        );
    }
    let _ = writeln!(out);
    if r.maps.iter().any(|m| m.error.is_some()) {
        let _ = writeln!(out, "### Map errors");
        let _ = writeln!(out);
        for m in &r.maps {
            if let Some(e) = &m.error {
                let _ = writeln!(
                    out,
                    "- **{}** [{}]: {}",
                    m.map,
                    m.status.name(),
                    e.replace('\n', " ")
                );
            }
            if let Some(e) = &m.reach_error {
                let _ = writeln!(out, "- {} reach: {}", m.map, e.replace('\n', " "));
            }
            if let Some(e) = &m.script_error {
                let _ = writeln!(out, "- {} script: {}", m.map, e.replace('\n', " "));
            }
        }
        let _ = writeln!(out);
    }
    let _ = writeln!(
        out,
        "## Campaign-wide suspension causes (top 20, by maps then count)"
    );
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "| # | kind | actor class | site | maps | count | first map | cause |"
    );
    let _ = writeln!(out, "|---:|---|---|---|---:|---:|---|---|");
    if r.aggregate.suspension_rank.is_empty() {
        let _ = writeln!(out, "| — | none |  |  |  |  |  |  |");
    }
    for (i, s) in r.aggregate.suspension_rank.iter().take(20).enumerate() {
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            i + 1,
            s.kind,
            s.actor_class,
            s.site,
            s.maps,
            s.count,
            s.first_map,
            s.cause.replace('|', "\\|")
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Campaign-wide missing natives (by maps, then hits)");
    let _ = writeln!(out);
    let _ = writeln!(out, "| # | native | index | maps | hits | first site |");
    let _ = writeln!(out, "|---:|---|---:|---:|---:|---|");
    if r.aggregate.missing_rank.is_empty() {
        let _ = writeln!(out, "| — | none |  |  |  |  |");
    }
    for (i, m) in r.aggregate.missing_rank.iter().enumerate() {
        let idx = m.index.map_or_else(String::new, |v| v.to_string());
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} |",
            i + 1,
            m.path,
            idx,
            m.maps,
            m.hits,
            m.first_site.replace('|', "\\|")
        );
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Import failure/skip categories (by count)");
    let _ = writeln!(out);
    let _ = writeln!(out, "| # | counter | maps | count |");
    let _ = writeln!(out, "|---:|---|---:|---:|");
    if r.aggregate.import_rank.is_empty() {
        let _ = writeln!(out, "| — | none |  |  |");
    }
    for (i, c) in r.aggregate.import_rank.iter().enumerate() {
        let _ = writeln!(out, "| {} | {} | {} | {} |", i + 1, c.key, c.maps, c.count);
    }
    let _ = writeln!(out);
    let _ = writeln!(out, "## Slowest maps (total measured ms)");
    let _ = writeln!(out);
    let _ = writeln!(out, "| # | map | total ms |");
    let _ = writeln!(out, "|---:|---|---:|");
    for (i, (m, t)) in r.aggregate.slowest.iter().enumerate() {
        let _ = writeln!(out, "| {} | {} | {t:.0} |", i + 1, m);
    }
    out
}

// -------------------------------------------------------------------------------------------
// Command
// -------------------------------------------------------------------------------------------

struct Cli {
    root: Option<PathBuf>,
    root_env: Option<String>,
    maps: Option<String>,
    json: Option<PathBuf>,
    md: Option<PathBuf>,
}

fn parse_cli(args: &[String]) -> Result<Cli, String> {
    let mut c = Cli {
        root: None,
        root_env: None,
        maps: None,
        json: None,
        md: None,
    };
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut val = |what: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| format!("{what} needs a value"))
        };
        match a.as_str() {
            "--root-env" => c.root_env = Some(val("--root-env")?),
            "--maps" => c.maps = Some(val("--maps")?),
            "--json" => c.json = Some(PathBuf::from(val("--json")?)),
            "--md" => c.md = Some(PathBuf::from(val("--md")?)),
            s if s.starts_with("--") => return Err(format!("unknown option '{s}'")),
            s if c.root.is_none() => c.root = Some(PathBuf::from(s)),
            s => return Err(format!("unexpected argument '{s}'")),
        }
    }
    if c.root.is_some() && c.root_env.is_some() {
        return Err("campaign takes either an installation root or --root-env, not both".into());
    }
    if c.root.is_none() && c.root_env.is_none() {
        return Err("campaign needs an installation root or --root-env <VAR>".into());
    }
    Ok(c)
}

/// Resolves the root from `--root-env <VAR>`: the variable must exist and be non-empty.
/// The value is never echoed; only the variable name is reported on error.
fn root_from_env(var: &str) -> Result<PathBuf, String> {
    root_from_env_value(var, std::env::var_os(var))
}

/// Pure core of [`root_from_env`]; the lookup is injected so the failure modes are testable
/// without mutating the process environment (forbidden here).
fn root_from_env_value(var: &str, value: Option<std::ffi::OsString>) -> Result<PathBuf, String> {
    match value {
        Some(v) if !v.is_empty() => Ok(PathBuf::from(v)),
        _ => Err(format!(
            "environment variable {var} is not set (or empty); campaign --root-env needs it"
        )),
    }
}

/// `xiii-tool campaign ...`.
pub fn run_cmd(args: &[String]) -> ExitCode {
    let cli = match parse_cli(args) {
        Ok(c) => c,
        Err(e) => return usage_error(&e),
    };
    let root = match (cli.root, cli.root_env) {
        (Some(root), _) => root,
        (None, Some(var)) => match root_from_env(&var) {
            Ok(root) => root,
            Err(e) => return usage_error(&e),
        },
        (None, None) => return usage_error("campaign needs an installation root or --root-env"),
    };
    if !root.is_dir() {
        return usage_error(&format!("{} is not a directory", root.display()));
    }
    for out in [cli.json.as_deref(), cli.md.as_deref()]
        .into_iter()
        .flatten()
    {
        if inside(out, &root) {
            return usage_error(&format!(
                "refusing to write {} inside the installation",
                out.display()
            ));
        }
    }

    let discovery = match discover_maps(&root, cli.maps.as_deref()) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    eprintln!(
        "campaign: {} maps via {}",
        discovery.maps.len(),
        discovery.source
    );
    let mut results = Vec::with_capacity(discovery.maps.len());
    for (i, map) in discovery.maps.iter().enumerate() {
        eprintln!("campaign: [{}/{}] {map}", i + 1, discovery.maps.len());
        results.push(run_one(&root, map));
    }
    let aggregate = aggregate(&results);
    let report = CampaignReport {
        root: root.display().to_string(),
        discovery,
        maps: results,
        aggregate,
    };
    let md = report_markdown(&report);
    emit(&md);
    if let Some(path) = &cli.json {
        let text = serde_json::to_string_pretty(&report_json(&report)).unwrap_or_default() + "\n";
        if let Err(e) = write_out(path, &text) {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
        eprintln!("wrote {}", path.display());
    }
    if let Some(path) = &cli.md {
        if let Err(e) = write_out(path, &md) {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
        eprintln!("wrote {}", path.display());
    }
    // A panic or an import-level error means the sweep found breakage; that is data, not a
    // usage problem. The command still reports success so the reviewer reads the report.
    ExitCode::SUCCESS
}

fn write_out(path: &Path, text: &str) -> Result<(), String> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    std::fs::write(path, text).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

// -------------------------------------------------------------------------------------------
// Tests
// -------------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_names_cover_variants() {
        assert_eq!(
            error_kind_name(&VmErrorKind::UnimplementedNative {
                path: "Actor.Spawn".into(),
                index: Some(278)
            }),
            "UnimplementedNative"
        );
        assert_eq!(
            error_kind_name(&VmErrorKind::NoPhysicsProvider { native: "x".into() }),
            "NoPhysicsProvider"
        );
        assert_eq!(error_kind_name(&VmErrorKind::Other("x".into())), "Other");
    }

    #[test]
    fn kind_from_display_extracts_the_variant() {
        assert_eq!(
            kind_from_display(
                "script error: UnimplementedNative { path: \"Actor.Spawn\", index: Some(278) }\n  at ..."
            ),
            "UnimplementedNative"
        );
        assert_eq!(kind_from_display("no prefix Other(\"x\")"), "no");
    }

    #[test]
    fn suspension_location_parses_cause_site_and_actor_class() {
        let err = "script error: UnsupportedValue { desc: \"context on uninstantiated object \
                   Plage01.SpriteEmitter135\" }\n  at xidcine.TrigerredEmitter.PostBeginPlay \
                   [TrigerredEmitter0] code 0x0017\n";
        let (cause, site) = suspension_location(err);
        assert!(cause.starts_with("script error: UnsupportedValue"));
        assert_eq!(site, "xidcine.TrigerredEmitter.PostBeginPlay");
        assert_eq!(class_from_site(&site), "xidcine.TrigerredEmitter");
        // A state function keeps the class prefix.
        assert_eq!(
            class_from_site("xiii.XIIICorpseStaticMesh.Dead.BeginState"),
            "xiii.XIIICorpseStaticMesh"
        );
        // An error with no stack yields a `-` site.
        let (_, site) = suspension_location("script error: Other(\"x\")");
        assert_eq!(site, "-");
    }

    /// A chain builder: the campaign is a linked list; a repeated link must terminate.
    #[test]
    fn chain_from_follows_links_and_stops_on_repeat() {
        let exact: BTreeMap<String, String> = [("a", "A"), ("b", "B"), ("c", "C")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        // a -> b -> c -> a (cycle)
        let next: BTreeMap<String, String> = [("a", "B.unr"), ("b", "c.unr"), ("c", "A.unr")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(chain_from("a", &exact, &next), vec!["A", "B", "C"]);
    }

    #[test]
    fn chain_from_stops_when_the_target_is_not_installed() {
        let exact: BTreeMap<String, String> = [("a", "A")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        let next: BTreeMap<String, String> = [("a", "missing.unr")]
            .into_iter()
            .map(|(k, v)| (k.to_owned(), v.to_owned()))
            .collect();
        assert_eq!(chain_from("a", &exact, &next), vec!["A"]);
    }

    #[test]
    fn multiplayer_classification() {
        for m in [
            "DM_Banque",
            "ctf_sanc",
            "SB_Camp",
            "dm_testpath",
            "Entry",
            "MapCredits",
        ] {
            assert!(is_multiplayer_map(m), "{m} should be excluded");
        }
        for m in ["Plage00", "banque01", "SSH101a", "PRock03"] {
            assert!(!is_multiplayer_map(m), "{m} should be kept");
        }
    }

    fn sample(
        map: &str,
        missing: &[(&str, u64)],
        fail: usize,
        ms: f64,
        status: MapStatus,
    ) -> MapResult {
        let mut r = MapResult::new(map);
        r.status = status;
        r.total_ms = ms;
        r.import_fail = fail;
        if fail > 0 {
            r.counters
                .insert("fail.actor.static_mesh (Foo)".into(), fail);
        }
        r.script = Some(ScriptSummary {
            actors_loaded: 1,
            missing_natives: missing
                .iter()
                .map(|(p, c)| MissingNativeRow {
                    path: (*p).to_owned(),
                    index: None,
                    calls: *c,
                    first_site: "Foo.Bar".into(),
                    first_actor: "A".into(),
                    first_offset: 0,
                })
                .collect(),
            ..ScriptSummary::default()
        });
        r
    }

    /// The ranking must sort by number of maps first, then hits; a native on two maps outranks
    /// one with more calls but a single map.
    #[test]
    fn aggregate_ranks_missing_natives_by_maps_then_hits() {
        let results = vec![
            sample(
                "M1",
                &[("Actor.Wide", 1), ("Actor.Narrow", 100)],
                0,
                5.0,
                MapStatus::Ok,
            ),
            sample("M2", &[("Actor.Wide", 2)], 0, 9.0, MapStatus::Ok),
        ];
        let a = aggregate(&results);
        assert_eq!(a.missing_rank[0].path, "Actor.Wide");
        assert_eq!(a.missing_rank[0].maps, 2);
        assert_eq!(a.missing_rank[0].hits, 3);
        assert_eq!(a.missing_rank[1].path, "Actor.Narrow");
        assert_eq!(a.slowest[0].0, "M2");
        assert_eq!(
            a.missing_rank[0].first_map, "M1",
            "earliest map in sweep order that hit it"
        );
        assert_eq!(a.missing_rank[1].first_map, "M1");
    }

    #[test]
    fn aggregate_counts_statuses_and_import_problems() {
        let results = vec![
            sample("M1", &[], 3, 1.0, MapStatus::Ok),
            sample("M2", &[], 0, 2.0, MapStatus::Error),
            sample("M3", &[], 0, 3.0, MapStatus::Panic),
        ];
        let a = aggregate(&results);
        assert_eq!((a.ok, a.errors, a.panics), (1, 1, 1));
        assert_eq!(a.import_rank.len(), 1);
        assert_eq!(a.import_rank[0].count, 3);
        assert_eq!(a.import_rank[0].maps, 1);
        assert!((a.total_ms - 6.0).abs() < 1e-9);
    }

    /// Import counters must classify only `fail.`/`skip.` as problems; `note.` is not a failure.
    #[test]
    fn import_problem_ranking_ignores_notes_and_placement() {
        let mut r = MapResult::new("M");
        r.counters.insert("note.zones.sky".into(), 5);
        r.counters.insert("placement.location.map".into(), 9);
        r.counters.insert("skip.actor.hidden (Foo)".into(), 2);
        let a = aggregate(&[r]);
        assert_eq!(a.import_rank.len(), 1);
        assert_eq!(a.import_rank[0].key, "skip.actor.hidden (Foo)");
    }

    #[test]
    fn report_json_is_metadata_shape() {
        let report = CampaignReport {
            root: "G".into(),
            discovery: MapDiscovery {
                maps: vec!["Plage00".into()],
                source: "test".into(),
                detail: "d".into(),
            },
            maps: vec![sample(
                "Plage00",
                &[("Actor.Spawn", 4)],
                1,
                10.0,
                MapStatus::Ok,
            )],
            aggregate: Aggregate::default(),
        };
        let v = report_json(&report);
        assert_eq!(v["schema"], 1);
        assert_eq!(v["map_discovery"]["maps"][0], "Plage00");
        assert_eq!(
            v["maps"][0]["script"]["missing_natives"][0]["path"],
            "Actor.Spawn"
        );
        assert_eq!(v["maps"][0]["import"]["fail"], 1);
    }

    /// `--root-env` is the alternative to a positional root; the two are mutually exclusive and
    /// one of them is required. The variable's value is never part of the parsed CLI.
    #[test]
    fn parse_cli_root_selection() {
        let c = parse_cli(&["--root-env".into(), "XIII_STEAM_DIR".into()]).expect("root-env alone");
        assert_eq!(c.root, None);
        assert_eq!(c.root_env.as_deref(), Some("XIII_STEAM_DIR"));

        let c = parse_cli(&["G".into(), "--maps".into(), "A,B".into()])
            .expect("positional root with maps");
        assert_eq!(c.root, Some(PathBuf::from("G")));
        assert_eq!(c.root_env, None);
        assert_eq!(c.maps.as_deref(), Some("A,B"));

        assert!(parse_cli(&[]).is_err(), "no root is an error");
        assert!(
            parse_cli(&["G".into(), "--root-env".into(), "XIII_STEAM_DIR".into()]).is_err(),
            "root and --root-env together is an error"
        );
    }

    /// An unset (or empty) variable must be a usage error, not a silent empty path.
    #[test]
    fn root_from_env_value_unset_and_empty_are_errors() {
        assert!(root_from_env_value("X", None).is_err());
        assert!(root_from_env_value("X", Some(std::ffi::OsString::new())).is_err());
        assert_eq!(
            root_from_env_value("X", Some(std::ffi::OsString::from("some/root"))).expect("set"),
            PathBuf::from("some/root")
        );
    }

    /// Reads a variable that exists in every process: the success path of [`root_from_env`].
    #[test]
    fn root_from_env_reads_an_existing_variable() {
        let var = "PATH";
        let expected = std::env::var_os(var).expect("PATH is set on every supported platform");
        assert_eq!(root_from_env(var).expect("PATH"), PathBuf::from(expected));
    }

    /// Opt-in: the whole discovered Steam campaign sweep completes with zero panics. Every map is
    /// imported and script-surveyed in isolation; a panic in any map fails this test (the same
    /// condition the `xiii-tool campaign --root-env XIII_STEAM_DIR` acceptance run reports).
    #[test]
    fn opt_in_steam_campaign_sweep_has_no_panics() {
        let Some(root) = std::env::var_os("XIII_STEAM_DIR") else {
            println!("SKIPPED: set XIII_STEAM_DIR to the Steam installation root to run this test");
            return;
        };
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let discovery = discover_maps(&path, None).expect("discover Steam maps");
        assert!(!discovery.maps.is_empty(), "no Steam maps discovered");
        let mut panics = Vec::new();
        for map in &discovery.maps {
            let r = run_one(&path, map);
            if r.status == MapStatus::Panic {
                eprintln!("PANIC on {map}: {:?}", r.error);
                panics.push(map.clone());
            }
        }
        assert!(panics.is_empty(), "Steam campaign panicked on {panics:?}");
        println!(
            "steam campaign sweep: {} maps, 0 panics",
            discovery.maps.len()
        );
    }

    /// Opt-in corpus test: sweep two real maps. Prints `SKIPPED` without `XIII_GOG_DIR`.
    #[test]
    fn opt_in_corpus_two_maps_sweep() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        for map in ["Plage00", "Plage01"] {
            let r = run_one(&path, map);
            assert_eq!(r.map, map);
            assert!(r.objects > 0, "{map}: imported objects");
            assert!(r.collision_box_entries > 0, "{map}: box collision entries");
            assert!(
                r.script.is_some(),
                "{map}: script survey ran: {:?}",
                r.script_error
            );
            assert!(
                r.reach.is_some(),
                "{map}: reach walk ran: {:?}",
                r.reach_error
            );
            println!(
                "campaign test {map}: {} objects, {} tris, {} nav edges, reach {}/{}, script actors {}, missing {}, suspended {}, events {}",
                r.objects,
                r.collision_tris,
                r.nav_edges,
                r.reach.as_ref().map_or(0, |x| x.passes),
                r.reach.as_ref().map_or(0, |x| x.eligible),
                r.script.as_ref().map_or(0, |s| s.actors_loaded),
                r.script.as_ref().map_or(0, |s| s.missing_natives.len()),
                r.script.as_ref().map_or(0, |s| s.suspended.len()),
                r.script.as_ref().map_or(0, |s| s.events_total),
            );
        }
    }
}
