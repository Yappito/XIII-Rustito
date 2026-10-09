//! Campaign start survey (`--survey`, item45).
//!
//! For every campaign map in `MapInfo.NextMapLevelWithUnr` order, this opens the **real**
//! headless session (the same `Session::open` login chain `--play` uses) and runs 90 simulated
//! seconds with no player input through [`super::run_script_with_cinematic_input`] — the exact
//! session code the route tests drive (empty input script; the session logic is not duplicated).
//!
//! Per map it records:
//! * the player controller's state timeline (`Vm` `StateChange` trace records) and the time
//!   control returned (first PlayerWalking-family state after the intro), or "never";
//! * at the end: every live `CineController2`/`Cine2` actor that is not finished, with its
//!   current action text and wait condition (the same fields the `XIII_CINE_TRACE` output
//!   reads: `ScriptedActionIndex`, `flagsPaused` bits, the pawn's action table, targets), and
//!   whether it waits on the player (`flagsPaused` bit 1) or on a player position (`Target`);
//! * VM errors / suspended actors with the first script stack line, missing natives and
//!   call-depth aborts extracted from the failure texts;
//! * the map's objectives (text, primary, anti-goal, completed) at the end;
//! * wall time per map.
//!
//! Classification (§2 of the task): `control`, `control+errors`, `waiting-on-player`,
//! `blocked`, `crash` — see [`Classification`]. This is a diagnostic tool: a blocked map is a
//! **finding**, never a silent success; the tool exits non-zero only when a run itself failed
//! (open/import error or panic).

use std::collections::BTreeMap;
use std::io::BufRead;
use std::panic::AssertUnwindSafe;
use std::path::{Path, PathBuf};
use std::time::Instant;

use bevy::prelude::AppExit;

use xiii_install::{Installation, OpenOptions, PackageKind};
use xiii_package::{Limits, Package, PropertyValue};
use xiii_script::{TraceEvent, TraceKind};

use super::session::{ObjectiveState, Session};
use super::{resolve_params, run_script_with_cinematic_input, script, viewer};
use crate::cli::Options;

/// Simulated seconds per map when `--exit-after-secs` is not given (the task's budget).
pub const DEFAULT_SECS: f32 = 90.0;

/// Player controller states that accept movement input ("PlayerWalking-family"). Decoded state
/// list of `Engine.PlayerController` + `XIII.XIIIPlayerController` (`script functions`):
/// the locomotion states. `NoControl`/`NoMove`/`CameraView`/`PlayingVideo`/`GameEnded*`/
/// `WaitForFirstDisplay` are deliberately absent — those are held/cutscene/death states.
const CONTROL_STATES: &[&str] = &[
    "playerwalking",
    "playerswimming",
    "playerclimbing",
    "playerflying",
    "playergunning",
    "climbdoor",
    "tyroling",
];

/// True when `name` is a PlayerWalking-family (control) state (case-insensitive).
pub(crate) fn is_control_state(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    CONTROL_STATES.contains(&lower.as_str())
}

/// `flagsPaused` wait-condition bits, mirroring the VM's cine trace decode
/// (`xiii-script/src/vm.rs trace_cinematic_controller`); kept in sync by that comment.
const WAIT_FLAGS: &[(i32, &str)] = &[
    (1, "player"),
    (2, "event"),
    (4, "warning"),
    (8, "speech/dial"),
    (16, "move/sequence"),
    (32, "see-player"),
    (64, "seen-by-player"),
    (128, "time"),
    (256, "animation"),
    (512, "not-seen-by-player"),
    (1024, "player-away"),
    (2048, "cadaver"),
];

/// Human-readable wait conditions from a `flagsPaused` bit field. Unknown bits are labelled so
/// a decode drift cannot hide a wait.
pub(crate) fn wait_labels(flags: i32) -> Vec<String> {
    let mut labels = Vec::new();
    let mut explained = 0i32;
    for (bit, label) in WAIT_FLAGS {
        if flags & bit != 0 {
            labels.push((*label).to_owned());
            explained |= bit;
        }
    }
    let unknown = flags & !explained;
    if unknown != 0 {
        labels.push(format!("unknown-bits=0x{unknown:X}"));
    }
    if labels.is_empty() {
        labels.push("none (flagsPaused=0)".to_owned());
    }
    labels
}

// ---------------------------------------------------------------------------
// Campaign order (same evidence source as xiii-tool campaign: MapInfo
// `NextMapLevelWithUnr` links read from the map packages)
// ---------------------------------------------------------------------------

fn stem_lower(s: &str) -> String {
    let s = s.rsplit(['/', '\\']).next().unwrap_or(s);
    let s = s
        .strip_suffix(".unr")
        .or_else(|| s.strip_suffix(".UNR"))
        .unwrap_or(s);
    s.to_ascii_lowercase()
}

/// Reads a map package's `NextMapLevelWithUnr` tagged-property string, if present. Same method
/// as `crates/xiii-tool/src/campaign_cmd.rs read_next_map`.
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

/// Builds the ordered chain starting at `start`, following links; stops at a missing or repeated
/// link. Pure so the cycle/missing-link handling is testable without game data.
pub(crate) fn build_chain(
    start: &str,
    exact_by_stem: &BTreeMap<String, String>,
    next_by_stem: &BTreeMap<String, String>,
) -> Vec<String> {
    let mut chain = Vec::new();
    let mut seen = std::collections::BTreeSet::new();
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

/// Discovers the campaign order from the maps' `NextMapLevelWithUnr` links. Returns the exact
/// map stems in order plus the number of maps that carry a link (for the evidence line).
pub(crate) fn discover_campaign_order(root: &Path) -> Result<(Vec<String>, String), String> {
    let install = Installation::open(root, &OpenOptions::default()).map_err(|e| e.to_string())?;
    let entries: Vec<(String, PathBuf)> = install
        .packages()
        .filter(|e| e.kind == PackageKind::Map)
        .map(|e| (e.name.clone(), e.path.clone()))
        .collect();
    let exact_by_stem: BTreeMap<String, String> = entries
        .iter()
        .map(|(n, _)| (stem_lower(n), n.clone()))
        .collect();
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
    if next_by_stem.is_empty() {
        return Err(
            "no map carries NextMapLevelWithUnr; the campaign order cannot be discovered \
             (use --maps)"
                .to_owned(),
        );
    }
    let target_stems: std::collections::BTreeSet<String> =
        next_by_stem.values().map(|n| stem_lower(n)).collect();
    let mut candidates: Vec<String> = next_by_stem
        .keys()
        .filter(|k| !target_stems.contains(*k))
        .cloned()
        .collect();
    if candidates.is_empty() {
        candidates = next_by_stem.keys().cloned().collect();
    }
    candidates.sort();
    let mut best: Vec<String> = Vec::new();
    for c in &candidates {
        let chain = build_chain(c, &exact_by_stem, &next_by_stem);
        if chain.len() > best.len() {
            best = chain;
        }
    }
    if best.is_empty() {
        return Err("NextMapLevelWithUnr links exist but form no chain (use --maps)".to_owned());
    }
    let mut source = format!(
        "NextMapLevelWithUnr chain (start {}, {} map(s) carry a link)",
        best[0],
        next_by_stem.len()
    );
    if read_errors > 0 {
        source.push_str(&format!(
            "; {read_errors} map package(s) could not be read for links"
        ));
    }
    Ok((best, source))
}

/// Parses the `--maps a,b,..` override into exact stems (case-insensitive), keeping the given
/// order.
fn explicit_maps(root: &Path, list: &str) -> Result<Vec<String>, String> {
    let install = Installation::open(root, &OpenOptions::default()).map_err(|e| e.to_string())?;
    let exact_by_stem: BTreeMap<String, String> = install
        .packages()
        .filter(|e| e.kind == PackageKind::Map)
        .map(|e| (stem_lower(&e.name), e.name.clone()))
        .collect();
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
    Ok(maps)
}

// ---------------------------------------------------------------------------
// Per-map survey run
// ---------------------------------------------------------------------------

/// One unfinished cine actor row (the data `XIII_CINE_TRACE` prints, read at the end state).
#[derive(Debug, Clone)]
pub(crate) struct CineRow {
    /// Actor name (`CineController21`, `Cine0`, ...).
    pub name: String,
    /// Class path.
    pub class: String,
    /// True for a `CineController2` (the interpreter), false for a `Cine2` (sequence pawn).
    pub is_controller: bool,
    /// Still in the executed scope.
    pub active: bool,
    /// Suspended by a script error.
    pub suspended: bool,
    /// Current state name.
    pub state: Option<String>,
    /// `ScriptedActionIndex` (controllers).
    pub action_index: Option<i32>,
    /// Action count of the pawn's active table (controllers).
    pub total_actions: Option<usize>,
    /// Current action text (the entry before `ScriptedActionIndex`, as the VM trace prints).
    pub current_action: Option<String>,
    /// Raw `flagsPaused` (controllers).
    pub flags_paused: Option<i32>,
    /// Decoded wait conditions.
    pub wait_labels: Vec<String>,
    /// `bInitialized` (sequence pawns): false = still gated in `CineInit`.
    pub b_initialized: Option<bool>,
    /// Sequence-pawn gate: `MI.EndCartoonEffect` when the pawn is still uninitialized.
    pub end_cartoon_effect: Option<bool>,
    /// Waits on the player (`flagsPaused` bit 1).
    pub waits_on_player: bool,
}

/// Everything the survey records about one map's run.
pub(crate) struct MapSurvey {
    /// Map stem.
    pub map: String,
    /// Whole per-map wall time (scene import + run + extraction), seconds.
    pub wall_secs: f32,
    /// Fixed ticks run (`None` when the run failed before ticking).
    pub ticks: Option<u64>,
    /// Run failure text (map open/import error or panic payload), when the run crashed.
    pub run_error: Option<String>,
    /// Player controller actor name.
    pub controller: Option<String>,
    /// Controller state at login (`SetInitialState`), from the first timeline entry.
    pub initial_state: Option<String>,
    /// Controller state at the end.
    pub final_state: Option<String>,
    /// `(time, from, to)` for every controller state change.
    pub timeline: Vec<(f64, Option<String>, Option<String>)>,
    /// `(time, state)` of the first PlayerWalking-family state after the intro, or `None`.
    /// A state the intro immediately froze again (`PlayerWalking` -> `NoMove`) still counts.
    pub control_at: Option<(f64, String)>,
    /// `(time, state)` of the moment control was established for good: the last family
    /// transition the controller never left again. `None` when control never held at the end.
    pub stable_control_at: Option<(f64, String)>,
    /// `(actor, first error line + first stack line)` per distinct failing actor.
    pub failures: Vec<(String, String)>,
    /// `(native path, distinct failing actors)` aggregated from the failure texts.
    pub missing_natives: Vec<(String, usize)>,
    /// Actors aborted by `CallDepthExceeded` (first stack line included in `failures`).
    pub call_depth_actors: Vec<String>,
    /// Suspended actor names.
    pub suspended: Vec<String>,
    /// Login-path blocks (`Session.blocked`).
    pub blocked: Vec<String>,
    /// Objectives at the end of the run for the surveyed map.
    pub objectives: Vec<ObjectiveState>,
    /// Unfinished cine actors (see [`collect_cine_rows`]).
    pub cine: Vec<CineRow>,
    /// `(time, actor)` player touches.
    pub touches: Vec<(f64, String)>,
    /// Live `XIIIPlayerPawn` actor count at the end.
    pub player_pawns: usize,
    /// Level transitions observed (usually empty with no player input).
    pub travel: Vec<String>,
    /// Map active at the end (differs from `map` only after a travel).
    pub final_map: String,
    /// Player controller state at the end of the *surveyed* map when a travel happened (the
    /// end-state session then belongs to the next map, so the timeline is per final map).
    pub travelled: bool,
}

/// Aggregated blocking cause across maps: `(error kind, first stack site)` -> maps + count.
pub(crate) type CauseRank = Vec<(String, String, usize, Vec<String>)>;

impl MapSurvey {
    /// A failed run's record (map open/import error or panic): nothing else was measured.
    fn failed(map: &str, wall_secs: f32, error: String) -> Self {
        Self {
            map: map.to_owned(),
            wall_secs,
            ticks: None,
            run_error: Some(error),
            controller: None,
            initial_state: None,
            final_state: None,
            timeline: Vec::new(),
            control_at: None,
            stable_control_at: None,
            failures: Vec::new(),
            missing_natives: Vec::new(),
            call_depth_actors: Vec::new(),
            suspended: Vec::new(),
            blocked: Vec::new(),
            objectives: Vec::new(),
            cine: Vec::new(),
            touches: Vec::new(),
            player_pawns: 0,
            travel: Vec::new(),
            final_map: map.to_owned(),
            travelled: false,
        }
    }
}

/// Entry point for `--survey` (see `cli::USAGE`).
///
/// Each map runs in a **child process** (`--survey-child`): a process-fatal failure such as a
/// Rust stack overflow cannot be caught in-process, so the parent contains it to that map
/// (recorded as `crash`) and continues the sweep. The child prints its per-map detail block and
/// one `[survey-result]` line (plus `[survey-cause]` lines); the parent streams the child's
/// output through, aggregates the rows and prints the compact table and cause ranking.
pub fn run(opts: &Options) -> AppExit {
    if opts.survey_child {
        child_run(opts)
    } else {
        parent_run(opts)
    }
}

/// One aggregated map row the parent collects from the child result lines.
#[derive(Debug, Clone)]
pub(crate) struct Row {
    /// Campaign order index (1-based).
    pub order: usize,
    /// Map stem.
    pub map: String,
    /// Result class.
    pub class: Classification,
    /// Child-reported wall seconds (scene import + run + extraction).
    pub wall_secs: f32,
    /// Stable-control time, when control held at the end.
    pub control: Option<f64>,
    /// Controller state at the end.
    pub final_state: Option<String>,
    /// Distinct failing actors.
    pub fails: usize,
    /// Natives hit without implementation.
    pub missing: usize,
    /// Suspended actors.
    pub susp: usize,
    /// Unfinished cine actors.
    pub cine: usize,
    /// Human-readable classification detail (crash rows carry the failure text here).
    pub detail: String,
    /// Distinct `(kind, site)` causes recorded by this map.
    pub causes: Vec<(String, String)>,
}

/// Parent: discovers the order, spawns one child per map, aggregates and reports.
fn parent_run(opts: &Options) -> AppExit {
    let Some(game_dir) = opts.game_dir.clone() else {
        eprintln!("error: --survey needs --game-dir");
        return AppExit::error();
    };
    let duration = opts.exit_after_secs.unwrap_or(DEFAULT_SECS);
    if !(duration.is_finite() && duration > 0.0) {
        eprintln!("error: --survey duration must be a positive number");
        return AppExit::error();
    }
    let (maps, source) = if let Some(list) = &opts.maps {
        match explicit_maps(&game_dir, list) {
            Ok(m) => (m, format!("explicit --maps ({list})")),
            Err(e) => {
                eprintln!("error: {e}");
                return AppExit::error();
            }
        }
    } else {
        match discover_campaign_order(&game_dir) {
            Ok(m) => m,
            Err(e) => {
                eprintln!("error: {e}");
                return AppExit::error();
            }
        }
    };
    println!("[survey] order source: {source}");
    println!(
        "[survey] {} map(s), {duration}s simulated each, no player input (one child process per map)",
        maps.len()
    );
    for (i, m) in maps.iter().enumerate() {
        println!("[survey]   {:2}. {}", i + 1, m);
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: cannot locate the survey executable: {e}");
            return AppExit::error();
        }
    };
    let mut rows: Vec<Row> = Vec::new();
    for (i, map) in maps.iter().enumerate() {
        println!("\n[survey] === map {}/{}: {map} ===", i + 1, maps.len());
        rows.push(spawn_map_child(&exe, &game_dir, map, duration, i + 1));
    }

    print_table(&rows);
    print_cause_ranking(&rows);
    let crashes: Vec<&Row> = rows
        .iter()
        .filter(|r| r.class == Classification::Crash)
        .collect();
    if !crashes.is_empty() {
        for r in &crashes {
            eprintln!("[survey] crash {}: {}", r.map, r.detail);
        }
        eprintln!(
            "[survey] {} map run(s) crashed (see the table's crash rows)",
            crashes.len()
        );
        return AppExit::error();
    }
    AppExit::Success
}

/// Spawns the single-map child for `map`, streams its stdout through (capturing the result
/// lines) and returns the aggregated row. A child that dies without reporting (abort, stack
/// overflow) becomes a `crash` row with the observed exit status.
fn spawn_map_child(exe: &Path, game_dir: &Path, map: &str, duration: f32, order: usize) -> Row {
    let started = Instant::now();
    let child = std::process::Command::new(exe)
        .arg("--survey-child")
        .arg("--game-dir")
        .arg(game_dir)
        .arg("--maps")
        .arg(map)
        .arg("--exit-after-secs")
        .arg(format!("{duration}"))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::inherit())
        .stdin(std::process::Stdio::null())
        .spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => {
            return Row {
                order,
                map: map.to_owned(),
                class: Classification::Crash,
                wall_secs: started.elapsed().as_secs_f32(),
                control: None,
                final_state: None,
                fails: 0,
                missing: 0,
                susp: 0,
                cine: 0,
                detail: format!("crash: child process could not be started: {e}"),
                causes: Vec::new(),
            };
        }
    };
    let mut result: Option<Row> = None;
    if let Some(stdout) = child.stdout.take() {
        let reader = std::io::BufReader::new(stdout);
        for line in reader.lines().map_while(Result::ok) {
            if let Some(row) = parse_result_line(&line, order) {
                result = Some(row);
            } else if let Some(detail) = line.strip_prefix("[survey-detail] ")
                && let Some(row) = result.as_mut()
            {
                row.detail = detail.to_owned();
            } else if let Some(cause) = line.strip_prefix("[survey-cause] ").map(|rest| {
                let mut it = rest.splitn(2, '\t');
                (
                    it.next().unwrap_or("?").to_owned(),
                    it.next().unwrap_or("<no stack>").to_owned(),
                )
            }) && let Some(row) = result.as_mut()
            {
                row.causes.push(cause);
            }
            println!("{line}");
        }
    }
    let status = child.wait();
    let wall_secs = started.elapsed().as_secs_f32();
    let crashed = |detail: String| Row {
        order,
        map: map.to_owned(),
        class: Classification::Crash,
        wall_secs,
        control: None,
        final_state: None,
        fails: 0,
        missing: 0,
        susp: 0,
        cine: 0,
        detail,
        causes: Vec::new(),
    };
    match (result, status) {
        (Some(mut row), _) => {
            row.order = order;
            row.causes.dedup();
            row
        }
        (None, Ok(status)) => crashed(format!(
            "crash: child terminated without a result line: {status}"
        )),
        (None, Err(e)) => crashed(format!("crash: waiting for the child failed: {e}")),
    }
}

/// Parses one `[survey-result]` line into a [`Row`] (causes arrive on later
/// `[survey-cause]` lines). Tab-separated `key=value` fields; the free-text detail and error
/// arrive on their own lines.
fn parse_result_line(line: &str, order: usize) -> Option<Row> {
    let rest = line.strip_prefix("[survey-result] ")?;
    let mut fields = rest.split('\t');
    let mut map = String::new();
    let mut class = Classification::Blocked;
    let mut wall_secs = 0.0f32;
    let mut control = None;
    let mut final_state = None;
    let mut fails = 0usize;
    let mut missing = 0usize;
    let mut susp = 0usize;
    let mut cine = 0usize;
    for field in fields.by_ref() {
        let Some((key, value)) = field.split_once('=') else {
            continue;
        };
        match key {
            "map" => map = value.to_owned(),
            "class" => {
                class = match value {
                    "control" => Classification::Control,
                    "control+errors" => Classification::ControlErrors,
                    "waiting-on-player" => Classification::WaitingOnPlayer,
                    "crash" => Classification::Crash,
                    _ => Classification::Blocked,
                };
            }
            "wall" => wall_secs = value.parse().unwrap_or(0.0),
            "control" => control = value.parse::<f64>().ok(),
            "final_state" => final_state = Some(value.to_owned()).filter(|s| s != "-"),
            "fails" => fails = value.parse().unwrap_or(0),
            "miss" => missing = value.parse().unwrap_or(0),
            "susp" => susp = value.parse().unwrap_or(0),
            "cine" => cine = value.parse().unwrap_or(0),
            _ => {}
        }
    }
    if map.is_empty() {
        return None;
    }
    Some(Row {
        order,
        map,
        class,
        wall_secs,
        control,
        final_state,
        fails,
        missing,
        susp,
        cine,
        detail: String::new(),
        causes: Vec::new(),
    })
}

/// Child: runs exactly one map (`--maps` must name one), prints the detail block and the
/// machine-readable result lines, and exits 0 whether the map played, blocked or reported a
/// run error (the class field carries it). Only an unexpected internal failure exits non-zero.
fn child_run(opts: &Options) -> AppExit {
    let Some(game_dir) = opts.game_dir.clone() else {
        eprintln!("error: --survey-child needs --game-dir");
        return AppExit::error();
    };
    let duration = opts.exit_after_secs.unwrap_or(DEFAULT_SECS);
    if !(duration.is_finite() && duration > 0.0) {
        eprintln!("error: --survey duration must be a positive number");
        return AppExit::error();
    }
    let Some(list) = &opts.maps else {
        eprintln!("error: --survey-child needs --maps with exactly one map");
        return AppExit::error();
    };
    let maps = match explicit_maps(&game_dir, list) {
        Ok(m) => m,
        Err(e) => {
            eprintln!("error: {e}");
            return AppExit::error();
        }
    };
    let [map] = maps.as_slice() else {
        eprintln!(
            "error: --survey-child runs exactly one map, got {} ({list})",
            maps.len()
        );
        return AppExit::error();
    };
    let resolved = match resolve_params(&game_dir) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("error: {e}");
            return AppExit::error();
        }
    };
    let started = Instant::now();
    // run_one_map drives the VM for the whole login chain; run it on the explicit VM host
    // stack (see vmstack) so the engine-limit recursion guard fires before the thread runs
    // out of stack. The map survey is plain data and crosses back through the join.
    let params = resolved.params;
    let map_name = map.clone();
    let survey = match std::panic::catch_unwind(AssertUnwindSafe(move || {
        crate::vmstack::run_on_vm_stack(move || {
            run_one_map(&game_dir, &map_name, &params, duration)
        })
    })) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            let failed = MapSurvey::failed(map, started.elapsed().as_secs_f32(), e);
            print_details(&failed);
            print_result_lines(&failed);
            return AppExit::Success;
        }
        Err(payload) => {
            let msg = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_owned())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "<non-string panic payload>".to_owned());
            let failed = MapSurvey::failed(
                map,
                started.elapsed().as_secs_f32(),
                format!("panic: {msg}"),
            );
            print_details(&failed);
            print_result_lines(&failed);
            return AppExit::Success;
        }
    };
    let mut survey = survey;
    survey.wall_secs = started.elapsed().as_secs_f32();
    print_details(&survey);
    print_result_lines(&survey);
    AppExit::Success
}

/// Prints the machine-readable result lines the parent aggregates.
fn print_result_lines(survey: &MapSurvey) {
    let control = survey
        .stable_control_at
        .as_ref()
        .map(|(t, _)| format!("{t:.3}"))
        .unwrap_or_else(|| "never".to_owned());
    println!(
        "[survey-result] map={}\tclass={}\twall={:.1}\tcontrol={control}\tfinal_state={}\tfails={}\tmiss={}\tsusp={}\tcine={}",
        survey.map,
        classify(survey).as_str(),
        survey.wall_secs,
        survey.final_state.as_deref().unwrap_or("-"),
        survey.failures.len(),
        survey.missing_natives.len(),
        survey.suspended.len(),
        survey.cine.len(),
    );
    println!("[survey-detail] {}", classification_detail(survey));
    if let Some(error) = &survey.run_error {
        println!("[survey-error] {error}");
    }
    let mut seen = std::collections::BTreeSet::new();
    for (_, summary) in &survey.failures {
        let kind = error_kind(summary);
        let site = summary
            .split(" | ")
            .nth(1)
            .unwrap_or("<no stack>")
            .trim_start_matches("at ")
            .to_owned();
        if seen.insert((kind.clone(), site.clone())) {
            println!("[survey-cause] {kind}\t{site}");
        }
    }
}

/// Imports one map, runs the empty-input session for `duration` seconds through the shared
/// headless session code and extracts the survey data from the end state.
fn run_one_map(
    game_dir: &Path,
    map: &str,
    params: &super::sim::PlayerParams,
    duration: f32,
) -> Result<MapSurvey, String> {
    let opts = Options {
        map: Some(map.to_owned()),
        game_dir: Some(game_dir.to_path_buf()),
        ..Default::default()
    };
    let scene = viewer::load_scene(&opts)?;
    // No player input: an empty script; `run_script_with_cinematic_input` is the same headless
    // session code the route tests use (no duplicated session logic).
    let script = script::Script::parse("")?;
    let outcome =
        run_script_with_cinematic_input(game_dir, map, &script, params, &scene, duration)?;
    Ok(extract(map, &outcome))
}

/// Extracts the survey record from a finished run. `map` is the surveyed map; the objective
/// states are taken from that map's entry in `map_objectives` (the end-state session belongs to
/// `final_map`, which differs only after a travel).
fn extract(map: &str, outcome: &super::ScriptOutcome) -> MapSurvey {
    let session = &outcome.session;
    let vm = session.vm();
    let controller_name = session
        .controller
        .map(|c| vm.objects[c as usize].name.clone());
    let timeline = controller_name
        .as_deref()
        .map(|name| controller_timeline(&vm.trace, name))
        .unwrap_or_default();
    let initial_state = timeline
        .first()
        .and_then(|(_, _, to)| to.clone())
        .or_else(|| session.controller.and_then(|c| vm.state_name(c)));
    let final_state = session.controller.and_then(|c| vm.state_name(c));
    // Control returned: the first PlayerWalking-family state after the intro (timeline entries
    // at t=0 are the login-time SetInitialState), or, when the controller never left it, the
    // initial state itself (evidence: no blocking intro ran).
    let control_at = timeline
        .iter()
        .find(|(t, _, to)| *t > 0.0 && to.as_deref().is_some_and(is_control_state))
        .map(|(t, _, to)| (*t, to.clone().unwrap_or_default()))
        .or_else(|| {
            initial_state
                .as_deref()
                .filter(|s| is_control_state(s))
                .map(|s| (0.0, s.to_owned()))
        });

    let failures = session
        .failures
        .iter()
        .map(|(actor, error)| (actor.clone(), summarize_error(error)))
        .collect();
    let mut missing: BTreeMap<String, usize> = BTreeMap::new();
    let mut call_depth_actors = Vec::new();
    for (actor, error) in &session.failures {
        if error_kind(error) == "CallDepthExceeded" {
            call_depth_actors.push(actor.clone());
        }
        if let Some(path) = native_path(error) {
            *missing.entry(path).or_default() += 1;
        }
    }
    let objectives = outcome
        .map_objectives
        .iter()
        .find(|(m, _)| m.eq_ignore_ascii_case(map))
        .or_else(|| outcome.map_objectives.last())
        .map(|(_, states)| states.clone())
        .unwrap_or_default();
    let cine = collect_cine_rows(session);
    let stable_control_at = stable_control(&timeline, initial_state.as_deref());
    MapSurvey {
        map: map.to_owned(),
        wall_secs: 0.0,
        ticks: Some(outcome.ticks),
        run_error: None,
        controller: controller_name,
        initial_state,
        final_state,
        timeline,
        control_at,
        stable_control_at,
        failures,
        missing_natives: missing.into_iter().collect(),
        call_depth_actors,
        suspended: session.suspended.clone(),
        blocked: session.blocked.clone(),
        objectives,
        cine,
        touches: session.touches(),
        player_pawns: session.player_pawn_actors().len(),
        travel: outcome
            .travel
            .iter()
            .map(|h| format!("{} -> {} at t={:.3}s", h.from, h.to, h.vm_time))
            .collect(),
        final_map: outcome.final_map.clone(),
        travelled: outcome.final_map != map,
    }
}

/// Controller state-change timeline from the VM trace (`StateChange` records of `controller`).
/// Re-entries into the state already held (`from == to`) are not changes and are dropped.
pub(crate) fn controller_timeline(
    trace: &[TraceEvent],
    controller: &str,
) -> Vec<(f64, Option<String>, Option<String>)> {
    trace
        .iter()
        .filter_map(|event| match &event.kind {
            TraceKind::StateChange {
                actor, from, to, ..
            } if actor.eq_ignore_ascii_case(controller) && from.as_deref() != to.as_deref() => {
                Some((event.time, from.clone(), to.clone()))
            }
            _ => None,
        })
        .collect()
}

/// The last transition into a PlayerWalking-family state after which the controller never left
/// the family again: the moment control was established for good. Falls back to the initial
/// state when the timeline has no family transition (a controller that spawned straight into
/// control, or never reached it).
pub(crate) fn stable_control(
    timeline: &[(f64, Option<String>, Option<String>)],
    initial_state: Option<&str>,
) -> Option<(f64, String)> {
    let mut best: Option<(f64, String)> = None;
    for (t, _, to) in timeline {
        let Some(to) = to else { continue };
        if is_control_state(to) {
            best = Some((*t, to.clone()));
        } else {
            best = None;
        }
    }
    best.or_else(|| {
        initial_state
            .filter(|s| is_control_state(s))
            .map(|s| (0.0, s.to_owned()))
    })
}

/// `(kind, first stack line)` summary of one failure text. The kind is the `Debug` name of the
/// `VmErrorKind` (`script error: Kind { .. }` first line); the stack line is the first
/// `  at Package.Class.Function [Actor] code 0x…` entry.
pub(crate) fn summarize_error(error: &str) -> String {
    let kind = error_kind(error);
    let stack = first_stack_line(error).unwrap_or_else(|| "<no stack>".to_owned());
    if stack == "<no stack>" {
        kind
    } else {
        format!("{kind} | {stack}")
    }
}

/// The `VmErrorKind` variant name from a failure text (`script error: Kind { .. }`).
pub(crate) fn error_kind(error: &str) -> String {
    error
        .strip_prefix("script error: ")
        .unwrap_or(error)
        .split(|c: char| !(c.is_alphanumeric() || c == '_'))
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("Other")
        .to_owned()
}

/// First `  at ` stack line of a failure text, trimmed.
pub(crate) fn first_stack_line(error: &str) -> Option<String> {
    error.lines().map(str::trim).find_map(|l| {
        l.strip_prefix("at ")
            .map(|rest| format!("at {}", rest.split_whitespace().next().unwrap_or(rest)))
    })
}

/// `Class.Function` of an `UnimplementedNative`/`UnregisteredNative` failure text, when present.
pub(crate) fn native_path(error: &str) -> Option<String> {
    let marker = "path: \"";
    let start = error.find(marker)? + marker.len();
    let rest = &error[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_owned())
}

/// Live `CineController2`/`Cine2` actors that are not finished, with their wait data.
///
/// Finished means, with evidence read from the VM:
/// * a controller whose sequence completed (state `Idle`) or that was destroyed;
/// * a sequence pawn that left its `CineInit` gate (`bInitialized`) or was destroyed.
///
/// A suspended actor is always unfinished.
pub(crate) fn collect_cine_rows(session: &Session) -> Vec<CineRow> {
    let vm = session.vm();
    let mut rows = Vec::new();
    for (i, object) in vm.objects.iter().enumerate() {
        if !object.is_actor
            || object.deleted
            // Class-default objects (`Default__Cine2`, ...) are templates, not live actors.
            || object.name.starts_with("Default__")
        {
            continue;
        }
        let id = i as xiii_script::ObjectId;
        let is_controller = vm.is_a(id, "CineController2");
        let is_sequence = !is_controller && vm.is_a(id, "Cine2");
        if !is_controller && !is_sequence {
            continue;
        }
        let state = vm.state_name(id);
        let suspended = object.suspended;
        let (action_index, flags_paused, total_actions, current_action, position_waits) =
            if is_controller {
                let action_index = match vm.get_property(id, "ScriptedActionIndex") {
                    Some(xiii_script::Value::Int(v)) => Some(*v),
                    _ => None,
                };
                let flags = match vm.get_property(id, "flagsPaused") {
                    Some(xiii_script::Value::Int(v)) => Some(*v),
                    _ => None,
                };
                // The pawn's active action table and the current action text, exactly like the
                // VM's cine trace: `CurrentTabActionIndex` selects tabActions/tabActions2/3 and the
                // action before `ScriptedActionIndex` is the one that ran / is waiting.
                let pawn = match vm.get_property(id, "MyPawn") {
                    Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(p)))) => {
                        Some(*p)
                    }
                    _ => None,
                };
                let tab = pawn.and_then(|p| match vm.get_property(p, "CurrentTabActionIndex") {
                    Some(xiii_script::Value::Int(t)) => Some(*t),
                    _ => None,
                });
                let list = pawn.and_then(|p| {
                    let name = match tab.unwrap_or(0) {
                        2 => "tabActions2",
                        3 => "tabActions3",
                        _ => "tabActions",
                    };
                    match vm.get_property(p, name) {
                        Some(xiii_script::Value::Array(items)) => Some(items),
                        _ => None,
                    }
                });
                let total = list.as_ref().map(|items| items.len());
                let current = list
                    .and_then(|items| {
                        usize::try_from(action_index?.saturating_sub(1))
                            .ok()
                            .and_then(|idx| items.get(idx))
                    })
                    .map(|v| match v {
                        xiii_script::Value::Str(s) | xiii_script::Value::Name(s) => s.clone(),
                        other => format!("{other:?}"),
                    });
                // Player-position waits, the same evidence the VM cine trace prints: a live
                // Target/NextTarget the controller's pawn has not reached yet.
                let pawn_location = pawn.and_then(|p| vm.vector_prop(p, "Location"));
                let mut position_waits = Vec::new();
                for property in ["Target", "NextTarget"] {
                    if let Some(target) = match vm.get_property(id, property) {
                        Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(
                            t,
                        )))) if vm.objects.get(*t as usize).is_some_and(|o| !o.deleted) => Some(*t),
                        _ => None,
                    } {
                        let target_name = vm.objects[target as usize].name.clone();
                        let target_location = vm.vector_prop(target, "Location");
                        let distance = pawn_location.zip(target_location).map(|(from, to)| {
                            let d = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
                            (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt()
                        });
                        position_waits.push(format!(
                            "{property}={target_name}@{target_location:?} distance={distance:?}"
                        ));
                    }
                }
                (action_index, flags, total, current, position_waits)
            } else {
                (None, None, None, None, Vec::new())
            };
        let b_initialized = match vm.get_property(id, "bInitialized") {
            Some(xiii_script::Value::Bool(b)) => Some(*b),
            _ => None,
        };
        let end_cartoon_effect = if is_sequence && b_initialized != Some(true) {
            match vm.get_property(id, "MI") {
                Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(mi)))) => {
                    Some(matches!(
                        vm.get_property(*mi, "EndCartoonEffect"),
                        Some(xiii_script::Value::Bool(true))
                    ))
                }
                _ => None,
            }
        } else {
            None
        };
        let mut wait_labels = flags_paused.map(wait_labels).unwrap_or_default();
        wait_labels.extend(position_waits);
        let unfinished = suspended
            || if is_controller {
                // A finished controller is in `Idle` (EndOfSeq) or destroyed. Anything else —
                // still interpreting (PlayingSequence), waiting to start (STA_init/Pre...),
                // mid-move or suspended — is unfinished.
                state.as_deref().map(|s| !s.eq_ignore_ascii_case("Idle")) != Some(false)
                    || flags_paused.is_some_and(|f| f != 0)
            } else {
                // A sequence pawn is finished once it left the CineInit gate (bInitialized).
                b_initialized != Some(true)
            };
        if !unfinished {
            continue;
        }
        rows.push(CineRow {
            name: object.name.clone(),
            class: vm.set().path(object.class),
            is_controller,
            active: object.active,
            suspended,
            state,
            action_index,
            total_actions,
            current_action,
            flags_paused,
            wait_labels,
            b_initialized,
            end_cartoon_effect,
            waits_on_player: flags_paused.is_some_and(|f| f & 1 != 0),
        });
    }
    rows.sort_by(|a, b| {
        b.is_controller
            .cmp(&a.is_controller)
            .then(a.name.cmp(&b.name))
    });
    rows
}

// ---------------------------------------------------------------------------
// Output
// ---------------------------------------------------------------------------

/// Result class of one map (task §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Classification {
    /// Control returned, no errors.
    Control,
    /// Control returned with recorded script failures.
    ControlErrors,
    /// Control did not return; the intro legitimately waits on the player (evidence required).
    WaitingOnPlayer,
    /// Control did not return; a VM/native defect or an unexplained stall blocks it.
    Blocked,
    /// The run itself failed (open/import error or panic).
    Crash,
}

impl Classification {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Classification::Control => "control",
            Classification::ControlErrors => "control+errors",
            Classification::WaitingOnPlayer => "waiting-on-player",
            Classification::Blocked => "blocked",
            Classification::Crash => "crash",
        }
    }
}

/// Classifies one map's result. Precedence (documented in the report): crash > control
/// (stably held at the end) > waiting-on-player (evidence) > blocked.
pub(crate) fn classify(survey: &MapSurvey) -> Classification {
    if survey.run_error.is_some() {
        return Classification::Crash;
    }
    if survey.stable_control_at.is_some() {
        return if survey.failures.is_empty() && survey.blocked.is_empty() {
            Classification::Control
        } else {
            Classification::ControlErrors
        };
    }
    if survey
        .cine
        .iter()
        .any(|c| c.waits_on_player || c.wait_labels.iter().any(|l| l.starts_with("Target=")))
    {
        return Classification::WaitingOnPlayer;
    }
    Classification::Blocked
}

/// True when a cine row's wait data shows a player-position wait (`Target`/`NextTarget` label).
pub(crate) fn waits_on_player_position(row: &CineRow) -> bool {
    row.wait_labels
        .iter()
        .any(|l| l.starts_with("Target=") || l.starts_with("NextTarget="))
}

fn classification_detail(survey: &MapSurvey) -> String {
    let control = survey
        .stable_control_at
        .as_ref()
        .map(|(t, s)| (*t, s.as_str()));
    match classify(survey) {
        Classification::Control => format!(
            "control established at t={:.3}s ({})",
            control.map(|(t, _)| t).unwrap_or_default(),
            control.map(|(_, s)| s).unwrap_or(""),
        ),
        Classification::ControlErrors => format!(
            "control established at t={:.3}s + {} failure(s)",
            control.map(|(t, _)| t).unwrap_or_default(),
            survey.failures.len(),
        ),
        Classification::WaitingOnPlayer => {
            let who = survey
                .cine
                .iter()
                .filter(|c| c.waits_on_player || waits_on_player_position(c))
                .map(|c| c.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            format!("waiting-on-player ({who})")
        }
        Classification::Blocked => {
            if survey.failures.is_empty() {
                "blocked (no script failure; see end state)".to_owned()
            } else {
                format!("blocked: {}", survey.failures[0].1)
            }
        }
        Classification::Crash => format!("crash: {}", survey.run_error.as_deref().unwrap_or("?")),
    }
}

/// Prints one map's detail block (called right after the map's run).
fn print_details(survey: &MapSurvey) {
    let m = &survey.map;
    println!(
        "[survey] {}: classification {} — {}",
        m,
        classify(survey).as_str(),
        classification_detail(survey)
    );
    if let Some(error) = &survey.run_error {
        println!("[survey] {m}: run failed: {error}");
        return;
    }
    println!(
        "[survey] {m}: controller {} initial {} final {}; ticks {:?}; player pawns {}; wall {:.1}s",
        survey.controller.as_deref().unwrap_or("-"),
        survey.initial_state.as_deref().unwrap_or("-"),
        survey.final_state.as_deref().unwrap_or("-"),
        survey.ticks,
        survey.player_pawns,
        survey.wall_secs,
    );
    match &survey.control_at {
        Some((t, state)) => println!("[survey] {m}: first control state at t={t:.3}s ({state})"),
        None => println!("[survey] {m}: control NEVER returned"),
    }
    if let Some((t, s)) = &survey.stable_control_at
        && survey
            .control_at
            .as_ref()
            .is_some_and(|(ft, _)| (*ft - *t).abs() > f64::EPSILON)
    {
        println!(
            "[survey] {m}: control was interrupted by the intro; established for good at t={t:.3}s ({s})"
        );
    }
    println!("[survey] {m}: controller state timeline:");
    for (t, from, to) in &survey.timeline {
        println!(
            "[survey] {m}:   t={t:.3}s {} -> {}",
            from.as_deref().unwrap_or("<none>"),
            to.as_deref().unwrap_or("<none>"),
        );
    }
    if survey.timeline.is_empty() {
        println!("[survey] {m}:   <no state change recorded>");
    }
    if !survey.failures.is_empty() {
        println!(
            "[survey] {m}: {} distinct failing actor(s):",
            survey.failures.len()
        );
        for (actor, summary) in survey.failures.iter().take(12) {
            println!("[survey] {m}:   {actor}: {summary}");
        }
        if survey.failures.len() > 12 {
            println!(
                "[survey] {m}:   ... and {} more (see the full log)",
                survey.failures.len() - 12
            );
        }
    }
    if !survey.missing_natives.is_empty() {
        println!("[survey] {m}: natives hit without implementation:");
        for (path, actors) in &survey.missing_natives {
            println!("[survey] {m}:   {path} ({actors} actor(s))");
        }
    }
    if !survey.call_depth_actors.is_empty() {
        println!(
            "[survey] {m}: CallDepthExceeded aborts: {}",
            survey.call_depth_actors.join(", ")
        );
    }
    if !survey.suspended.is_empty() {
        println!(
            "[survey] {m}: suspended actors ({}): {}",
            survey.suspended.len(),
            survey.suspended.join(", ")
        );
    }
    if !survey.blocked.is_empty() {
        for b in &survey.blocked {
            println!("[survey] {m}:   login path blocked: {b}");
        }
    }
    if survey.cine.is_empty() {
        println!("[survey] {m}: cine: no unfinished Cine2/CineController2 actor");
    } else {
        println!(
            "[survey] {m}: {} unfinished cine actor(s):",
            survey.cine.len()
        );
        for c in &survey.cine {
            let kind = if c.is_controller { "ctrl" } else { "seq" };
            let actions = match (c.action_index, c.total_actions) {
                (Some(i), Some(n)) => format!(" action {}/{}", i, n),
                (Some(i), None) => format!(" action {i}/?"),
                _ => String::new(),
            };
            let flags = c
                .flags_paused
                .map(|f| format!(" flagsPaused=0x{f:X}"))
                .unwrap_or_default();
            let action = c
                .current_action
                .as_deref()
                .map(|a| format!(" action[{a:?}]"))
                .unwrap_or_default();
            let init = c
                .b_initialized
                .map(|b| format!(" bInitialized={b}"))
                .unwrap_or_default();
            let gate = c
                .end_cartoon_effect
                .map(|b| format!(" MI.EndCartoonEffect={b}"))
                .unwrap_or_default();
            println!(
                "[survey] {m}:   [{kind}] {} class={} state={} active={} suspended={} {flags} wait={} {actions}{action}{init}{gate}",
                c.name,
                c.class,
                c.state.as_deref().unwrap_or("<none>"),
                c.active,
                c.suspended,
                if c.wait_labels.is_empty() {
                    "-".to_owned()
                } else {
                    c.wait_labels.join("|")
                },
            );
            if c.waits_on_player {
                println!(
                    "[survey] {m}:     -> waits on the PLAYER (flagsPaused bit 1); player touches this run: {}",
                    survey.touches.len(),
                );
            }
        }
    }
    if !survey.touches.is_empty() {
        let last = survey
            .touches
            .iter()
            .rev()
            .take(5)
            .map(|(t, a)| format!("[{t:.3}s] {a}"))
            .collect::<Vec<_>>()
            .join(", ");
        println!(
            "[survey] {m}: player touches {} (last: {last})",
            survey.touches.len()
        );
    }
    println!(
        "[survey] {m}: objectives at end: {}",
        if survey.objectives.is_empty() {
            "<none>".to_owned()
        } else {
            survey
                .objectives
                .iter()
                .map(|o| {
                    format!(
                        "[{}{}{}] {}",
                        o.index,
                        if o.primary { " P" } else { " -" },
                        if o.completed {
                            " C"
                        } else if o.anti_goal {
                            " A"
                        } else {
                            " ."
                        },
                        if o.text.is_empty() {
                            "<empty>"
                        } else {
                            o.text.as_str()
                        }
                    )
                })
                .collect::<Vec<_>>()
                .join(" | ")
        }
    );
    if survey.travelled {
        println!(
            "[survey] {m}: NOTE the run travelled to {} (timeline/cine data are from the final map): {:?}",
            survey.final_map, survey.travel,
        );
    }
}

/// One row of the compact table.
fn table_row(row: &Row) -> String {
    format!(
        "{:>2}. {:<12} {:<16} ctrl@{:<7} fails {:>2} miss {:>2} susp {:>2} cine {:>2} {:>6.1}s  {}",
        row.order,
        row.map,
        row.final_state.as_deref().unwrap_or("-"),
        row.control
            .map(|t| format!("{t:.1}s"))
            .unwrap_or_else(|| "never".to_owned()),
        row.fails,
        row.missing,
        row.susp,
        row.cine,
        row.wall_secs,
        row.class.as_str(),
    )
}

/// Prints the compact one-line-per-map table and the aggregate header.
fn print_table(rows: &[Row]) {
    println!("\n[survey] compact table (one line per map, campaign order):");
    println!(
        "[survey] {:>2} {:<12} {:<16} {:<14} {:>5} {:>4} {:>4} {:>4} {:>8}  class",
        "#", "map", "final state", "control@", "fails", "miss", "susp", "cine", "wall s"
    );
    for r in rows {
        println!("[survey] {}", table_row(r));
    }
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in rows {
        *counts.entry(r.class.as_str()).or_default() += 1;
    }
    println!("[survey] classification totals: {:?}", counts);
}

/// Aggregates the distinct causes across maps, most common first: `(kind, first stack site,
/// occurrence count, maps)`.
pub(crate) fn cause_ranking(rows: &[Row]) -> CauseRank {
    let mut by_cause: BTreeMap<(String, String), (usize, Vec<String>)> = BTreeMap::new();
    for r in rows {
        for (kind, site) in &r.causes {
            let entry = by_cause.entry((kind.clone(), site.clone())).or_default();
            entry.0 += 1;
            if !entry.1.contains(&r.map) {
                entry.1.push(r.map.clone());
            }
        }
    }
    let mut rank: Vec<(String, String, usize, Vec<String>)> = by_cause
        .into_iter()
        .map(|((kind, site), (count, maps))| (kind, site, count, maps))
        .collect();
    rank.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)).then(a.1.cmp(&b.1)));
    rank
}

fn print_cause_ranking(rows: &[Row]) {
    let rank = cause_ranking(rows);
    println!("\n[survey] blocking-cause ranking (distinct cause per map, by occurrences):");
    if rank.is_empty() {
        println!("[survey]   none");
        return;
    }
    for (i, (kind, site, count, maps)) in rank.iter().take(15).enumerate() {
        println!(
            "[survey]   {}. {} at {site} — {count} occurrence(s), maps: {}",
            i + 1,
            kind,
            maps.join(", "),
        );
    }
}

// ---------------------------------------------------------------------------
// Tests (pure helpers; game data only through the opt-in test below)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn state_change(time: f64, actor: &str, from: Option<&str>, to: Option<&str>) -> TraceEvent {
        TraceEvent {
            tick: 0,
            time,
            kind: TraceKind::StateChange {
                actor: actor.to_owned(),
                from: from.map(str::to_owned),
                to: to.map(str::to_owned),
                label: None,
            },
        }
    }

    /// The timeline only carries the named controller's state changes, in trace order, and
    /// drops re-entries into the state already held (`from == to`).
    #[test]
    fn timeline_filters_the_controller() {
        let trace = vec![
            state_change(0.0, "XIIIPlayerController", None, Some("NoControl")),
            state_change(0.1, "IAController3", None, Some("faction")),
            state_change(
                1.5,
                "XIIIPlayerController",
                Some("NoControl"),
                Some("CameraView"),
            ),
            // A re-entry is not a change and must be dropped.
            state_change(
                2.0,
                "XIIIPlayerController",
                Some("CameraView"),
                Some("CameraView"),
            ),
            state_change(
                4.0,
                "XIIIPlayerController",
                Some("CameraView"),
                Some("PlayerWalking"),
            ),
        ];
        let timeline = controller_timeline(&trace, "XIIIPlayerController");
        assert_eq!(timeline.len(), 3);
        assert_eq!(timeline[0].1.as_deref(), None);
        assert_eq!(timeline[2].2.as_deref(), Some("PlayerWalking"));
    }

    /// Stable control: the last family transition never followed by a non-family one; a first
    /// family state that the intro re-froze does not count as established.
    #[test]
    fn stable_control_ignores_re_frozen_returns() {
        let timeline = vec![
            (0.0, None, Some("PlayerWaiting".to_owned())),
            (
                0.2,
                Some("PlayerWaiting".into()),
                Some("PlayerWalking".into()),
            ),
            (0.25, Some("PlayerWalking".into()), Some("NoMove".into())),
            (6.1, Some("NoMove".into()), Some("PlayerWalking".into())),
        ];
        let stable = stable_control(&timeline, Some("PlayerWaiting"));
        assert_eq!(
            stable.as_ref().map(|(t, s)| (*t, s.as_str())),
            Some((6.1, "PlayerWalking"))
        );
        // Control frozen again at the end: no stable control.
        let frozen = vec![
            (0.2, None, Some("PlayerWalking".to_owned())),
            (1.0, Some("PlayerWalking".into()), Some("NoControl".into())),
        ];
        assert_eq!(stable_control(&frozen, None), None);
        // No transitions at all: the initial state decides.
        assert_eq!(
            stable_control(&[], Some("PlayerWalking")).map(|(t, _)| t),
            Some(0.0)
        );
        assert_eq!(stable_control(&[], Some("NoControl")), None);
        assert_eq!(stable_control(&[], None), None);
    }

    /// Boundary: an empty trace yields an empty timeline and no control return.
    #[test]
    fn timeline_empty_trace_has_no_control() {
        assert!(controller_timeline(&[], "PC").is_empty());
    }

    /// The control-state family accepts the locomotion states and rejects cutscene/death ones.
    #[test]
    fn control_state_family_boundaries() {
        for accepted in [
            "PlayerWalking",
            "playerwalking",
            "PlayerSwimming",
            "PlayerClimbing",
            "PlayerFlying",
            "PlayerGunning",
            "ClimbDoor",
            "Tyroling",
        ] {
            assert!(is_control_state(accepted), "{accepted} must be control");
        }
        for rejected in [
            "NoControl",
            "NoMove",
            "CameraView",
            "PlayingVideo",
            "WaitForFirstDisplay",
            "GameEnded",
            "GameEndedSuccess",
            "BossView",
            "",
            "PlayerWalkingImmaterial",
        ] {
            assert!(
                !is_control_state(rejected),
                "{rejected} must NOT be control"
            );
        }
    }

    /// Wait-flag decode: bit boundaries, the unknown-bit label, and the zero case.
    #[test]
    fn wait_flags_decode() {
        assert_eq!(wait_labels(0), vec!["none (flagsPaused=0)"]);
        assert_eq!(wait_labels(1), vec!["player"]);
        assert_eq!(wait_labels(2), vec!["event"]);
        // 3 = player|event, order follows the table, not the bit value.
        assert_eq!(wait_labels(3), vec!["player", "event"]);
        assert_eq!(wait_labels(2048), vec!["cadaver"]);
        // A bit outside the decoded table must be visible, never silently dropped.
        let unknown = wait_labels(1 | (1 << 13));
        assert_eq!(unknown[0], "player");
        assert!(unknown[1].starts_with("unknown-bits=0x"));
    }

    /// Failure-text summarising: kind name, first stack line, native path extraction; and the
    /// degenerate no-stack text.
    #[test]
    fn error_text_summarising() {
        let error = "script error: UnimplementedNative { path: \"XIIIPlayerController.SwitchWeapon\", index: None }\n  at XIIIPlayerController.SwitchWeapon [XIIIPlayerController0] code 0x0013\n  at XIIIPlayerController.Tick [XIIIPlayerController0] code 0x0080\n";
        assert_eq!(error_kind(error), "UnimplementedNative");
        assert_eq!(
            first_stack_line(error).as_deref(),
            Some("at XIIIPlayerController.SwitchWeapon")
        );
        assert_eq!(
            native_path(error).as_deref(),
            Some("XIIIPlayerController.SwitchWeapon")
        );
        let summary = summarize_error(error);
        assert!(summary.starts_with("UnimplementedNative | at XIIIPlayerController.SwitchWeapon"));
        // No stack at all: the summary is just the kind, never a panic.
        assert_eq!(
            summarize_error("script error: BudgetExceeded { limit: 1000000 }"),
            "BudgetExceeded"
        );
        assert_eq!(native_path("script error: DivisionByZero"), None);
    }

    /// Chain building: linear, missing link, cycle, and a start not in the exact map set.
    #[test]
    fn campaign_chain_building() {
        let exact: BTreeMap<String, String> = ["a", "b", "c", "d"]
            .into_iter()
            .map(|s| (s.to_owned(), s.to_ascii_uppercase()))
            .collect();
        let mut links: BTreeMap<String, String> = BTreeMap::new();
        links.insert("a".into(), "B.unr".into());
        links.insert("b".into(), "c".into());
        links.insert("c".into(), "d".into());
        assert_eq!(build_chain("a", &exact, &links), vec!["A", "B", "C", "D"]);
        // Missing link: c is still reachable through b's link, but the chain stops there
        // (c has no outgoing link).
        links.remove("c");
        assert_eq!(build_chain("a", &exact, &links), vec!["A", "B", "C"]);
        // Cycle: terminates with each map once (a -> b -> c -> a stops at the repeat).
        links.insert("c".into(), "a".into());
        links.insert("b".into(), "c".into());
        let chain = build_chain("a", &exact, &links);
        assert_eq!(chain, vec!["A", "B", "C"]);
        // Unknown start: empty.
        assert!(build_chain("zz", &exact, &links).is_empty());
    }

    fn survey_with(
        run_error: Option<&str>,
        control_at: Option<f64>,
        failures: usize,
        waits_on_player: bool,
    ) -> MapSurvey {
        MapSurvey {
            map: "Test00".into(),
            wall_secs: 1.0,
            ticks: Some(1),
            run_error: run_error.map(str::to_owned),
            controller: Some("XIIIPlayerController0".into()),
            initial_state: Some("NoControl".into()),
            final_state: Some("NoControl".into()),
            timeline: Vec::new(),
            control_at: control_at.map(|t| (t, "PlayerWalking".into())),
            stable_control_at: control_at.map(|t| (t, "PlayerWalking".into())),
            failures: (0..failures)
                .map(|i| (format!("Actor{i}"), "UnimplementedNative | at X.Y.Z".into()))
                .collect(),
            missing_natives: Vec::new(),
            call_depth_actors: Vec::new(),
            suspended: Vec::new(),
            blocked: Vec::new(),
            objectives: vec![ObjectiveState {
                index: 0,
                text: "goal".into(),
                completed: false,
                primary: true,
                anti_goal: false,
            }],
            cine: vec![CineRow {
                name: "CineController21".into(),
                class: "xidcine.CineController2".into(),
                is_controller: true,
                active: true,
                suspended: false,
                state: Some("PlayingSequence".into()),
                action_index: Some(3),
                total_actions: Some(9),
                current_action: Some("wait player 5".into()),
                flags_paused: Some(if waits_on_player { 1 } else { 0 }),
                wait_labels: wait_labels(if waits_on_player { 1 } else { 0 }),
                b_initialized: None,
                end_cartoon_effect: None,
                waits_on_player,
            }],
            touches: Vec::new(),
            player_pawns: 1,
            travel: Vec::new(),
            final_map: "Test00".into(),
            travelled: false,
        }
    }

    /// Result-line parsing: all fields, the "-" placeholders, and a wrong prefix.
    #[test]
    fn result_line_parsing() {
        let line = "[survey-result] map=Amos01\tclass=control+errors\twall=42.5\tcontrol=46.917\tfinal_state=PlayerWalking\tfails=1\tmiss=0\tsusp=2\tcine=3";
        let row = parse_result_line(line, 7).expect("parses");
        assert_eq!(row.order, 7);
        assert_eq!(row.map, "Amos01");
        assert_eq!(row.class, Classification::ControlErrors);
        assert_eq!(row.wall_secs, 42.5);
        assert_eq!(row.control, Some(46.917));
        assert_eq!(row.final_state.as_deref(), Some("PlayerWalking"));
        assert_eq!((row.fails, row.missing, row.susp, row.cine), (1, 0, 2, 3));
        // Placeholders parse to None/0, never to garbage.
        let empty = parse_result_line(
            "[survey-result] map=X\tclass=blocked\twall=0\tcontrol=never\tfinal_state=-\tfails=0\tmiss=0\tsusp=0\tcine=0",
            1,
        )
        .expect("parses");
        assert_eq!(empty.control, None);
        assert_eq!(empty.final_state, None);
        assert_eq!(empty.class, Classification::Blocked);
        assert!(parse_result_line("[play] not a result line", 1).is_none());
    }

    /// Classification precedence: crash beats everything; control (with/without failures) beats
    /// waiting-on-player; a cine waiting on the player with no control is waiting-on-player;
    /// everything else is blocked.
    #[test]
    fn classification_precedence() {
        assert_eq!(
            classify(&survey_with(Some("panic: boom"), None, 0, false)),
            Classification::Crash
        );
        assert_eq!(
            classify(&survey_with(None, Some(45.0), 0, false)),
            Classification::Control
        );
        assert_eq!(
            classify(&survey_with(None, Some(45.0), 2, false)),
            Classification::ControlErrors
        );
        assert_eq!(
            classify(&survey_with(None, None, 0, true)),
            Classification::WaitingOnPlayer
        );
        assert_eq!(
            classify(&survey_with(None, None, 3, false)),
            Classification::Blocked
        );
    }
}
