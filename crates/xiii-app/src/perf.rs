//! Frame-time and per-system performance measurement (`--perf`).
//!
//! This module is deliberately independent of the viewer/play code: [`Perf`] is a plain
//! accumulator every hot system writes into, and [`perf_report_system`] prints a table on an
//! interval and once more at exit. Bevy's own [`FrameTimeDiagnosticsPlugin`] supplies the
//! frame-time samples (raw milliseconds) and [`EntityCountDiagnosticsPlugin`] the live entity
//! count, so the numbers come from the same source as any other Bevy diagnostic.
//!
//! Claim labels for the report: the frame-time percentiles are **measured** from the diagnostic
//! history; the per-system rows are **measured** wall-clock `Instant` deltas around the named
//! calls; the render row is a **counted** asset/entity census, not a GPU capture (Bevy exposes no
//! draw-call counter here).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use bevy::diagnostic::{
    DiagnosticsStore, EntityCountDiagnosticsPlugin, FrameTimeDiagnosticsPlugin,
};
use bevy::prelude::*;

use xiii_script::vm::NativeProfile;

/// Per-system perf configuration from the CLI.
#[derive(Resource, Clone, Copy, Debug)]
pub struct PerfConfig {
    /// `--perf` was given.
    pub enabled: bool,
    /// Seconds between tables.
    pub interval: f32,
    /// `--perf-natives` was given (individual VM natives are timed).
    pub natives: bool,
    /// `--benchmark N`: render `N` frames as fast as possible, print the final table and exit.
    pub benchmark: Option<u32>,
    /// `--benchmark`: warm-up frames discarded before the measured frames.
    pub benchmark_warmup: u32,
}

impl Default for PerfConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            interval: 5.0,
            natives: false,
            benchmark: None,
            benchmark_warmup: 0,
        }
    }
}

/// One accumulated timer: total wall-clock microseconds and the number of calls.
#[derive(Debug, Default, Clone, Copy)]
pub struct Counter {
    /// Total time in microseconds.
    pub micros: u64,
    /// Number of calls observed.
    pub calls: u64,
}

/// A counted census of what the renderer holds, filled in by the scene setup. These are entity
/// and asset counts (Bevy exposes no public draw-call count), labelled as such in the report.
#[derive(Resource, Debug, Default, Clone, Copy)]
pub struct RenderStats {
    /// Imported scene objects.
    pub objects: usize,
    /// Mesh entities spawned (a proxy for draw submissions before culling/batching).
    pub draw_entities: usize,
    /// Distinct `Mesh` assets registered.
    pub mesh_assets: usize,
    /// Distinct `StandardMaterial` assets registered.
    pub material_assets: usize,
    /// Triangles across the spawned mesh entities.
    pub triangles: usize,
}

impl RenderStats {
    /// Combines a scene-part census into one row.
    pub fn new(
        objects: usize,
        draw_entities: usize,
        mesh_assets: usize,
        material_assets: usize,
        triangles: usize,
    ) -> Self {
        Self {
            objects,
            draw_entities,
            mesh_assets,
            material_assets,
            triangles,
        }
    }
}

/// The `--perf` accumulator resource. Always present; only populated when enabled.
#[derive(Resource)]
pub struct Perf {
    /// Configuration.
    pub config: PerfConfig,
    /// Application start.
    pub start: Instant,
    /// Time of the last printed table.
    pub last_report: Instant,
    /// Timer accumulators since the last table.
    pub interval: BTreeMap<&'static str, Counter>,
    /// Timer accumulators since the start.
    pub total: BTreeMap<&'static str, Counter>,
    /// Per-call samples in microseconds since the start (for per-counter p99; bounded by
    /// `sample_cap`, so a long run does not grow without limit).
    pub total_samples: BTreeMap<&'static str, Vec<f64>>,
    /// How many per-call samples are retained per counter. Beyond this, later samples are
    /// dropped (the counter's `calls` still counts all of them). Enough for a stable p99 over
    /// the windows this tool reports.
    pub sample_cap: usize,
    /// Fixed steps since the last table.
    pub interval_steps: u64,
    /// Fixed steps since the start.
    pub total_steps: u64,
    /// Frame count at the last table (to slice the diagnostic history).
    pub last_frame_count: u64,
    /// A final table was requested (the unattended system is exiting).
    pub final_requested: bool,
    /// The final table was already printed.
    pub final_printed: bool,
    /// Benchmark: the frame count at which the measured window begins (after warm-up).
    pub benchmark_start_frame: Option<u64>,
}

impl Default for Perf {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            config: PerfConfig::default(),
            start: now,
            last_report: now,
            interval: BTreeMap::new(),
            total: BTreeMap::new(),
            total_samples: BTreeMap::new(),
            sample_cap: 8192,
            interval_steps: 0,
            total_steps: 0,
            last_frame_count: 0,
            final_requested: false,
            final_printed: false,
            benchmark_start_frame: None,
        }
    }
}

impl Perf {
    /// A disabled/enabled accumulator.
    pub fn new(config: PerfConfig) -> Self {
        Self {
            config,
            ..Default::default()
        }
    }

    /// True when `--perf` is active.
    pub fn enabled(&self) -> bool {
        self.config.enabled
    }

    /// Adds `d` to the named counter (both the interval and the total accumulation).
    pub fn add(&mut self, name: &'static str, d: Duration) {
        if !self.enabled() {
            return;
        }
        let micros = d.as_micros() as u64;
        for map in [&mut self.interval, &mut self.total] {
            let c = map.entry(name).or_default();
            c.micros += micros;
            c.calls += 1;
        }
        let samples = self.total_samples.entry(name).or_default();
        if samples.len() < self.sample_cap {
            samples.push(d.as_micros() as f64);
        }
    }

    /// Convenience: `add` from a start [`Instant`].
    pub fn span(&mut self, name: &'static str, t0: Instant) {
        self.add(name, t0.elapsed());
    }

    /// Increments the fixed-step counters.
    pub fn step(&mut self) {
        if !self.enabled() {
            return;
        }
        self.interval_steps += 1;
        self.total_steps += 1;
    }

    /// Requests a final table (called by the unattended exit path).
    pub fn request_final(&mut self) {
        self.final_requested = true;
    }
}

/// The `--perf` plugin: installs the diagnostics and the reporting system.
pub struct PerfPlugin {
    /// Configuration from the CLI.
    pub config: PerfConfig,
}

impl Plugin for PerfPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Perf::new(self.config));
        if self.config.enabled || self.config.benchmark.is_some() {
            app.add_plugins(FrameTimeDiagnosticsPlugin::new(200_000))
                .add_plugins(EntityCountDiagnosticsPlugin::new(4));
        }
        if self.config.enabled {
            app.add_systems(Last, perf_report_system);
        }
        if self.config.benchmark.is_some() {
            app.add_systems(Last, benchmark_system);
        }
    }
}

/// `--benchmark N`: counts warm-up then measured frames, prints a frame-time table for the
/// measured window only, and exits. The window excludes the warm-up so first-frame shader and
/// asset uploads do not skew the percentiles.
fn benchmark_system(
    mut perf: ResMut<Perf>,
    store: Res<DiagnosticsStore>,
    render: Option<Res<RenderStats>>,
    particles: Option<Res<crate::viewer::particles::ParticleStats>>,
    particle_data: Option<Res<crate::viewer::particles::ParticleRenderData>>,
    particle_emitters: Query<&crate::viewer::particles::ParticleEmitterRender>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(n) = perf.config.benchmark else {
        return;
    };
    let frame_count = store
        .get(&FrameTimeDiagnosticsPlugin::FRAME_COUNT)
        .and_then(|d| d.value())
        .unwrap_or(0.0) as u64;
    match perf.benchmark_start_frame {
        None => {
            if frame_count >= u64::from(perf.config.benchmark_warmup) {
                perf.benchmark_start_frame = Some(frame_count);
            }
            return;
        }
        Some(start) => {
            let measured = frame_count.saturating_sub(start);
            if measured < u64::from(n) {
                return;
            }
        }
    }
    // The measured window is the last `n` frame-time samples.
    let mut samples: Vec<f64> = store
        .get(&FrameTimeDiagnosticsPlugin::FRAME_TIME)
        .map(|d| d.values().copied().collect())
        .unwrap_or_default();
    if samples.len() > n as usize {
        samples.drain(0..samples.len() - n as usize);
    }
    let entities = store
        .get(&EntityCountDiagnosticsPlugin::ENTITY_COUNT)
        .and_then(|d| d.value())
        .unwrap_or(0.0);
    println!(
        "[perf] ===== BENCHMARK {n} frames (warm-up {}) | entities {entities:.0} =====",
        perf.config.benchmark_warmup
    );
    match stats(&samples) {
        Some((n, mean, p50, p95, p99, min, max)) => {
            println!(
                "[perf]   frame ms: mean {mean:6.2}  p50 {p50:6.2}  p95 {p95:6.2}  p99 {p99:6.2}  \
                 min {min:6.2}  max {max:6.2}  (samples {n}) | p50 {:.1} fps | mean {:.1} fps",
                1000.0 / p50.max(1e-9),
                1000.0 / mean.max(1e-9),
            );
        }
        None => println!("[perf]   frame ms: no samples"),
    }
    if let Some(r) = render {
        println!(
            "[perf]   render: objects {} | mesh entities (pre-cull draw submissions) {} | mesh assets {} | material assets {} | triangles {}",
            r.objects, r.draw_entities, r.mesh_assets, r.material_assets, r.triangles
        );
    }
    if let Some(p) = particles {
        println!(
            "[perf]   particles: emitters {} | authored-enabled {} | VM-enabled {} | live {}",
            p.emitters, p.authored_enabled_emitters, p.enabled_emitters, p.live_particles
        );
        if let Some(data) = particle_data {
            let mut authored_enabled = Vec::new();
            let mut vm_enabled = Vec::new();
            for emitter in &particle_emitters {
                let Some(system) = data.systems.get(emitter.system) else {
                    continue;
                };
                let Some(desc) = system.emitters.get(emitter.emitter) else {
                    continue;
                };
                if xiii_world::particles::initially_enabled(system, desc) {
                    authored_enabled.push(desc.name.as_str());
                }
                if emitter.sim.enabled {
                    vm_enabled.push(desc.name.as_str());
                }
            }
            println!(
                "[perf]   particle emitter paths authored-enabled: {}",
                authored_enabled.join(", ")
            );
            println!(
                "[perf]   particle emitter paths VM-enabled: {}",
                vm_enabled.join(", ")
            );
        }
    }
    perf.final_requested = true;
    exit.write(AppExit::Success);
}

/// Returns `(count, mean, p50, p95, p99, min, max)` in milliseconds for `values`.
fn stats(values: &[f64]) -> Option<(usize, f64, f64, f64, f64, f64, f64)> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let mean = sorted.iter().sum::<f64>() / sorted.len() as f64;
    let pct = |p: f64| -> f64 {
        let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
        sorted[idx.min(sorted.len() - 1)]
    };
    Some((
        sorted.len(),
        mean,
        pct(50.0),
        pct(95.0),
        pct(99.0),
        sorted[0],
        sorted[sorted.len() - 1],
    ))
}

/// The `p`-th percentile (0-100) in microseconds of `values`, using the same linear index rule
/// as [`stats`]; `None` for an empty slice.
fn percentile_us(values: &[f64], p: f64) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    let mut sorted: Vec<f64> = values.to_vec();
    sorted.sort_by(f64::total_cmp);
    let idx = ((p / 100.0) * (sorted.len() as f64 - 1.0)).round() as usize;
    Some(sorted[idx.min(sorted.len() - 1)])
}

/// Formats one counter row: `name`, per-call mean and p99 microseconds, total seconds spent in
/// the window, calls, and the share of the window as a percentage.
fn counter_row(
    name: &str,
    c: &Counter,
    window_secs: f64,
    samples: Option<&[f64]>,
    steps: u64,
) -> String {
    let total_secs = c.micros as f64 / 1e6;
    let pct = if window_secs > 0.0 {
        total_secs / window_secs * 100.0
    } else {
        0.0
    };
    let per_call = if c.calls > 0 {
        c.micros as f64 / c.calls as f64
    } else {
        0.0
    };
    let calls_per_sec = if window_secs > 0.0 {
        c.calls as f64 / window_secs
    } else {
        0.0
    };
    let p99 = samples.and_then(|s| percentile_us(s, 99.0)).unwrap_or(0.0);
    let _ = steps;
    format!(
        "  {name:<20} {per_call:>9.1} us  p99 {p99:>9.1} us  {total_secs:>8.3} s  {pct:>6.1}%  {calls_per_sec:>9.1}/s  (calls {})",
        c.calls
    )
}

fn print_profile(profile: Option<&NativeProfile>, header: &str) {
    let Some(p) = profile else {
        return;
    };
    if !p.enabled {
        return;
    }
    println!("[perf] {header} VM section totals (microseconds, cumulative):");
    println!(
        "  timers {}  animation {}  interpolation/movers {}  state-code {}  tick-dispatch {}  player-tick-dispatch {}  natives {}",
        p.timers_micros,
        p.animation_micros,
        p.movers_micros,
        p.state_micros,
        p.tick_dispatch_micros,
        p.player_tick_dispatch_micros,
        p.natives_micros
    );
    println!(
        "  host: player-write {}  touch-refresh {}  event-drain/touch {}  render-sync {}  mover-states {}  ai-perception {}",
        p.player_write_micros,
        p.touch_micros,
        p.events_micros,
        p.sync_micros,
        p.mover_states_micros,
        p.perception_micros
    );
    println!(
        "  tick-resolution: {} lookups, {} memo hits | spawns {} (objects reallocs {})",
        p.tick_lookups, p.tick_cache_hits, p.spawn_count, p.objects_reallocs
    );
    let mut ticks: Vec<(&String, u64)> = p.tick_fns.iter().map(|(k, v)| (k, *v)).collect();
    ticks.sort_by_key(|a| std::cmp::Reverse(a.1));
    if !ticks.is_empty() {
        println!("[perf] {header} top per-frame Tick executions by cumulative time:");
        for (name, micros) in ticks.iter().take(10) {
            println!("  {name:<44} {:>10.3} ms", *micros as f64 / 1000.0);
        }
    }
    let mut natives: Vec<(&String, u64, u64)> = p
        .micros
        .iter()
        .map(|(k, v)| (k, *v, *p.calls.get(k).unwrap_or(&0)))
        .collect();
    natives.sort_by_key(|a| std::cmp::Reverse(a.1));
    println!("[perf] {header} top VM natives by cumulative time:");
    for (name, micros, calls) in natives.iter().take(10) {
        println!(
            "  {name:<44} {:>10.3} ms  {:>9} calls  {:>8.1} us/call",
            *micros as f64 / 1000.0,
            calls,
            if *calls > 0 {
                *micros as f64 / *calls as f64
            } else {
                0.0
            }
        );
    }
}

fn print_table(
    perf: &mut Perf,
    store: &DiagnosticsStore,
    render: Option<&RenderStats>,
    profile: Option<&NativeProfile>,
    final_report: bool,
) {
    let now = Instant::now();
    let window = if final_report {
        now.duration_since(perf.start)
    } else {
        now.duration_since(perf.last_report)
    };
    let window_secs = window.as_secs_f64().max(1e-9);

    let frame_count = store
        .get(&FrameTimeDiagnosticsPlugin::FRAME_COUNT)
        .and_then(|d| d.value())
        .unwrap_or(0.0) as u64;
    let frame_time = store.get(&FrameTimeDiagnosticsPlugin::FRAME_TIME);
    let n = if final_report {
        usize::MAX
    } else {
        frame_count.saturating_sub(perf.last_frame_count) as usize
    };
    let mut samples: Vec<f64> = frame_time
        .map(|d| d.values().copied().collect())
        .unwrap_or_default();
    if !final_report && n > 0 && samples.len() > n {
        samples.drain(0..samples.len() - n);
    }
    let entities = store
        .get(&EntityCountDiagnosticsPlugin::ENTITY_COUNT)
        .and_then(|d| d.value())
        .unwrap_or(0.0);

    // The frame-time diagnostic records nothing on the very first frame (Real delta 0), and the
    // first table can fire immediately after a long startup. Wait for a sample rather than
    // printing an empty table and resetting the window.
    let st = stats(&samples);
    if st.is_none() && !final_report {
        return;
    }
    println!(
        "[perf] ===== {label} {elapsed:.1}s since {since} | window {window:.2}s | {fps:.1} fps | entities {entities:.0} =====",
        label = if final_report { "FINAL" } else { "t+" },
        elapsed = perf.start.elapsed().as_secs_f64(),
        since = if final_report { "start" } else { "last table" },
        window = window_secs,
        fps = 1.0 / (st.map_or(0.0, |s| s.1) / 1000.0).max(1e-9),
        entities = entities,
    );
    match st {
        Some((n, mean, p50, p95, p99, min, max)) => println!(
            "[perf]   frame ms: mean {mean:6.2}  p50 {p50:6.2}  p95 {p95:6.2}  p99 {p99:6.2}  \
             min {min:6.2}  max {max:6.2}  (samples {n})"
        ),
        None => println!("[perf]   frame ms: no samples yet"),
    }
    if let Some(r) = render {
        println!(
            "[perf]   render: objects {} | mesh entities (pre-cull draw submissions) {} | mesh assets {} | material assets {} | triangles {}",
            r.objects, r.draw_entities, r.mesh_assets, r.material_assets, r.triangles
        );
    }
    let counters = if final_report {
        &perf.total
    } else {
        &perf.interval
    };
    let steps = if final_report {
        perf.total_steps
    } else {
        perf.interval_steps
    };
    if !counters.is_empty() {
        println!(
            "[perf]   system timers (mean/p99 per call, total, % of {window_secs:.1}s window):"
        );
        for (name, c) in counters {
            println!(
                "{}",
                counter_row(
                    name,
                    c,
                    window_secs,
                    perf.total_samples.get(name).map(Vec::as_slice),
                    steps
                )
            );
        }
        println!("[perf]   fixed steps in window: {steps}");
    }
    if final_report {
        print_profile(profile, "FINAL");
    }

    perf.last_report = now;
    perf.last_frame_count = frame_count;
    perf.interval.clear();
    perf.interval_steps = 0;
}

/// `Last` system: prints the interval table, or the final table when requested.
fn perf_report_system(
    mut perf: ResMut<Perf>,
    store: Res<DiagnosticsStore>,
    render: Option<Res<RenderStats>>,
    session: Option<NonSend<Result<crate::play::session::Session, String>>>,
) {
    if !perf.enabled() {
        return;
    }
    let profile = if perf.config.natives {
        session
            .as_deref()
            .and_then(|s| s.as_ref().ok())
            .map(|s| s.vm().native_profile())
    } else {
        None
    };
    if perf.final_requested && !perf.final_printed {
        perf.final_printed = true;
        print_table(&mut perf, &store, render.as_deref(), profile, true);
        return;
    }
    if perf.last_report.elapsed().as_secs_f32() >= perf.config.interval {
        print_table(&mut perf, &store, render.as_deref(), profile, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percentile_us_empty_is_none() {
        assert!(percentile_us(&[], 99.0).is_none());
    }

    #[test]
    fn percentile_us_matches_linear_index() {
        // 100 samples 0..=99: the linear index rule round(p/100*(n-1)) gives 99 at p=100,
        // 0 at p=0, and 98 at p=99.
        let values: Vec<f64> = (0..100).map(|v| v as f64).collect();
        assert_eq!(percentile_us(&values, 0.0), Some(0.0));
        assert_eq!(percentile_us(&values, 100.0), Some(99.0));
        assert_eq!(percentile_us(&values, 99.0), Some(98.0));
    }

    #[test]
    fn percentile_us_single_sample_collapses() {
        assert_eq!(percentile_us(&[42.0], 99.0), Some(42.0));
    }

    #[test]
    fn counter_row_reports_p99_tail_not_just_mean() {
        // 98 calls at 10 us and two outliers (900, 1000). Mean is 28.8 us; p99 (index 98) is
        // 900 us. A row that only showed the mean would hide the tail.
        let mut c = Counter::default();
        let mut samples = vec![10.0; 98];
        c.micros += 98 * 10;
        c.calls += 98;
        for v in [900.0, 1000.0] {
            c.micros += v as u64;
            c.calls += 1;
            samples.push(v);
        }
        let row = counter_row("vm_step", &c, 20.0, Some(&samples), 100);
        assert!(row.contains("vm_step"), "row: {row}");
        assert!(row.contains("28.8 us"), "row: {row}");
        assert!(row.contains("900.0 us"), "row: {row}");
        // No samples (a counter only fed through `span` before samples existed) still prints.
        let row = counter_row("vm_step", &c, 20.0, None, 100);
        assert!(row.contains("p99"), "row: {row}");
    }

    #[test]
    fn add_records_samples_up_to_cap() {
        let mut p = Perf::default();
        p.config.enabled = true;
        p.sample_cap = 3;
        for i in 0..5 {
            p.add("x", Duration::from_micros(i as u64));
        }
        let c = p.total.get("x").unwrap();
        assert_eq!(c.calls, 5);
        // Samples are capped; the first `cap` are kept.
        assert_eq!(p.total_samples.get("x").unwrap().len(), 3);
        assert_eq!(p.total_samples.get("x").unwrap(), &[0.0, 1.0, 2.0]);
    }
}
