//! Bink 1 cutscene playback decoded by the clean-room `xiii-video` decoder, with its Bink Audio
//! track.
//!
//! Two consumers share this module:
//!
//! * the standalone `--video FILE` mode ([`VideoPlugin`]), which plays one file and exits; and
//! * the in-game `Engine.VideoPlayer` host ([`VideoHostHandle`], item21): the VM's video natives
//!   open/play/stop a clip, [`VideoHostHandle`] decodes and plays it fullscreen (letterboxed,
//!   [`VideoOverlay`]) over the scene with its audio track, and the VM's `GetStatus` follows the
//!   actual host playback.
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
//! The caller-paced [`CutscenePlayer::advance_virtual`] replaces the clock entirely (headless
//! hosts and tests): time accumulates the passed `dt` and no audio is decoded.
//!
//! If the decoder cannot decode a frame the picture holds the last good frame, the error is
//! recorded and the clip is flagged errored (`Engine.VideoPlayer.GetStatus` reports the game's
//! error status `2`); the error is reported on exit, never silently.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::audio::{AddAudioSource, Decodable, Source};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};

use xiii_script::VideoPlayerHost;
use xiii_video::container::BikFile;
use xiii_video::{AudioDecoder, AudioTables, BinkTables, Decoder, YuvFrame};

use crate::cli::{Audio, Options};

/// Seconds of decoded audio kept queued ahead of the mixer.
pub const AUDIO_AHEAD_SECS: f64 = 1.0;

/// How long to wait for the output device to pull the first sample before falling back to the
/// wall clock.
pub const AUDIO_START_TIMEOUT: Duration = Duration::from_millis(1500);

/// Z-index of the fullscreen black backdrop behind a playing cutscene (above every HUD layer).
const OVERLAY_BACKDROP_Z: i32 = 90;

/// Z-index of the letterboxed cutscene picture (above the backdrop).
const OVERLAY_PICTURE_Z: i32 = 100;

/// Duration of a Bink 1 clip from its fixed header (36 bytes), or `None` for an invalid
/// signature/zero frame-rate field. Used only as the labelled `VideoPlayer` fallback when
/// `xiii-video` cannot decode the clip.
pub fn bink_duration_from_header(header: &[u8; 36]) -> Option<f32> {
    if &header[0..3] != b"BIK" {
        return None;
    }
    let frames = u32::from_le_bytes(header[8..12].try_into().ok()?);
    let fps_num = u32::from_le_bytes(header[28..32].try_into().ok()?);
    let fps_den = u32::from_le_bytes(header[32..36].try_into().ok()?);
    if frames == 0 || fps_num == 0 || fps_den == 0 {
        return None;
    }
    let secs = frames as f32 * fps_den as f32 / fps_num as f32;
    secs.is_finite().then_some(secs)
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

/// The playback clock of a [`CutscenePlayer`]. Idle until [`CutscenePlayer::play`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PlayerClock {
    /// Not playing (before `play`, after `stop`).
    Idle,
    /// Realtime playback: audio clock master with the wall clock fallback.
    Realtime,
    /// Caller-paced playback ([`CutscenePlayer::advance_virtual`]): headless hosts and tests.
    Virtual,
}

/// One decoded cutscene: the parsed Bink file, the clean-room decoder, the audio stream and the
/// playback clock. The picture (`image`) carries the newest decoded frame for upload.
pub struct CutscenePlayer {
    data: Vec<u8>,
    bik: BikFile,
    decoder: Decoder,
    audio: Option<AudioPlayback>,
    prev: Option<YuvFrame>,
    next_frame: usize,
    image: Image,
    clock: PlayerClock,
    /// Origin of the realtime clock (reset when the audio start timeout falls back to wall).
    realtime_origin: Instant,
    /// Set once the realtime clock fell back from the audio clock to the wall clock.
    fell_back: bool,
    /// Virtual time accumulated by `advance_virtual`.
    virtual_time: f64,
    /// Most recent playback-clock value used to decode frames. Completion also waits until this
    /// time reaches the clip duration, so the last frame remains visible for its full frame time.
    playback_time: f64,
    /// Whether the audio stream has been handed to the mixer (`take_audio_start`).
    audio_started: bool,
    decoded: usize,
    errors: usize,
    first_error: Option<String>,
}

/// Result of one [`CutscenePlayer::advance_realtime`]/[`CutscenePlayer::advance_virtual`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameInfo {
    /// Index of the newest frame decoded (and blitted into [`CutscenePlayer::image`]) this call.
    pub newest: Option<usize>,
    /// Playback time in seconds after this call.
    pub time: f64,
    /// Every frame of the clip has been decoded: the playback actually ended.
    pub finished: bool,
}

impl CutscenePlayer {
    /// Reads and parses `file`, prepares the decoder and (when `audio_on` and the install
    /// provides the audio tables) audio track 0. An error here is returned, never swallowed:
    /// the caller labels the fallback.
    pub fn open(
        file: &Path,
        tables: &BinkTables,
        audio_tables: Option<&AudioTables>,
        audio_on: bool,
    ) -> Result<CutscenePlayer, String> {
        let data = std::fs::read(file).map_err(|e| format!("reading {}: {e}", file.display()))?;
        let bik = BikFile::parse(&data).map_err(|e| format!("{}: {e}", file.display()))?;
        let width = bik.header.width as usize;
        let height = bik.header.height as usize;
        let image = Image::new(
            Extent3d {
                width: width as u32,
                height: height as u32,
                depth_or_array_layers: 1,
            },
            TextureDimension::D2,
            vec![0u8; width * height * 4],
            TextureFormat::Rgba8UnormSrgb,
            RenderAssetUsages::default(),
        );
        let audio = if audio_on {
            bik.audio.first().and_then(|track| match audio_tables {
                None => None,
                Some(tables) => match AudioDecoder::new(tables, track) {
                    Ok(decoder) => Some(AudioPlayback {
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
                    }),
                    Err(e) => {
                        eprintln!("[video] audio unavailable, using the wall clock: {e}");
                        None
                    }
                },
            })
        } else {
            None
        };
        Ok(CutscenePlayer {
            data,
            bik,
            decoder: Decoder::new(tables.clone()),
            audio,
            prev: None,
            next_frame: 0,
            image,
            clock: PlayerClock::Idle,
            realtime_origin: Instant::now(),
            fell_back: false,
            virtual_time: 0.0,
            playback_time: 0.0,
            audio_started: false,
            decoded: 0,
            errors: 0,
            first_error: None,
        })
    }

    /// Picture width in pixels.
    pub fn width(&self) -> u32 {
        self.bik.header.width
    }

    /// Picture height in pixels.
    pub fn height(&self) -> u32 {
        self.bik.header.height
    }

    /// Total frame count of the clip.
    pub fn frame_count(&self) -> usize {
        self.bik.frame_count()
    }

    /// Declared frame rate.
    pub fn fps(&self) -> f64 {
        self.bik.header.fps()
    }

    /// Declared duration in seconds (frames / fps).
    pub fn duration_secs(&self) -> f64 {
        let fps = self.fps();
        if fps <= 0.0 {
            0.0
        } else {
            self.bik.frame_count() as f64 / fps
        }
    }

    /// Audio track facts for the setup report: `(track count, sample rate, channels, block
    /// samples per channel)`.
    pub fn audio_track_info(&self) -> Option<(usize, u32, u16, usize)> {
        let a = self.audio.as_ref()?;
        Some((
            self.bik.audio.len(),
            a.decoder.sample_rate(),
            a.decoder.channels(),
            a.decoder.frame_len(),
        ))
    }

    /// Whether playback is running (between `play` and `stop`).
    pub fn is_playing(&self) -> bool {
        self.clock != PlayerClock::Idle
    }

    /// Frames decoded so far.
    pub fn decoded(&self) -> usize {
        self.decoded
    }

    /// Video decode errors seen so far (the clip is errored when this is > 0).
    pub fn errors(&self) -> usize {
        self.errors
    }

    /// First video decode error, if any.
    pub fn first_error(&self) -> Option<&str> {
        self.first_error.as_deref()
    }

    /// Index of the newest decoded frame (`usize::MAX` before the first one).
    pub fn shown_frame(&self) -> usize {
        self.next_frame.saturating_sub(1)
    }

    /// The current picture (black until the first frame is decoded).
    pub fn image(&self) -> &Image {
        &self.image
    }

    /// Audio mixer statistics: `(pulled secs, queued secs, underrun samples, packets, errors)`.
    pub fn audio_stats(&self) -> Option<(f64, f64, u64, usize, usize)> {
        let a = self.audio.as_ref()?;
        Some((
            a.pulled_secs(),
            a.queued as f64 / a.samples_per_sec(),
            a.shared.underrun.load(Ordering::Relaxed),
            a.packets,
            a.errors,
        ))
    }

    /// First audio decode error, if any.
    pub fn audio_first_error(&self) -> Option<&str> {
        self.audio.as_ref().and_then(|a| a.first_error.as_deref())
    }

    /// Audio pull rate in real time since the first pull (`pulled secs / elapsed`).
    pub fn audio_pull_rate(&self) -> Option<f64> {
        let a = self.audio.as_ref()?;
        let started = a.started?;
        Some(a.pulled_secs() / started.elapsed().as_secs_f64().max(1e-9))
    }

    /// Whether the realtime clock still follows the audio device (`false`: wall clock).
    pub fn clock_is_audio(&self) -> bool {
        self.audio.is_some() && !self.fell_back
    }

    /// Hands the audio stream to the mixer once realtime playback begins: `(sample rate,
    /// channels, shared FIFO)`. `None` while idle/already taken/audio off or absent.
    pub fn take_audio_start(&mut self) -> Option<(u32, u16, Arc<PcmShared>)> {
        if self.audio_started || self.clock != PlayerClock::Realtime {
            return None;
        }
        let a = self.audio.as_ref()?;
        self.audio_started = true;
        Some((
            a.decoder.sample_rate(),
            a.decoder.channels(),
            Arc::clone(&a.shared),
        ))
    }

    /// Whether every frame has been decoded while playing.
    pub fn is_finished(&self) -> bool {
        self.clock != PlayerClock::Idle
            && self.next_frame >= self.bik.frame_count()
            && self.playback_time >= self.duration_secs()
    }

    /// Whether playback failed (any video decode error; `GetStatus` reports the game's status 2).
    pub fn is_errored(&self) -> bool {
        self.errors > 0
    }

    /// Starts playback (idempotent). Realtime callers use the audio/wall clock; virtual callers
    /// ([`CutscenePlayer::advance_virtual`]) switch the clock on their first call.
    pub fn play(&mut self) {
        if self.clock == PlayerClock::Idle {
            self.clock = PlayerClock::Realtime;
            self.realtime_origin = Instant::now();
        }
    }

    /// Stops playback. A following `play` restarts the clip from frame 0.
    #[allow(dead_code)] // the host releases clips by dropping them; kept for symmetry with play
    pub fn stop(&mut self) {
        self.clock = PlayerClock::Idle;
        self.virtual_time = 0.0;
        self.playback_time = 0.0;
        self.next_frame = 0;
        self.prev = None;
    }

    /// Current playback time in seconds under the realtime rule (see the module docs). This is
    /// the same computation the standalone `--video` mode has always used; it also performs the
    /// audio-start-timeout fallback (reported through [`CutscenePlayer::took_clock_fallback`]).
    pub fn playback_time(&mut self) -> f64 {
        if self.clock != PlayerClock::Realtime {
            return match self.clock {
                PlayerClock::Virtual => self.virtual_time,
                PlayerClock::Idle | PlayerClock::Realtime => 0.0,
            };
        }
        // After the audio start timeout (or with no audio at all) the wall clock rules.
        if self.fell_back || self.audio.is_none() {
            return self.realtime_origin.elapsed().as_secs_f64();
        }
        let a = self.audio.as_mut().expect("audio checked above");
        let pulled = a.shared.pulled.load(Ordering::Relaxed);
        if pulled == 0 {
            if self.realtime_origin.elapsed() >= AUDIO_START_TIMEOUT {
                self.fell_back = true;
                self.realtime_origin = Instant::now();
                return 0.0;
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

    /// Returns and clears the realtime-clock fallback flag (audio start timeout).
    pub fn took_clock_fallback(&mut self) -> bool {
        std::mem::replace(&mut self.fell_back, false)
    }

    /// A read-only estimate of the playback time (no fallback transitions).
    pub fn time_estimate(&self) -> f64 {
        match self.clock {
            PlayerClock::Idle => 0.0,
            PlayerClock::Virtual => self.virtual_time,
            PlayerClock::Realtime => match self.audio.as_ref() {
                Some(a) if a.shared.pulled.load(Ordering::Relaxed) > 0 => a.pulled_secs(),
                _ => self.realtime_origin.elapsed().as_secs_f64(),
            },
        }
    }

    /// Decodes every frame due at playback time `now`, blits the newest one into the picture
    /// and feeds the audio (when present). Inter frames depend on their predecessor, so every
    /// due frame is decoded even when only the newest is shown.
    fn decode_due(&mut self, now: f64) -> FrameInfo {
        self.playback_time = now;
        if self.clock == PlayerClock::Realtime
            && let Some(a) = self.audio.as_mut()
        {
            a.feed(&self.data, &self.bik);
        }
        let fps = self.bik.header.fps().max(1.0);
        let due = (((now * fps) as usize) + 1).min(self.bik.frame_count());
        let mut newest = None;
        while self.next_frame < due {
            let i = self.next_frame;
            self.next_frame += 1;
            let video = match xiii_video::frame_video(&self.data, &self.bik, i) {
                Ok(v) => v,
                Err(e) => {
                    self.errors += 1;
                    self.first_error.get_or_insert_with(|| e.to_string());
                    continue;
                }
            };
            match self
                .decoder
                .decode_frame(video, &self.bik.header, self.prev.as_ref())
            {
                Ok((frame, _stats)) => {
                    self.prev = Some(frame);
                    self.decoded += 1;
                    newest = Some(i);
                }
                Err(e) => {
                    self.errors += 1;
                    self.first_error
                        .get_or_insert_with(|| format!("frame {i}: {e}"));
                }
            }
        }
        if newest.is_some()
            && let Some(frame) = self.prev.as_ref()
        {
            self.image.data = Some(frame.to_rgba());
        }
        FrameInfo {
            newest,
            time: now,
            finished: self.is_finished(),
        }
    }

    /// Advances realtime playback (audio clock master) and decodes the due frames.
    pub fn advance_realtime(&mut self) -> FrameInfo {
        if self.clock != PlayerClock::Realtime {
            return FrameInfo {
                newest: None,
                time: self.time_estimate(),
                finished: self.is_finished(),
            };
        }
        let now = self.playback_time();
        self.decode_due(now)
    }

    /// Advances caller-paced playback by `dt` seconds (headless hosts and tests): no audio is
    /// decoded and completion is the decoded frame count.
    pub fn advance_virtual(&mut self, dt: f64) -> FrameInfo {
        if self.clock == PlayerClock::Idle {
            return FrameInfo {
                newest: None,
                time: self.virtual_time,
                finished: false,
            };
        }
        self.clock = PlayerClock::Virtual;
        self.virtual_time += dt.max(0.0);
        self.decode_due(self.virtual_time)
    }
}

/// Bound count of diagnostics kept per host.
const HOST_LOG_LIMIT: usize = 16;

/// Shared state of one [`VideoHostHandle`].
struct HostInner {
    game_dir: PathBuf,
    audio_on: bool,
    /// Decoder tables read once from the installation's `binkw32.dll`; `None` (with the reason
    /// in `tables_error`) when they could not be read: every open then falls back to the
    /// VM's duration timer.
    tables: Option<Arc<BinkTables>>,
    audio_tables: Option<Arc<AudioTables>>,
    tables_error: Option<String>,
    player: Option<CutscenePlayer>,
    /// Incremented whenever `Open` replaces a clip or `Stop` releases one, so the overlay can
    /// retire old image/audio entities even when stop+open happen between render updates.
    generation: u64,
    last_error: Option<String>,
    log: VecDeque<String>,
    opens: u64,
    host_opens: u64,
    plays: u64,
    stops: u64,
}

impl HostInner {
    fn note(&mut self, msg: String) {
        if self.log.len() >= HOST_LOG_LIMIT {
            self.log.pop_front();
        }
        self.log.push_back(msg);
    }
}

/// item21: the host side of the VM's `Engine.VideoPlayer` ([`xiii_script::VideoPlayerHost`]).
///
/// The VM natives run on the main thread (fixed step / menu tick); the Bevy sync systems run on
/// the same thread and drive the player through a clone of this handle. The clip is decoded
/// with the clean-room `xiii-video` decoder, shown fullscreen (letterboxed, [`VideoOverlay`])
/// over the scene and streamed through the Bink Audio track. `GetStatus` reports completion
/// from the actual playback ([`CutscenePlayer::is_finished`]); decode failures report the
/// game's error status. `open` returning `None` (unreadable file or decoder tables) falls back
/// to the VM's duration timer, which the native's trace labels.
#[derive(Clone)]
pub struct VideoHostHandle {
    inner: Rc<RefCell<HostInner>>,
}

impl VideoHostHandle {
    /// Creates a host for `game_dir`. Decoder tables are read lazily on the first open.
    pub fn new(game_dir: PathBuf, audio_on: bool) -> Self {
        Self {
            inner: Rc::new(RefCell::new(HostInner {
                game_dir,
                audio_on,
                tables: None,
                audio_tables: None,
                tables_error: None,
                player: None,
                generation: 0,
                last_error: None,
                log: VecDeque::new(),
                opens: 0,
                host_opens: 0,
                plays: 0,
                stops: 0,
            })),
        }
    }

    /// Enables/disables the Bink audio track for clips opened afterwards (the `--audio` flag).
    #[allow(dead_code)] // symmetric API for hosts constructed before options are parsed
    pub fn set_audio(&self, on: bool) {
        self.inner.borrow_mut().audio_on = on;
    }

    /// The resolved clip path for `stem` (case-insensitive stem match in `<game_dir>/Video`).
    fn find_clip(game_dir: &Path, stem: &str) -> Option<PathBuf> {
        let dir = game_dir.join("Video");
        let entries = std::fs::read_dir(&dir).ok()?;
        entries.flatten().map(|e| e.path()).find(|p| {
            p.extension().is_some_and(|x| x.eq_ignore_ascii_case("bik"))
                && p.file_stem().is_some_and(|s| s.eq_ignore_ascii_case(stem))
        })
    }

    /// Opens (and prepares) the clip named `stem`; returns its decoded duration, or `None` with
    /// the reason recorded (the VM then falls back to its duration timer, labelled).
    pub fn open_clip(&self, stem: &str) -> Option<f32> {
        let mut inner = self.inner.borrow_mut();
        inner.opens += 1;
        if inner.player.take().is_some() {
            inner.stops += 1;
            inner.generation = inner.generation.wrapping_add(1);
        }
        let Some(path) = Self::find_clip(&inner.game_dir, stem) else {
            inner.last_error = Some(format!(
                "no Video/{stem}.bik under {}",
                inner.game_dir.display()
            ));
            inner.note(format!("open {stem}: no clip file"));
            return None;
        };
        let tables = match inner.tables.clone() {
            Some(t) => Some(t),
            None => match BinkTables::from_install(&inner.game_dir) {
                Ok(t) => {
                    inner.tables = Some(Arc::new(t));
                    inner.tables.clone()
                }
                Err(e) => {
                    inner.tables_error = Some(e.to_string());
                    None
                }
            },
        };
        let Some(tables) = tables else {
            inner.last_error = inner.tables_error.clone();
            inner.note(format!("open {stem}: decoder tables unavailable"));
            return None;
        };
        // Audio tables are optional: without them the clip plays on the wall clock.
        if inner.audio_on && inner.audio_tables.is_none() {
            match AudioTables::from_install(&inner.game_dir) {
                Ok(t) => inner.audio_tables = Some(Arc::new(t)),
                Err(e) if inner.audio_on => {
                    eprintln!(
                        "[video] Bink Audio tables unavailable; playing without cutscene audio: {e}"
                    );
                }
                Err(_) => {}
            }
        }
        let audio_on = inner.audio_on;
        let audio_tables = inner.audio_tables.clone();
        match CutscenePlayer::open(&path, &tables, audio_tables.as_deref(), audio_on) {
            Ok(player) => {
                let dur = player.duration_secs() as f32;
                inner.host_opens += 1;
                inner.note(format!(
                    "open {stem}: {}x{} {} frames ({:.3}s) from {}",
                    player.width(),
                    player.height(),
                    player.frame_count(),
                    player.duration_secs(),
                    path.display()
                ));
                inner.player = Some(player);
                inner.generation = inner.generation.wrapping_add(1);
                Some(dur)
            }
            Err(e) => {
                inner.last_error = Some(e.clone());
                inner.note(format!("open {stem}: {e}"));
                None
            }
        }
    }

    /// Starts playback of the opened clip.
    pub fn play_clip(&self) {
        let mut inner = self.inner.borrow_mut();
        if let Some(p) = inner.player.as_mut() {
            p.play();
            inner.plays += 1;
        }
    }

    /// Stops playback and releases the clip (the overlay sync tears the picture down).
    pub fn stop_clip(&self) {
        let mut inner = self.inner.borrow_mut();
        if inner.player.take().is_some() {
            inner.stops += 1;
            inner.generation = inner.generation.wrapping_add(1);
        }
    }

    /// Whether a clip is open and its playback running.
    pub fn is_playing(&self) -> bool {
        self.inner
            .borrow()
            .player
            .as_ref()
            .is_some_and(CutscenePlayer::is_playing)
    }

    /// Estimate of the running clip's playback time in seconds (0 when none is playing).
    pub fn time_estimate(&self) -> f64 {
        self.inner
            .borrow()
            .player
            .as_ref()
            .map_or(0.0, CutscenePlayer::time_estimate)
    }

    /// Frame count of the open clip, if any (tests).
    #[allow(dead_code)] // used by the opt-in corpus test
    pub fn frame_count(&self) -> Option<usize> {
        self.inner
            .borrow()
            .player
            .as_ref()
            .map(CutscenePlayer::frame_count)
    }

    /// Declared fps of the open clip, if any (tests).
    #[allow(dead_code)] // used by the opt-in corpus test
    pub fn fps(&self) -> Option<f64> {
        self.inner.borrow().player.as_ref().map(CutscenePlayer::fps)
    }

    /// Frames decoded of the open clip, if any (tests).
    #[allow(dead_code)] // used by the opt-in corpus test
    pub fn decoded(&self) -> Option<usize> {
        self.inner
            .borrow()
            .player
            .as_ref()
            .map(CutscenePlayer::decoded)
    }

    /// Advances caller-paced playback of the open clip (headless hosts and tests).
    pub fn advance_virtual_all(&self, dt: f64) {
        if let Some(p) = self.inner.borrow_mut().player.as_mut() {
            p.advance_virtual(dt);
        }
    }

    /// One exit-report line: what was opened, played and stopped, and the first failure.
    pub fn report_line(&self) -> String {
        let inner = self.inner.borrow();
        format!(
            "[video host] opens {} (host {}, fallback/failed {}), plays {}, stops {}; first error: {}",
            inner.opens,
            inner.host_opens,
            inner.opens - inner.host_opens,
            inner.plays,
            inner.stops,
            inner.last_error.as_deref().unwrap_or("none"),
        )
    }

    /// The bounded diagnostics log (open/play/stop events with reasons).
    pub fn diagnostics(&self) -> Vec<String> {
        self.inner.borrow().log.iter().cloned().collect()
    }

    /// Drives the fullscreen overlay for one frame: tears it down when no clip is open, else
    /// (re)creates the black backdrop and the letterboxed picture node, hands the audio stream
    /// to the mixer, advances realtime playback and uploads the newest frame.
    pub fn sync_overlay(
        &self,
        overlay: &mut VideoOverlay,
        images: &mut Assets<Image>,
        audio_assets: &mut Assets<BinkAudio>,
        commands: &mut Commands,
        window: [f32; 2],
    ) {
        let mut inner = self.inner.borrow_mut();
        let generation = inner.generation;
        let Some(player) = inner.player.as_mut() else {
            overlay.teardown(commands, images);
            return;
        };
        if overlay.generation.is_some_and(|old| old != generation) {
            overlay.teardown(commands, images);
        }
        // First frame with a clip open: put the (black) picture into the assets and build the
        // overlay entities.
        if overlay.image.is_none() {
            let handle = images.add(player.image().clone());
            let backdrop = commands
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        left: px(0.0),
                        top: px(0.0),
                        width: Val::Percent(100.0),
                        height: Val::Percent(100.0),
                        ..default()
                    },
                    BackgroundColor(Color::BLACK),
                    GlobalZIndex(OVERLAY_BACKDROP_Z),
                ))
                .id();
            let node = commands
                .spawn((
                    Node {
                        position_type: PositionType::Absolute,
                        ..default()
                    },
                    ImageNode {
                        image: handle.clone(),
                        ..default()
                    },
                    GlobalZIndex(OVERLAY_PICTURE_Z),
                ))
                .id();
            overlay.image = Some(handle);
            overlay.backdrop = Some(backdrop);
            overlay.node = Some(node);
            overlay.generation = Some(generation);
        }
        // Start the audio stream once realtime playback begins.
        if overlay.audio.is_none()
            && let Some((rate, channels, shared)) = player.take_audio_start()
        {
            let asset = audio_assets.add(BinkAudio {
                shared,
                sample_rate: rate,
                channels,
            });
            let entity = commands
                .spawn((
                    Name::new("bink cutscene audio"),
                    AudioPlayer(asset),
                    PlaybackSettings::ONCE,
                ))
                .id();
            overlay.audio = Some(entity);
        }
        // Advance playback and upload the newest frame.
        let info = player.advance_realtime();
        if info.newest.is_some()
            && let Some(handle) = overlay.image.clone()
            && let (Some(mut img), Some(rgba)) =
                (images.get_mut(&handle), player.image().data.clone())
        {
            img.data = Some(rgba);
        }
        // Letterbox: keep the picture aspect-centred in the window.
        if let Some(node) = overlay.node {
            let vw = player.width() as f32;
            let vh = player.height() as f32;
            let scale = (window[0] / vw).min(window[1] / vh);
            let dw = vw * scale;
            let dh = vh * scale;
            commands.entity(node).insert(Node {
                position_type: PositionType::Absolute,
                left: px((window[0] - dw) * 0.5),
                top: px((window[1] - dh) * 0.5),
                width: px(dw),
                height: px(dh),
                ..default()
            });
        }
    }
}

impl VideoPlayerHost for VideoHostHandle {
    fn open(&mut self, name: &str) -> Option<f32> {
        self.open_clip(name)
    }

    fn play(&mut self) {
        self.play_clip();
    }

    fn stop(&mut self) {
        self.stop_clip();
    }

    fn finished(&self) -> bool {
        self.inner
            .borrow()
            .player
            .as_ref()
            .is_some_and(CutscenePlayer::is_finished)
    }

    fn errored(&self) -> bool {
        self.inner
            .borrow()
            .player
            .as_ref()
            .is_some_and(CutscenePlayer::is_errored)
    }
}

/// Display entities and assets of the in-game cutscene overlay (plain resource; the host
/// handle itself is non-send and owned by the session side).
#[derive(Resource, Default)]
pub struct VideoOverlay {
    backdrop: Option<Entity>,
    node: Option<Entity>,
    audio: Option<Entity>,
    image: Option<Handle<Image>>,
    generation: Option<u64>,
}

impl VideoOverlay {
    /// Despawns the overlay entities and releases the picture asset.
    fn teardown(&mut self, commands: &mut Commands, images: &mut Assets<Image>) {
        if let Some(e) = self.backdrop.take() {
            commands.entity(e).try_despawn();
        }
        if let Some(e) = self.node.take() {
            commands.entity(e).try_despawn();
        }
        if let Some(e) = self.audio.take() {
            commands.entity(e).try_despawn();
        }
        if let Some(h) = self.image.take() {
            images.remove(h.id());
        }
        self.generation = None;
    }
}

/// Non-send resource holding the host side of the VM's `Engine.VideoPlayer`. `None` until a
/// session is open (a script-session failure leaves it empty and the sync systems idle).
/// Inserted with `insert_non_send` (the `Rc` inside is main-thread only).
#[derive(Default)]
pub struct CutsceneHost(pub Option<VideoHostHandle>);

/// Registers the cutscene overlay resources and the Bink audio source. Each mode adds its own
/// sync system (it reads that mode's session resource) — see `play::cutscene`/`menu::cutscene`.
pub struct CutscenePlugin;

impl Plugin for CutscenePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<VideoOverlay>()
            .init_non_send::<CutsceneHost>()
            .add_audio_source::<BinkAudio>();
    }
}

// -------------------------------------------------------------------------------------------
// Standalone `--video FILE` mode
//

/// Resource holding the parsed options.
#[derive(Resource, Clone)]
pub struct VideoConfig {
    /// Runtime options (file, game dir, exit/screenshot).
    pub options: Options,
}

/// Runtime playback state of the standalone mode (bookkeeping around the shared player).
#[derive(Resource)]
pub struct VideoState {
    player: CutscenePlayer,
    handle: Handle<Image>,
    start: Instant,
    shot: u8,
    shot_done: bool,
    target_at: Option<Instant>,
    /// Largest `clock - frame time` at an upload (how late the shown frame was), seconds.
    max_late: f64,
    /// Uploaded frames.
    uploads: usize,
    /// Periodic sync log: next playback second to print.
    next_log: f64,
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

    let audio_on = cfg.options.audio != Audio::Off;
    let audio_tables = if audio_on {
        match AudioTables::from_install(&game_dir) {
            Ok(tables) => Some(tables),
            Err(e) => {
                eprintln!("[video] Bink Audio tables unavailable; using the wall clock: {e}");
                None
            }
        }
    } else {
        None
    };
    let mut player = match CutscenePlayer::open(&file, &tables, audio_tables.as_ref(), audio_on) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[video] {e}");
            commands.write_message(AppExit::error());
            return;
        }
    };
    println!(
        "[video] {} {}x{} {} frames {:.3} fps (decoder clean-room Bink 1)",
        file.display(),
        width,
        height,
        bik.frame_count(),
        bik.header.fps()
    );
    match player.audio_track_info() {
        Some((tracks, rate, channels, block)) => println!(
            "[video] audio track 0 of {tracks}: {rate} Hz x {channels} ch, DCT, block {block} samples/channel (clean-room Bink Audio)"
        ),
        None if audio_on => println!("[video] no audio track: wall clock"),
        None => println!("[video] audio off (--audio off): wall clock"),
    }
    // Playback starts with the scene (the clock the frames are due on); decode the first second
    // of audio now so the mixer has data when it starts pulling, then hand the stream to the
    // mixer as its own entity.
    player.play();
    player.advance_realtime();
    if let Some((rate, channels, shared)) = player.take_audio_start() {
        let asset = bink_audio.add(BinkAudio {
            shared,
            sample_rate: rate,
            channels,
        });
        commands.spawn((
            Name::new("bink audio track 0"),
            AudioPlayer(asset),
            PlaybackSettings::ONCE,
        ));
    }

    commands.insert_resource(VideoState {
        player,
        handle,
        start: Instant::now(),
        shot: 0,
        shot_done: false,
        target_at: None,
        max_late: 0.0,
        uploads: 0,
        next_log: 1.0,
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
    if state.player.took_clock_fallback() {
        println!(
            "[video] the audio device pulled no samples within {:?}; switching to the wall clock",
            AUDIO_START_TIMEOUT
        );
    }
    let info = state.player.advance_realtime();
    if let Some(i) = info.newest {
        if let (Some(mut img), Some(rgba)) = (
            images.get_mut(&state.handle),
            state.player.image().data.clone(),
        ) {
            img.data = Some(rgba);
        }
        state.uploads += 1;
        // Lateness of the shown frame against the clock *after* decoding it.
        let after = state.player.playback_time();
        let fps = state.player.fps().max(1.0);
        state.max_late = state.max_late.max(after - i as f64 / fps);
    }
    if cfg.options.exit_after_secs.is_some() {
        let now = state.player.playback_time();
        if now >= state.next_log {
            state.next_log = now.floor() + 1.0;
            let wall = state.start.elapsed().as_secs_f64();
            let shown = state.player.shown_frame();
            match state.player.audio_stats() {
                Some((pulled, queued, underrun, _, _)) => println!(
                    "[video] sync t={now:.3}s clock={:?} frame={shown} (frame time {:.3}s) audio pulled {pulled:.3}s queued {queued:.3}s underrun {underrun} wall {wall:.3}s",
                    if state.player.clock_is_audio() {
                        "Audio"
                    } else {
                        "Wall"
                    },
                    shown as f64 / state.player.fps().max(1.0),
                ),
                None => println!(
                    "[video] sync t={now:.3}s clock={:?} frame={shown} wall {wall:.3}s",
                    if state.player.clock_is_audio() {
                        "Audio"
                    } else {
                        "Wall"
                    },
                ),
            }
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
    let state = &mut *state;
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
    let decoded = state.player.decoded();
    let errors = state.player.errors();
    let first_error = state.player.first_error().map(str::to_owned);
    println!(
        "[video] exit after {elapsed:.1}s: {decoded} frames decoded, {errors} errors, {} total frames{}",
        state.player.frame_count(),
        match &first_error {
            Some(e) => format!("; first error: {e}"),
            None => String::new(),
        }
    );
    let fps = state.player.fps().max(1.0);
    let shown = state.player.shown_frame();
    match state.player.audio_stats() {
        Some((pulled, queued, underrun, packets, audio_errors)) => {
            let rate = state.player.audio_pull_rate().unwrap_or(0.0);
            println!(
                "[video] audio exit: clock={:?} packets {packets} errors {audio_errors} queued {queued:.3}s pulled {pulled:.3}s \
                 (pull rate {rate:.3}x real time since first pull) underrun samples {underrun}; last shown frame {shown} \
                 (time {:.3}s); max frame lateness at upload {:.1} ms over {} uploads{}",
                if state.player.clock_is_audio() {
                    "Audio"
                } else {
                    "Wall"
                },
                shown as f64 / fps,
                state.max_late * 1000.0,
                state.uploads,
                match state.player.audio_first_error() {
                    Some(e) => format!("; first audio error: {e}"),
                    None => String::new(),
                }
            );
        }
        None => println!(
            "[video] audio exit: clock={:?} (no audio); max frame lateness at upload {:.1} ms over {} uploads",
            if state.player.clock_is_audio() {
                "Audio"
            } else {
                "Wall"
            },
            state.max_late * 1000.0,
            state.uploads
        ),
    }
    exit.write(AppExit::Success);
}
