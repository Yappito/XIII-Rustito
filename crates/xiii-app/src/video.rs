//! `--video`: play a Bink 1 cutscene decoded by the clean-room `xiii-video` decoder, with its
//! Bink Audio track.
//!
//! The decoded RGBA frame is uploaded to a Bevy image each tick. If the decoder cannot decode a
//! frame it shows black, records the error and keeps running for the unattended duration so a
//! screenshot is still produced; the error is reported on exit.
//!
//! ## Audio and the sync rule
//!
//! Audio track 0 is decoded packet by packet ([`xiii_video::AudioDecoder`]) into a shared FIFO
//! that a custom Bevy audio source drains on the audio thread, through the same Bevy/rodio
//! output device and mixer that `--play` uses (`audio.rs` registers its own streamed source the
//! same way). Packets are decoded ahead of playback (about [`AUDIO_AHEAD_SECS`] queued), in
//! file order, independent of video decoding.
//!
//! **Sync rule:** audio sample `n` (per channel) belongs to time `n / sample_rate` and video
//! frame `i` to time `i / fps`, both from the start of the file (the convention the FFmpeg
//! oracle's demuxer applies as well; on `ubi.bik` the decoded audio lasts exactly as long as the
//! 302 video frames). The **audio clock is master**: the playback time is the number of samples
//! the mixer has pulled from the source divided by the rate, and the video shows the newest frame
//! due at that time. Device output latency after the mixer is not compensated. Before the device
//! has pulled anything the picture holds frame 0; if no sample is pulled within
//! [`AUDIO_START_TIMEOUT`] (no output device) or audio is off/absent, the wall clock is used;
//! after the audio track ends, time continues on the wall clock from the last audio time.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::audio::{AddAudioSource, Decodable, Source};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};

use xiii_video::container::BikFile;
use xiii_video::{AudioDecoder, AudioTables, BinkTables, Decoder, YuvFrame};

use crate::cli::{Audio, Options};

/// Seconds of decoded audio kept queued ahead of the mixer.
pub const AUDIO_AHEAD_SECS: f64 = 1.0;

/// How long to wait for the output device to pull the first sample before falling back to the
/// wall clock.
pub const AUDIO_START_TIMEOUT: Duration = Duration::from_millis(1500);

/// Resource holding the parsed options.
#[derive(Resource, Clone)]
pub struct VideoConfig {
    /// Runtime options (file, game dir, exit/screenshot).
    pub options: Options,
}

/// FIFO shared between the decoder (main thread) and the audio source (audio thread).
#[derive(Default)]
pub struct PcmShared {
    queue: Mutex<VecDeque<i16>>,
    /// Interleaved samples handed to the mixer.
    pulled: AtomicU64,
    /// Silent samples emitted because the queue was empty before the end.
    underrun: AtomicU64,
    /// Set once every packet has been queued.
    finished: AtomicBool,
}

impl PcmShared {
    fn lock(&self) -> std::sync::MutexGuard<'_, VecDeque<i16>> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Bevy audio asset for a cutscene's audio track; its decoder drains the shared FIFO.
#[derive(Asset, TypePath)]
pub struct BinkAudio {
    shared: Arc<PcmShared>,
    sample_rate: u32,
    channels: u16,
}

impl Decodable for BinkAudio {
    type Decoder = BinkAudioSource;

    fn decoder(&self) -> Self::Decoder {
        BinkAudioSource {
            shared: Arc::clone(&self.shared),
            local: VecDeque::new(),
            sample_rate: self.sample_rate,
            channels: self.channels,
            phase: 0,
        }
    }
}

/// The rodio source pulled by the mixer. It moves whole sample frames from the shared FIFO in
/// small batches, emits silence (in whole frames) on an underrun and ends when the FIFO is
/// empty and every packet has been queued.
pub struct BinkAudioSource {
    shared: Arc<PcmShared>,
    local: VecDeque<i16>,
    sample_rate: u32,
    channels: u16,
    /// Position inside the current interleaved frame.
    phase: u16,
}

const SOURCE_BATCH: usize = 1024;

impl Iterator for BinkAudioSource {
    type Item = bevy::audio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        if self.local.is_empty() {
            let mut q = self.shared.lock();
            let ch = usize::from(self.channels.max(1));
            let take = (q.len().min(SOURCE_BATCH) / ch) * ch;
            self.local.extend(q.drain(..take));
        }
        let ch = self.channels.max(1);
        let out = match self.local.pop_front() {
            Some(s) => {
                self.shared.pulled.fetch_add(1, Ordering::Relaxed);
                f32::from(s) / 32768.0
            }
            None if self.phase == 0 && self.shared.finished.load(Ordering::Acquire) => {
                return None;
            }
            None => {
                self.shared.underrun.fetch_add(1, Ordering::Relaxed);
                0.0
            }
        };
        self.phase = (self.phase + 1) % ch;
        Some(out)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl Source for BinkAudioSource {
    fn current_span_len(&self) -> Option<usize> {
        None
    }

    fn channels(&self) -> std::num::NonZero<u16> {
        std::num::NonZero::new(self.channels.max(1)).unwrap_or(std::num::NonZero::<u16>::MIN)
    }

    fn sample_rate(&self) -> std::num::NonZero<u32> {
        std::num::NonZero::new(self.sample_rate.max(1)).unwrap_or(std::num::NonZero::<u32>::MIN)
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Audio side of the playback state.
struct AudioPlayback {
    decoder: AudioDecoder,
    shared: Arc<PcmShared>,
    track: usize,
    next_frame: usize,
    queued: u64,
    packets: usize,
    errors: usize,
    first_error: Option<String>,
    /// When the mixer first pulled a sample.
    started: Option<Instant>,
    /// (audio time, instant) once the track has fully drained.
    drained: Option<(f64, Instant)>,
}

impl AudioPlayback {
    fn samples_per_sec(&self) -> f64 {
        f64::from(self.decoder.sample_rate()) * f64::from(self.decoder.channels())
    }

    /// Decodes packets until about [`AUDIO_AHEAD_SECS`] are queued or the file ends.
    fn feed(&mut self, data: &[u8], bik: &BikFile) {
        let ahead = (AUDIO_AHEAD_SECS * self.samples_per_sec()) as usize;
        let mut pcm = Vec::new();
        while self.next_frame < bik.frame_count() && self.shared.lock().len() + pcm.len() < ahead {
            let f = self.next_frame;
            self.next_frame += 1;
            let r = bik
                .frame_packets(data, f)
                .and_then(|p| match p.audio[self.track] {
                    Some(a) => {
                        let start = a.offset as usize;
                        let payload = &data[start..start + a.size as usize];
                        self.decoder
                            .decode_packet(payload, a.decoded_bytes, &mut pcm)
                            .map(Some)
                    }
                    None => Ok(None),
                });
            match r {
                Ok(Some(_)) => self.packets += 1,
                Ok(None) => {}
                Err(e) => {
                    self.packets += 1;
                    self.errors += 1;
                    self.first_error
                        .get_or_insert_with(|| format!("frame {f}: {e}"));
                }
            }
        }
        if !pcm.is_empty() {
            self.queued += pcm.len() as u64;
            self.shared.lock().extend(pcm);
        }
        if self.next_frame >= bik.frame_count() {
            self.shared.finished.store(true, Ordering::Release);
        }
    }

    /// Seconds of audio pulled by the mixer.
    fn pulled_secs(&self) -> f64 {
        self.shared.pulled.load(Ordering::Relaxed) as f64 / self.samples_per_sec()
    }
}

/// Where the playback time comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClockSource {
    /// Samples pulled by the mixer.
    Audio,
    /// Wall clock (audio off, absent, failed or never pulled).
    Wall,
}

/// Runtime playback state.
#[derive(Resource)]
pub struct VideoState {
    data: Vec<u8>,
    bik: BikFile,
    decoder: Decoder,
    prev: Option<YuvFrame>,
    next_frame: usize,
    handle: Handle<Image>,
    start: Instant,
    last_advance: Instant,
    shot: u8,
    shot_done: bool,
    decoded: usize,
    errors: usize,
    first_error: Option<String>,
    target_at: Option<Instant>,
    audio: Option<AudioPlayback>,
    clock: ClockSource,
    /// Largest `clock - frame time` at an upload (how late the shown frame was), seconds.
    max_late: f64,
    /// Uploaded frames.
    uploads: usize,
    /// Periodic sync log: next playback second to print.
    next_log: f64,
    /// Origin of the wall clock (reset when falling back from the audio clock).
    wall_origin: Instant,
}

#[derive(Resource, Default)]
struct ShotFlag(bool);

/// Playback plugin.
pub struct VideoPlugin {
    /// Parsed command-line options.
    pub options: Options,
}

impl Plugin for VideoPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(VideoConfig {
            options: self.options.clone(),
        })
        .init_resource::<ShotFlag>()
        .add_audio_source::<BinkAudio>()
        .add_systems(Startup, setup)
        .add_systems(Update, (advance, unattended).chain());
    }
}

fn image_from_rgba(rgba: &[u8], w: usize, h: usize) -> Image {
    Image::new(
        Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba.to_vec(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    )
}

/// Opens audio track 0 when audio is on and the file has a decodable track.
fn open_audio(
    cfg: &VideoConfig,
    game_dir: &std::path::Path,
    bik: &BikFile,
) -> Result<Option<AudioPlayback>, String> {
    if cfg.options.audio == Audio::Off {
        println!("[video] audio off (--audio off): wall clock");
        return Ok(None);
    }
    let Some(track) = bik.audio.first() else {
        println!("[video] no audio track: wall clock");
        return Ok(None);
    };
    let tables = AudioTables::from_install(game_dir).map_err(|e| e.to_string())?;
    let decoder = AudioDecoder::new(&tables, track).map_err(|e| e.to_string())?;
    println!(
        "[video] audio track 0 of {}: {} Hz x {} ch, DCT, block {} samples/channel (clean-room Bink Audio)",
        bik.audio.len(),
        decoder.sample_rate(),
        decoder.channels(),
        decoder.frame_len()
    );
    Ok(Some(AudioPlayback {
        decoder,
        shared: Arc::new(PcmShared::default()),
        track: 0,
        next_frame: 0,
        queued: 0,
        packets: 0,
        errors: 0,
        first_error: None,
        started: None,
        drained: None,
    }))
}

fn setup(
    mut commands: Commands,
    cfg: Res<VideoConfig>,
    mut images: ResMut<Assets<Image>>,
    mut bink_audio: ResMut<Assets<BinkAudio>>,
) {
    let Some(file) = cfg.options.video.clone() else {
        eprintln!("[video] no --video file");
        commands.write_message(AppExit::error());
        return;
    };
    let Some(game_dir) = cfg
        .options
        .game_dir
        .clone()
        .or_else(|| install_root_of(&file))
    else {
        eprintln!(
            "[video] --video needs --game-dir (or a file inside the installation's Video folder)"
        );
        commands.write_message(AppExit::error());
        return;
    };
    let data = match std::fs::read(&file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[video] cannot read {}: {e}", file.display());
            commands.write_message(AppExit::error());
            return;
        }
    };
    let bik = match BikFile::parse(&data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[video] {}: {e}", file.display());
            commands.write_message(AppExit::error());
            return;
        }
    };
    let tables = match BinkTables::from_install(&game_dir) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[video] {e}");
            commands.write_message(AppExit::error());
            return;
        }
    };
    let width = bik.header.width as usize;
    let height = bik.header.height as usize;
    let black = vec![0u8; width * height * 4];
    let handle = images.add(image_from_rgba(&black, width, height));

    commands.spawn(Camera2d);
    commands.spawn((
        Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            ..default()
        },
        ImageNode {
            image: handle.clone(),
            ..default()
        },
    ));

    let decoder = Decoder::new(tables);
    println!(
        "[video] {} {}x{} {} frames {:.3} fps (decoder clean-room Bink 1)",
        file.display(),
        width,
        height,
        bik.frame_count(),
        bik.header.fps()
    );

    // Audio: decode the first second now so the mixer has data when it starts pulling.
    let mut audio = match open_audio(&cfg, &game_dir, &bik) {
        Ok(a) => a,
        Err(e) => {
            // A required decode failure is visible, not silent: reported now and on exit.
            eprintln!("[video] audio unavailable, using the wall clock: {e}");
            None
        }
    };
    if let Some(a) = audio.as_mut() {
        a.feed(&data, &bik);
        let asset = BinkAudio {
            shared: Arc::clone(&a.shared),
            sample_rate: a.decoder.sample_rate(),
            channels: a.decoder.channels(),
        };
        commands.spawn((
            Name::new("bink audio track 0"),
            AudioPlayer(bink_audio.add(asset)),
            PlaybackSettings::ONCE,
        ));
    }

    let now = Instant::now();
    let clock = if audio.is_some() {
        ClockSource::Audio
    } else {
        ClockSource::Wall
    };
    commands.insert_resource(VideoState {
        data,
        bik,
        decoder,
        prev: None,
        next_frame: 0,
        handle,
        start: now,
        last_advance: now,
        shot: 0,
        shot_done: false,
        decoded: 0,
        errors: 0,
        first_error: None,
        target_at: None,
        audio,
        clock,
        max_late: 0.0,
        uploads: 0,
        next_log: 1.0,
        wall_origin: now,
    });
}

/// The installation root for a cutscene stored as `<root>/Video/<file>.bik`.
fn install_root_of(file: &std::path::Path) -> Option<std::path::PathBuf> {
    let video_dir = file.parent()?;
    let name = video_dir
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    if name != "video" {
        return None;
    }
    video_dir.parent().map(std::path::Path::to_path_buf)
}

/// Current playback time in seconds (see the module docs for the rule).
fn playback_time(state: &mut VideoState) -> f64 {
    let wall = state.wall_origin.elapsed().as_secs_f64();
    if state.clock == ClockSource::Wall {
        return wall;
    }
    let Some(a) = state.audio.as_mut() else {
        state.clock = ClockSource::Wall;
        return wall;
    };
    let pulled = a.shared.pulled.load(Ordering::Relaxed);
    if pulled == 0 {
        if state.start.elapsed() >= AUDIO_START_TIMEOUT {
            println!(
                "[video] the audio device pulled no samples within {:?}; switching to the wall clock",
                AUDIO_START_TIMEOUT
            );
            state.clock = ClockSource::Wall;
            state.wall_origin = Instant::now();
        }
        return 0.0;
    }
    a.started.get_or_insert_with(Instant::now);
    let t = a.pulled_secs();
    // After the last queued sample has been pulled, continue on the wall clock.
    if a.shared.finished.load(Ordering::Acquire) && pulled >= a.queued {
        let (t0, at) = *a.drained.get_or_insert_with(|| (t, Instant::now()));
        return t0 + at.elapsed().as_secs_f64();
    }
    t
}

fn advance(
    cfg: Res<VideoConfig>,
    state: Option<ResMut<VideoState>>,
    mut images: ResMut<Assets<Image>>,
) {
    // Setup failed (and requested exit): nothing to play.
    let Some(mut state) = state else {
        return;
    };
    let state = &mut *state;
    if let Some(a) = state.audio.as_mut() {
        a.feed(&state.data, &state.bik);
    }
    // Frames are due by playback time; every due frame is decoded (inter frames depend on their
    // predecessor), only the newest one is uploaded.
    let fps = state.bik.header.fps().max(1.0);
    let now = playback_time(state);
    let due = ((now * fps) as usize + 1).min(state.bik.frame_count());
    let mut newest = None;
    while state.next_frame < due {
        let i = state.next_frame;
        state.next_frame += 1;
        let video = match xiii_video::frame_video(&state.data, &state.bik, i) {
            Ok(v) => v,
            Err(e) => {
                state.errors += 1;
                state.first_error.get_or_insert_with(|| e.to_string());
                continue;
            }
        };
        match state
            .decoder
            .decode_frame(video, &state.bik.header, state.prev.as_ref())
        {
            Ok((frame, _stats)) => {
                state.prev = Some(frame);
                state.decoded += 1;
                newest = Some(i);
            }
            Err(e) => {
                state.errors += 1;
                state
                    .first_error
                    .get_or_insert_with(|| format!("frame {i}: {e}"));
            }
        }
    }
    if let Some(i) = newest
        && let Some(frame) = state.prev.as_ref()
    {
        let rgba = frame.to_rgba();
        if let Some(mut img) = images.get_mut(&state.handle) {
            img.data = Some(rgba);
        }
        state.last_advance = Instant::now();
        state.uploads += 1;
        // Lateness of the shown frame against the clock *after* decoding it.
        let after = playback_time(state);
        state.max_late = state.max_late.max(after - i as f64 / fps);
    }
    if cfg.options.exit_after_secs.is_some() && now >= state.next_log {
        state.next_log = now.floor() + 1.0;
        let wall = state.start.elapsed().as_secs_f64();
        match state.audio.as_ref() {
            Some(a) => println!(
                "[video] sync t={now:.3}s clock={:?} frame={} (frame time {:.3}s) audio pulled {:.3}s queued {:.3}s underrun {} wall {wall:.3}s",
                state.clock,
                state.next_frame.saturating_sub(1),
                state.next_frame.saturating_sub(1) as f64 / fps,
                a.pulled_secs(),
                a.queued as f64 / a.samples_per_sec(),
                a.shared.underrun.load(Ordering::Relaxed),
            ),
            None => println!(
                "[video] sync t={now:.3}s clock={:?} frame={} wall {wall:.3}s",
                state.clock,
                state.next_frame.saturating_sub(1),
            ),
        }
    }
}

fn unattended(
    mut commands: Commands,
    cfg: Res<VideoConfig>,
    state: Option<ResMut<VideoState>>,
    flag: Res<ShotFlag>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(mut state) = state else {
        return;
    };
    let Some(secs) = cfg.options.exit_after_secs else {
        return;
    };
    let elapsed = state.start.elapsed().as_secs_f32();
    if flag.0 {
        state.shot_done = true;
    }
    if let Some(path) = &cfg.options.screenshot
        && state.shot == 0
        && elapsed >= secs * 0.75
    {
        let path = path.clone();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path))
            .observe(|_: On<ScreenshotCaptured>, mut f: ResMut<ShotFlag>| f.0 = true);
        state.shot = 1;
    }
    if elapsed < secs {
        return;
    }
    let reached = *state.target_at.get_or_insert_with(Instant::now);
    if state.shot == 1 && !state.shot_done && reached.elapsed() < Duration::from_secs(5) {
        return;
    }
    println!(
        "[video] exit after {:.1}s: {} frames decoded, {} errors, {} total frames{}",
        elapsed,
        state.decoded,
        state.errors,
        state.bik.frame_count(),
        match &state.first_error {
            Some(e) => format!("; first error: {e}"),
            None => String::new(),
        }
    );
    let fps = state.bik.header.fps().max(1.0);
    let shown = state.next_frame.saturating_sub(1);
    match state.audio.as_ref() {
        Some(a) => {
            let pulled = a.pulled_secs();
            let rate = a
                .started
                .map(|s| pulled / s.elapsed().as_secs_f64().max(1e-9))
                .unwrap_or(0.0);
            println!(
                "[video] audio exit: clock={:?} packets {} errors {} queued {:.3}s pulled {:.3}s \
                 (pull rate {:.3}x real time since first pull) underrun samples {}; last shown frame {} \
                 (time {:.3}s); max frame lateness at upload {:.1} ms over {} uploads{}",
                state.clock,
                a.packets,
                a.errors,
                a.queued as f64 / a.samples_per_sec(),
                pulled,
                rate,
                a.shared.underrun.load(Ordering::Relaxed),
                shown,
                shown as f64 / fps,
                state.max_late * 1000.0,
                state.uploads,
                match &a.first_error {
                    Some(e) => format!("; first audio error: {e}"),
                    None => String::new(),
                }
            );
        }
        None => println!(
            "[video] audio exit: clock={:?} (no audio); max frame lateness at upload {:.1} ms over {} uploads",
            state.clock,
            state.max_late * 1000.0,
            state.uploads
        ),
    }
    exit.write(AppExit::Success);
}
