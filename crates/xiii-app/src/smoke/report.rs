//! Unattended-run control and the stdout report.
//!
//! With `--frames N` or `--exit-after-secs S` the app triggers the tone once,
//! optionally captures a screenshot, then exits with `AppExit::Success` and
//! prints adapter/backend, resolution, present mode, frame-time statistics and
//! whether audio playback started.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use bevy::prelude::*;
use bevy::render::renderer::RenderAdapterInfo;
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};
use bevy::window::PrimaryWindow;

use super::audio::{PlayTone, ToneStats};
use crate::cli::Options;

/// Frames excluded from the steady-state average (pipeline compilation,
/// swapchain creation, first audio device open).
const WARMUP_FRAMES: usize = 30;
/// Frame on which an unattended run requests the tone.
const TONE_FRAME: u64 = 15;
/// Grace period after the target for pending screenshot/audio work.
const GRACE: Duration = Duration::from_secs(3);

#[derive(Resource, Debug, Clone)]
pub struct RunConfig {
    pub frames: Option<u32>,
    pub exit_after: Option<Duration>,
    pub screenshot: Option<PathBuf>,
}

impl RunConfig {
    pub fn from_options(o: &Options) -> Self {
        Self {
            frames: o.frames,
            exit_after: o.exit_after_secs.map(Duration::from_secs_f32),
            screenshot: o.screenshot.clone(),
        }
    }

    fn unattended(&self) -> bool {
        self.frames.is_some() || self.exit_after.is_some()
    }
}

#[derive(Resource)]
struct RunState {
    start: Instant,
    frame: u64,
    frame_times: Vec<f32>,
    adapter_logged: bool,
    tone_requested: bool,
    screenshot: ShotState,
    target_reached_at: Option<Instant>,
    done: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShotState {
    NotRequested,
    Pending,
    Saved,
}

#[derive(Resource, Default)]
struct ScreenshotDone(bool);

pub struct ReportPlugin;

impl Plugin for ReportPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(RunState {
            start: Instant::now(),
            frame: 0,
            frame_times: Vec::new(),
            adapter_logged: false,
            tone_requested: false,
            screenshot: ShotState::NotRequested,
            target_reached_at: None,
            done: false,
        })
        .init_resource::<ScreenshotDone>()
        .add_systems(Update, (log_adapter_once, drive_unattended).chain());
    }
}

fn log_adapter_once(adapter: Option<Res<RenderAdapterInfo>>, mut state: ResMut<RunState>) {
    if state.adapter_logged {
        return;
    }
    if let Some(info) = adapter {
        println!(
            "[smoke] adapter: {} | backend: {:?} | type: {:?} | driver: {} {}",
            info.name, info.backend, info.device_type, info.driver, info.driver_info
        );
        state.adapter_logged = true;
    }
}

#[allow(clippy::too_many_arguments)]
fn drive_unattended(
    mut commands: Commands,
    cfg: Res<RunConfig>,
    time: Res<Time<Real>>,
    mut state: ResMut<RunState>,
    shot_done: Res<ScreenshotDone>,
    tone: Res<ToneStats>,
    adapter: Option<Res<RenderAdapterInfo>>,
    window: Single<&Window, With<PrimaryWindow>>,
    mut tone_out: MessageWriter<PlayTone>,
    mut exit: MessageWriter<AppExit>,
) {
    if !cfg.unattended() || state.done {
        return;
    }
    state.frame += 1;
    if state.frame > 1 {
        state.frame_times.push(time.delta_secs());
    }
    let elapsed = state.start.elapsed();

    if !state.tone_requested && state.frame >= TONE_FRAME {
        tone_out.write(PlayTone);
        state.tone_requested = true;
    }

    // Progress in [0, 1] towards whichever target is set.
    let progress = match (cfg.frames, cfg.exit_after) {
        (Some(n), _) => state.frame as f32 / n as f32,
        (None, Some(d)) => elapsed.as_secs_f32() / d.as_secs_f32(),
        (None, None) => unreachable!(),
    };

    if shot_done.0 {
        state.screenshot = ShotState::Saved;
    }
    if let Some(path) = &cfg.screenshot
        && state.screenshot == ShotState::NotRequested
        && progress >= 0.75
    {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path.clone()))
            .observe(
                |_: On<ScreenshotCaptured>, mut done: ResMut<ScreenshotDone>| {
                    done.0 = true;
                },
            );
        state.screenshot = ShotState::Pending;
    }

    if progress < 1.0 {
        return;
    }
    let reached = *state.target_reached_at.get_or_insert_with(Instant::now);
    let pending_shot = state.screenshot == ShotState::Pending;
    let pending_audio = state.tone_requested && tone.started == 0;
    if (pending_shot || pending_audio) && reached.elapsed() < GRACE {
        return;
    }

    state.done = true;
    print_report(&state, &cfg, &tone, adapter.as_deref(), &window, elapsed);
    exit.write(AppExit::Success);
}

fn print_report(
    state: &RunState,
    cfg: &RunConfig,
    tone: &ToneStats,
    adapter: Option<&RenderAdapterInfo>,
    window: &Window,
    elapsed: Duration,
) {
    let all = stats(&state.frame_times);
    let steady = stats(state.frame_times.get(WARMUP_FRAMES..).unwrap_or(&[]));
    println!("[smoke] ---- report ----");
    match adapter {
        Some(a) => println!(
            "[smoke] adapter: {} | backend: {:?} | type: {:?} | vendor 0x{:04x} device 0x{:04x} | driver: {} {}",
            a.name, a.backend, a.device_type, a.vendor, a.device, a.driver, a.driver_info
        ),
        None => println!("[smoke] adapter: <RenderAdapterInfo not available>"),
    }
    println!(
        "[smoke] window: logical {}x{} | physical {}x{} | scale {:.2} | present {:?}",
        window.width(),
        window.height(),
        window.physical_width(),
        window.physical_height(),
        window.scale_factor(),
        window.present_mode
    );
    println!(
        "[smoke] frames: {} in {:.2}s | build: {}",
        state.frame,
        elapsed.as_secs_f32(),
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
    for (label, s) in [("all", all), ("steady", steady)] {
        match s {
            Some(s) => println!(
                "[smoke] frametime {label}: avg {:.3} ms ({:.1} fps) | min {:.3} ms | max {:.3} ms | n={}",
                s.avg * 1e3,
                1.0 / s.avg,
                s.min * 1e3,
                s.max * 1e3,
                s.n
            ),
            None => println!("[smoke] frametime {label}: <no samples>"),
        }
    }
    println!(
        "[smoke] audio: requested {} | playback started {} | {}",
        tone.requested,
        tone.started,
        if tone.started > 0 {
            "OK"
        } else {
            "NOT STARTED"
        }
    );
    match (&cfg.screenshot, state.screenshot) {
        (Some(p), ShotState::Saved) => println!("[smoke] screenshot: saved {}", p.display()),
        (Some(p), s) => println!("[smoke] screenshot: {s:?} (not confirmed) {}", p.display()),
        (None, _) => {}
    }
}

#[derive(Debug, Clone, Copy)]
struct Stats {
    avg: f32,
    min: f32,
    max: f32,
    n: usize,
}

fn stats(xs: &[f32]) -> Option<Stats> {
    if xs.is_empty() {
        return None;
    }
    let sum: f32 = xs.iter().sum();
    Some(Stats {
        avg: sum / xs.len() as f32,
        min: xs.iter().copied().fold(f32::INFINITY, f32::min),
        max: xs.iter().copied().fold(0.0, f32::max),
        n: xs.len(),
    })
}
