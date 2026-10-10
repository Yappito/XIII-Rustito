//! VM/level sound and music playback for `--play` (Bevy audio).
//!
//! The script VM is headless and emits [`PresentationEvent`]s; this module is the presentation
//! side. It turns
//!
//! * `PlaySound`/`PlayRolloffSound`/`PlayMusic` events into Bevy audio, resolved through
//!   [`xiii_audio::SoundLibrary`] (`Sound` -> HX bank entry -> PCM),
//! * the **level's own audio** discovered by `xiii_world::audio` into ambient loops
//!   (`AmbientSound` on actors) and the level music cue (`LevelInfo.InitMusic`, a
//!   `XIIISaveGameTrigger.SoundToLaunch`, or the `Music__<Title>` convention),
//! * distance attenuation matching XIII's `CRolloffParam` (see [`xiii_audio::Attenuation`]);
//!   Bevy's rodio spatial path has no configurable roll-off, so the host applies the gain.
//!
//! ## Streaming
//!
//! Music and long ambient loops are decoded **in chunks on the audio thread** through
//! [`xiii_audio::WaveStream`] wrapped in a custom [`MusicSample`]/[`StreamSample`] rodio
//! [`Source`], not by decoding a whole streamed entry into memory on the main thread. The
//! chunked decoder is bit-exact with a whole decode (tested in `xiii-audio`).
//!
//! ## Semantics (evidence)
//!
//! * `PlayMusic` replaces the current music track; `StopMusic` stops it (XIII's own `Mover` and
//!   `TriggerSound` call `PlayMusic` on state changes and `StopSound` on the mover ambient).
//!   The exact fade curve is **not** decoded; stop/replace is immediate and labelled.
//! * Distance `radius` (`Param3`, **hypothesis** slot/volume/radius/pitch reading) selects a
//!   saturation distance with the class-default stabilisation ratio; actor `SaturationDistance`/
//!   `StabilisationDistance`/`StabilisationVolume` class defaults drive ambient emitters.
//!
//! Absence of an audio device is logged and never fatal: sinks are never attached, such entities
//! are expired and counted so an unattended run continues.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bevy::audio::{AddAudioSource, AudioSinkPlayback, SpatialScale, Volume};
use bevy::prelude::*;

use xiii_audio::{Attenuation, PcmAudio, ResolveFailure, SoundLibrary, WaveStream};
use xiii_script::PresentationEvent;
use xiii_world::audio::{LevelAudio, MusicCue};

use crate::cli::Options;

/// Maximum number of unresolved/queued requests kept for the overlay.
const OVERLAY_HISTORY: usize = 8;
/// Audio entities that never receive a sink (no audio device) are dropped after this long.
const PENDING_TIMEOUT: Duration = Duration::from_secs(5);
/// Ear gap of the listener (metres). Hypothesis: a human head is ~0.2 m wide.
const EAR_GAP_M: f32 = 0.2;
/// Spatial scale applied to every spatial emitter. Bevy/rodio's spatial source applies an
/// uncapped `1/d^2` attenuation (XIII is in Unreal units and metres, where that would silence a
/// 200 m emitter); the UE2 `CRolloffParam` gain (see [`xiii_audio::Attenuation`]) is the intended
/// distance model. A very small scale pushes rodio's `min(1.0, 1/d^2)` into the clamp so rodio
/// contributes **no** distance attenuation, while its left/right panning (which depends only on
/// the relative ear/emitter geometry) still works. The scale is small enough that the whole map's
/// emitter/listener distances stay inside the clamp (`1e-5 * 2222 m = 0.022` -> `1/d^2 >> 1`).
const NO_RODIO_ROLLOFF: SpatialScale = SpatialScale::new(1.0 / 100_000.0);
/// Frames per streamed decode chunk (about 0.25 s at 22050 Hz); bounded work per audio callback.
const STREAM_CHUNK_FRAMES: usize = 8192;
/// Re-check interval for ambient distance gain (metres of player movement is cheap; time is fine).
const ATTENUATION_PERIOD: Duration = Duration::from_millis(100);

static MENU_MASTER_DB: std::sync::atomic::AtomicU32 =
    std::sync::atomic::AtomicU32::new(0.0f32.to_bits());
static MENU_MUSIC_ENABLED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
static MENU_STOP_MUSIC: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Updates the menu mixer preview from the retail dB value.
pub fn set_menu_volume_db(db: f32) {
    MENU_MASTER_DB.store(db.to_bits(), Ordering::Relaxed);
}

/// Updates whether the retail menu music selector is enabled (zero is off).
pub fn set_menu_music_enabled(enabled: bool) {
    MENU_MUSIC_ENABLED.store(u64::from(enabled), Ordering::Relaxed);
}

/// Requests that current menu music stop on the audio system's next update.
pub fn stop_menu_music() {
    MENU_STOP_MUSIC.store(1, Ordering::Relaxed);
}

/// Which native emitted a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundKind {
    /// `Actor.PlaySound`.
    Sound,
    /// `Actor.PlayMusic`.
    Music,
    /// `Actor.PlayRolloffSound`.
    Rolloff,
    /// Host-synthesised footstep from the floor's `XIIIFootStepSound` (the player path of
    /// `XIIIPlayerPawn.PlayFootStep`; the pawn has no third-person animation to fire the notify).
    Footstep,
}

/// One queued playback request, copied out of a [`PresentationEvent`] by [`pump`].
#[derive(Debug, Clone, PartialEq)]
pub struct SoundRequest {
    /// Emitting native.
    pub kind: SoundKind,
    /// VM time in seconds.
    pub time: f64,
    /// Object the native ran on.
    pub actor: String,
    /// Decoded `Sound` object path, `None` when the VM could not resolve the reference.
    pub sound: Option<String>,
    /// Decoded `RollOffActor` (rolloff sounds only).
    pub rolloff_actor: Option<String>,
    /// Decoded `Param1` (semantic `slot` — hypothesis).
    pub slot: Option<i32>,
    /// Decoded `Param2` (semantic `volume` — hypothesis).
    pub volume: Option<i32>,
    /// Decoded `Param3` (semantic `radius` — hypothesis).
    pub radius: Option<i32>,
    /// Decoded `Param4` (semantic `pitch` — hypothesis).
    pub pitch: Option<i32>,
    /// Decoded `Param5` (no semantic name).
    pub param5: Option<i32>,
}

impl SoundRequest {
    fn from_event(ev: &PresentationEvent, kind: SoundKind) -> Self {
        let (e, rolloff) = match ev {
            PresentationEvent::PlayRolloffSound(e) => (e, Some(())),
            PresentationEvent::PlaySound(e) | PresentationEvent::PlayMusic(e) => (e, None),
            other => unreachable!("pump only forwards sound events, got {other}"),
        };
        Self {
            kind,
            time: e.time,
            actor: e.actor.clone(),
            sound: e.sound.clone(),
            rolloff_actor: rolloff.and(e.rolloff_actor.clone()),
            slot: e.slot,
            volume: e.volume,
            radius: e.radius,
            pitch: e.pitch,
            param5: e.param5,
        }
    }

    /// A host-synthesised footstep at VM time `time`, playing `sound` (a `XIIIFootStepSound`
    /// path) on `actor`. Surface is already resolved by the caller.
    pub fn footstep(time: f64, actor: String, sound: String) -> Self {
        Self {
            kind: SoundKind::Footstep,
            time,
            actor,
            sound: Some(sound),
            rolloff_actor: None,
            slot: None,
            volume: None,
            radius: None,
            pitch: None,
            param5: None,
        }
    }
}

/// Process-global queue and event-cursor. The VM session is `!Send` and lives on the main
/// thread, but the cursor is a plain `Mutex` so it is correct regardless of the executor thread.
struct Pending {
    /// Last VM time whose events were forwarded.
    last_time: f64,
    /// Number of events at `last_time` already forwarded.
    at_last_time: usize,
    /// Forwarded requests not yet consumed by the plugin.
    queue: Vec<SoundRequest>,
}

static PENDING: Mutex<Pending> = Mutex::new(Pending {
    last_time: f64::NEG_INFINITY,
    at_last_time: 0,
    queue: Vec::new(),
});

/// Total requests ever forwarded (for diagnostics/tests).
static FORWARDED: AtomicU64 = AtomicU64::new(0);

/// The one hook called by the play loop each fixed step: copies newly drained sound/music events
/// from `sess.events` into the playback queue. Events are appended with non-decreasing VM time,
/// so the cursor is `(last_time, count_at_last_time)`.
pub fn pump<'a>(events: impl Iterator<Item = &'a (f64, PresentationEvent)>) {
    let mut p = match PENDING.lock() {
        Ok(p) => p,
        Err(poisoned) => poisoned.into_inner(),
    };
    let prev_time = p.last_time;
    let items: Vec<&(f64, PresentationEvent)> = events.collect();
    // The retained deque drops oldest entries first, so when events we already forwarded at
    // `prev_time` have been popped, clamp the skip count to what is actually still retained.
    let sound_kind = |ev: &PresentationEvent| match ev {
        PresentationEvent::PlaySound(_) => Some(SoundKind::Sound),
        PresentationEvent::PlayMusic(_) => Some(SoundKind::Music),
        PresentationEvent::PlayRolloffSound(_) => Some(SoundKind::Rolloff),
        _ => None,
    };
    // Footstep requests are host-synthesised (queued separately by the footstep driver), so they
    // are not part of the VM cursor bookkeeping.
    let retained_at_prev = items
        .iter()
        .filter(|(t, e)| *t == prev_time && sound_kind(e).is_some())
        .count();
    let already = p.at_last_time.min(retained_at_prev);

    let mut newest = prev_time;
    let mut seen_at_prev = 0usize;
    for (t, ev) in &items {
        let Some(kind) = sound_kind(ev) else {
            continue;
        };
        if *t < prev_time {
            continue;
        }
        if *t == prev_time {
            seen_at_prev += 1;
            if seen_at_prev <= already {
                continue;
            }
        }
        p.queue.push(SoundRequest::from_event(ev, kind));
        FORWARDED.fetch_add(1, Ordering::Relaxed);
        if *t > newest {
            newest = *t;
        }
    }
    p.last_time = newest;
    p.at_last_time = items
        .iter()
        .filter(|(t, e)| *t == newest && sound_kind(e).is_some())
        .count();
}

/// Queues one host-synthesised request (the player's footstep) ahead of the plugin, in addition
/// to the VM presentation events [`pump`] forwards.
pub fn queue_request(req: SoundRequest) {
    let mut p = match PENDING.lock() {
        Ok(p) => p,
        Err(poisoned) => poisoned.into_inner(),
    };
    p.queue.push(req);
    FORWARDED.fetch_add(1, Ordering::Relaxed);
}

/// Number of requests ever forwarded by [`pump`] (for tests).
#[cfg(test)]
pub fn forwarded_count() -> u64 {
    FORWARDED.load(Ordering::Relaxed)
}

/// Clears the queue and cursor (tests only).
#[cfg(test)]
pub fn reset_queue() {
    if let Ok(mut p) = PENDING.lock() {
        p.last_time = f64::NEG_INFINITY;
        p.at_last_time = 0;
        p.queue.clear();
    }
    FORWARDED.store(0, Ordering::Relaxed);
}

/// One line of the overlay ("last played sounds").
#[derive(Debug, Clone, PartialEq)]
pub struct PlayedEntry {
    /// VM time.
    pub time: f64,
    /// Resolution/sound label.
    pub label: String,
    /// `played`, `music`, or a failure reason.
    pub outcome: String,
    /// How the sound was resolved (e.g. `resource_ref Fix.hxc#31 c2/0`).
    pub resolution: String,
}

/// Playback statistics (reported in the overlay and by tests).
#[derive(Debug, Clone, Default)]
pub struct AudioStats {
    /// `--audio` mode in effect.
    pub enabled: bool,
    /// Requests seen from the queue.
    pub requests: usize,
    /// Sounds decoded and spawned as players.
    pub played: usize,
    /// Music tracks started.
    pub music: usize,
    /// Failures by reason label.
    pub failed: HashMap<&'static str, usize>,
    /// Sounds played non-spatially because the actor position was unknown.
    pub spatial_fallback: usize,
    /// Sounds played spatially.
    pub spatial: usize,
    /// Host-synthesised player footsteps emitted (the `--play` notify-free path).
    pub footsteps: usize,
    /// Requests whose decoded radius was applied as distance attenuation.
    pub radius_applied: usize,
    /// Requests with no radius parameter (full volume; nothing to attenuate).
    pub radius_absent: usize,
    /// Player entities expired without a sink (no audio device).
    pub no_device_expired: usize,
    /// Library scan statistics.
    pub banks_seen: usize,
    pub banks_parsed: usize,
    pub banks_failed: usize,
    pub names: usize,
    /// `.uax` packages seen/parsed and `Sound` exports indexed with a resource reference.
    pub uax_seen: usize,
    pub uax_parsed: usize,
    pub sound_exports: usize,
    pub sound_refs: usize,
    pub resources: usize,
    /// Level-audio discovery: ambient emitters found and started / failed.
    pub level_ambients: usize,
    pub level_ambients_started: usize,
    /// Level music cue source label, when one was found.
    pub level_music: Option<String>,
    /// Bounded history for the overlay.
    pub last: Vec<PlayedEntry>,
    /// Active ambient emitters (name, sound, current distance m), refreshed for the overlay.
    pub ambients: Vec<AmbientStatus>,
}

/// One active ambient emitter, for the overlay/report.
#[derive(Debug, Clone, PartialEq)]
pub struct AmbientStatus {
    /// Emitter actor name.
    pub actor: String,
    /// Resolved sound path.
    pub sound: String,
    /// Current player distance in metres, when both positions are known.
    pub distance_m: Option<f32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Played,
    Music,
    Failure(&'static str),
}

impl Outcome {
    fn as_str(self) -> &'static str {
        match self {
            Outcome::Played => "played",
            Outcome::Music => "music",
            Outcome::Failure(s) => s,
        }
    }
}

impl AudioStats {
    fn record(&mut self, time: f64, label: String, outcome: Outcome) {
        self.record_res(time, label, outcome, String::new());
    }

    fn record_res(&mut self, time: f64, label: String, outcome: Outcome, resolution: String) {
        if let Outcome::Failure(reason) = outcome {
            *self.failed.entry(reason).or_default() += 1;
        }
        self.last.push(PlayedEntry {
            time,
            label,
            outcome: outcome.as_str().to_owned(),
            resolution,
        });
        if self.last.len() > OVERLAY_HISTORY {
            self.last.remove(0);
        }
    }
}

/// `&'static str` label for a failure outcome.
fn reason_label(s: &str) -> &'static str {
    match s {
        "no_sound_name" => "no_sound_name",
        "no_name_match" => "no_name_match",
        "decode_failed" => "decode_failed",
        "stream_missing" => "stream_missing",
        "audio_off" => "audio_off",
        "no_position" => "no_position",
        "footstep_surface_missing" => "footstep_surface_missing",
        "footstep_sound_unresolved" => "footstep_sound_unresolved",
        _ => "other",
    }
}

/// Resource owning the resolver library and the stats.
#[derive(Resource)]
pub struct AudioRes {
    /// Resolver library (empty when `--audio off`). Shared behind a mutex so the VM's
    /// `VoiceDuration` provider (installed from this same library) can resolve/decode voice
    /// lengths without a second HX scan.
    pub library: Arc<Mutex<SoundLibrary>>,
    /// Playback statistics.
    pub stats: AudioStats,
}

/// A custom rodio [`Source`](bevy::audio::Source) over a chunked [`WaveStream`]. The Bevy audio
/// thread pulls [`Source::next`] and the stream decodes one bounded chunk at a time, so a long
/// streamed entry is never decoded whole on the main thread.
///
/// `looping` makes the source infinite by rewinding the decoder at end of stream (music/ambient).
/// That avoids rodio's `Repeat`, which would buffer the whole stream in memory.
pub struct StreamSample {
    /// Shared chunked decoder (kept behind a mutex because `Source` is `Send`).
    stream: std::sync::Mutex<WaveStream>,
    /// Decoded samples of the current chunk.
    buffer: std::collections::VecDeque<i16>,
    /// Interleaved samples per frame.
    channels: u16,
    /// Sample rate.
    sample_rate: u32,
    /// Rewind at end of stream instead of returning `None`.
    looping: bool,
}

impl StreamSample {
    /// Wraps a [`WaveStream`]; `looping` rewinds at end of stream.
    pub fn new(stream: WaveStream, looping: bool) -> Self {
        let channels = stream.channels();
        let sample_rate = stream.sample_rate();
        Self {
            stream: std::sync::Mutex::new(stream),
            buffer: std::collections::VecDeque::new(),
            channels,
            sample_rate,
            looping,
        }
    }

    fn refill(&mut self) -> bool {
        let mut stream = match self.stream.lock() {
            Ok(s) => s,
            Err(p) => p.into_inner(),
        };
        let Ok(chunk) = stream.next_chunk(STREAM_CHUNK_FRAMES) else {
            return false;
        };
        self.buffer.extend(chunk.samples);
        if self.buffer.is_empty() && self.looping {
            stream.rewind();
            if let Ok(chunk) = stream.next_chunk(STREAM_CHUNK_FRAMES) {
                self.buffer.extend(chunk.samples);
            }
        }
        !self.buffer.is_empty()
    }
}

impl Iterator for StreamSample {
    type Item = bevy::audio::Sample;

    fn next(&mut self) -> Option<Self::Item> {
        if self.buffer.is_empty() && !self.refill() {
            return None;
        }
        self.buffer.pop_front().map(sample_to_float)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        (0, None)
    }
}

impl bevy::audio::Source for StreamSample {
    fn current_span_len(&self) -> Option<usize> {
        // A looping stream is "infinite" (`None`); a one-shot reports its buffered remainder.
        if self.looping {
            None
        } else if self.buffer.is_empty() {
            Some(0)
        } else {
            Some(self.buffer.len())
        }
    }

    fn channels(&self) -> std::num::NonZero<u16> {
        std::num::NonZero::new(self.channels).expect("stream channels are non-zero")
    }

    fn sample_rate(&self) -> std::num::NonZero<u32> {
        std::num::NonZero::new(self.sample_rate).expect("stream sample rate is non-zero")
    }

    fn total_duration(&self) -> Option<Duration> {
        None
    }
}

/// Converts a decoded `i16` to Bevy's `f32` sample (`[-1, 1]`).
fn sample_to_float(s: i16) -> bevy::audio::Sample {
    f32::from(s) / 32768.0
}

/// Asset wrapper so [`StreamSample`] can be a Bevy audio source. The asset holds the chunked
/// decoder's spec and shared sample bytes; each player's [`Decodable::decoder`] builds its own
/// read cursor over those shared bytes, so retriggering and looping never replay the asset's
/// decode position.
#[derive(Asset, TypePath)]
pub struct StreamAudio {
    /// The wave spec (codec/channels/rate/data range) to rebuild decoders from.
    pub spec: xiii_audio::WaveSpec,
    /// Shared sample bytes (the `.hsc` file or the internal bank range).
    pub bytes: std::sync::Arc<[u8]>,
    /// Entry offset within `bytes`.
    pub offset: usize,
    /// Entry length within `bytes`.
    pub len: usize,
    /// Looping (music/ambient) or one-shot.
    pub looping: bool,
}

impl StreamAudio {
    /// Wraps a freshly opened [`WaveStream`]; `looping` rewinds at end of stream.
    pub fn new(stream: WaveStream, looping: bool) -> Self {
        let (spec, bytes, offset, len) = stream.parts();
        Self {
            spec,
            bytes,
            offset,
            len,
            looping,
        }
    }
}

impl bevy::audio::Decodable for StreamAudio {
    type Decoder = StreamSample;

    fn decoder(&self) -> Self::Decoder {
        // Build a fresh decoder over the shared sample bytes so the source has its own cursor.
        let stream = WaveStream::new(
            self.spec.clone(),
            std::sync::Arc::clone(&self.bytes),
            self.offset,
            self.len,
        )
        .expect("rebuild stream from specs");
        StreamSample::new(stream, self.looping)
    }
}

/// Marker for the plugin's own overlay text (separate from the play overlay).
#[derive(Component)]
struct AudioOverlay;

/// A music player entity (stop/replace semantics).
#[derive(Component)]
struct MusicTrack;

/// An ambient emitter player, with the data needed to recompute distance gain.
#[derive(Component)]
struct AmbientEmitter {
    /// Emitter actor name.
    actor: String,
    /// Resolved sound path.
    sound: String,
    /// Emitter position in Bevy space (metres).
    position: Vec3,
    /// Roll-off parameters in Unreal units.
    rolloff: Attenuation,
    /// Base linear gain (from `SoundVolume`/class default; 1.0 when absent).
    base_gain: f32,
    /// Last gain written to the live sink, so the sink is only touched when it changes.
    applied_gain: f32,
}

/// A spawned sound entity with its spawn time, so a missing audio device can be expired.
#[derive(Component)]
struct SpawnedSound {
    at: Instant,
}

/// Per-frame cached emitter gains, refreshed on a slow timer.
#[derive(Resource, Default)]
struct AmbientGains {
    last_update: Option<Instant>,
}

/// Mirrored main-listener position (Bevy metres), refreshed from the play camera each frame.
#[derive(Resource, Default)]
struct ListenerPos(Option<Vec3>);

/// Playback plugin for `--play`.
pub struct AudioFxPlugin {
    /// Parsed options (game dir + `--audio`).
    pub options: Options,
}

#[derive(Resource)]
struct AudioConfig {
    options: Options,
}

impl Plugin for AudioFxPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(AudioConfig {
            options: self.options.clone(),
        })
        .init_resource::<AmbientGains>()
        .init_resource::<ListenerPos>()
        // Register the custom streamed source type with the audio plugin.
        .add_audio_source::<StreamAudio>()
        .add_systems(Startup, setup_audio)
        .add_systems(
            Update,
            (
                mirror_listener,
                ensure_listener,
                consume_requests,
                update_ambient_gain,
                expire_without_device,
                overlay_audio,
            )
                .chain(),
        )
        .add_systems(Last, report_audio_exit);
    }
}

/// Prints one final `[audio]` summary line when the app is about to exit, so an unattended run
/// records what was played.
fn report_audio_exit(
    audio: Option<Res<AudioRes>>,
    mut exiting: MessageReader<AppExit>,
    mut done: Local<bool>,
) {
    if *done || exiting.read().next().is_none() {
        return;
    }
    *done = true;
    let Some(audio) = audio else {
        return;
    };
    let s = &audio.stats;
    let failures: Vec<String> = s.failed.iter().map(|(k, v)| format!("{k}={v}")).collect();
    println!(
        "[audio] exit: enabled={} requests={} played={} music={} steps={} spatial={} fallback={} radius_applied={} radius_absent={} no_device_expired={} level_ambients={} level_ambients_started={} level_music={} failures=[{}]",
        s.enabled,
        s.requests,
        s.played,
        s.music,
        s.footsteps,
        s.spatial,
        s.spatial_fallback,
        s.radius_applied,
        s.radius_absent,
        s.no_device_expired,
        s.level_ambients,
        s.level_ambients_started,
        s.level_music.as_deref().unwrap_or("-"),
        failures.join(", ")
    );
    for a in &s.ambients {
        match a.distance_m {
            Some(d) => println!("[audio]   ambient {} {} at {d:.1} m", a.actor, a.sound),
            None => println!("[audio]   ambient {} {}", a.actor, a.sound),
        }
    }
    for e in &s.last {
        if e.resolution.is_empty() {
            println!("[audio]   last [{:.3}s] {} {}", e.time, e.outcome, e.label);
        } else {
            println!(
                "[audio]   last [{:.3}s] {} {} [{}]",
                e.time, e.outcome, e.label, e.resolution
            );
        }
    }
}

/// Scans the installation's HX banks (read-only) and installs the resolver, unless `--audio off`.
fn setup_audio(mut commands: Commands, cfg: Res<AudioConfig>) {
    let enabled = cfg.options.audio == crate::cli::Audio::On;
    let stats = AudioStats {
        enabled,
        ..Default::default()
    };
    if !enabled {
        commands.insert_resource(AudioRes {
            library: Arc::new(Mutex::new(SoundLibrary::empty())),
            stats,
        });
        println!("[audio] disabled (--audio off): VM sound events are counted, not played");
        return;
    }
    let Some(dir) = cfg.options.game_dir.clone() else {
        commands.insert_resource(AudioRes {
            library: Arc::new(Mutex::new(SoundLibrary::empty())),
            stats,
        });
        println!("[audio] no --game-dir; sound playback disabled");
        return;
    };
    let started = Instant::now();
    let library = Arc::new(Mutex::new(SoundLibrary::scan(&dir)));
    let s = library.lock().map(|l| l.stats()).unwrap_or_default();
    let mut stats = stats;
    stats.banks_seen = s.banks_seen;
    stats.banks_parsed = s.banks_parsed;
    stats.banks_failed = s.banks_failed;
    stats.names = s.names;
    stats.uax_seen = s.uax_seen;
    stats.uax_parsed = s.uax_parsed;
    stats.sound_exports = s.sound_exports;
    stats.sound_refs = s.sound_refs;
    stats.resources = s.resources;
    println!(
        "[audio] HX banks: {} seen, {} parsed, {} failed; {} named sounds, {} resource pairs, \
         {} Sound exports with a resource reference; .uax {} seen/{} parsed ({:.2}s)",
        s.banks_seen,
        s.banks_parsed,
        s.banks_failed,
        s.names,
        s.resources,
        s.sound_refs,
        s.uax_seen,
        s.uax_parsed,
        started.elapsed().as_secs_f32()
    );
    commands.insert_resource(AudioRes { library, stats });
}

/// Query over the play camera(s): entities that may need a [`SpatialListener`].
type ListenerCams<'w, 's> = Query<
    'w,
    's,
    (Entity, Option<&'static SpatialListener>),
    (With<Camera3d>, Without<crate::viewer::SkyCamera>),
>;

/// Attaches a [`SpatialListener`] to the main play camera once (the sky camera is excluded).
fn ensure_listener(mut commands: Commands, cams: ListenerCams) {
    for (entity, listener) in &cams {
        if listener.is_none() {
            commands
                .entity(entity)
                .insert(SpatialListener::new(EAR_GAP_M));
        }
    }
}

/// Drains the process-global queue, resolves/decodes each request and spawns the player.
#[allow(clippy::too_many_arguments)]
fn consume_requests(
    mut commands: Commands,
    audio: Option<ResMut<AudioRes>>,
    mut sources: ResMut<Assets<AudioSource>>,
    mut streams: ResMut<Assets<StreamAudio>>,
    names: Query<(&Name, &GlobalTransform)>,
    music: Query<Entity, With<MusicTrack>>,
    mut music_sinks: Query<&mut AudioSink, With<MusicTrack>>,
    listener: Res<ListenerPos>,
    cfg: Res<AudioConfig>,
    mut started_level_audio: Local<bool>,
) {
    let Some(mut audio) = audio else {
        return;
    };
    // Level audio (ambients + music cue) is started once, on the first Update after startup.
    if !*started_level_audio {
        *started_level_audio = true;
        if audio.stats.enabled {
            start_level_audio(&mut commands, &mut audio, &mut streams, &cfg.options);
        }
    }

    if MENU_STOP_MUSIC.swap(0, Ordering::Relaxed) != 0 {
        for entity in &music {
            commands.entity(entity).despawn();
        }
    }
    let gain = 10.0f32.powf(f32::from_bits(MENU_MASTER_DB.load(Ordering::Relaxed)) / 20.0);
    let music_gain = if MENU_MUSIC_ENABLED.load(Ordering::Relaxed) == 0 {
        0.0
    } else {
        gain
    };
    for mut sink in &mut music_sinks {
        sink.set_volume(Volume::Linear(music_gain));
    }

    // Take the whole queue once.
    let requests = {
        let mut p = match PENDING.lock() {
            Ok(p) => p,
            Err(poisoned) => poisoned.into_inner(),
        };
        std::mem::take(&mut p.queue)
    };
    if requests.is_empty() {
        return;
    }

    // Actor world positions (Bevy metres), lazily built only when a request needs one.
    let positions = |actor: &str| -> Option<Vec3> {
        let prefix = format!("{actor} -> ");
        let mut found: Option<(Vec3, usize)> = None;
        for (name, t) in &names {
            let s = name.as_str();
            let is_actor = s == actor || s.starts_with(&prefix);
            if !is_actor {
                continue;
            }
            let p = t.translation();
            match &mut found {
                Some((sum, count)) => {
                    *sum += p;
                    *count += 1;
                }
                None => found = Some((p, 1)),
            }
        }
        found.map(|(sum, count)| sum / count as f32)
    };

    for req in requests {
        audio.stats.requests += 1;
        if !audio.stats.enabled {
            audio
                .stats
                .record(req.time, label(&req), Outcome::Failure("audio_off"));
            continue;
        }
        match req.kind {
            SoundKind::Music => play_music(&mut commands, &mut audio, &mut sources, &req, &music),
            SoundKind::Sound | SoundKind::Rolloff | SoundKind::Footstep => play_sound(
                &mut commands,
                &mut audio,
                &mut sources,
                &req,
                &positions,
                listener.0,
            ),
        }
    }
}

/// Discovers the map's level audio and starts the ambient emitters and music cue. This is the
/// level-placed counterpart to the VM's `PlaySound`/`PlayMusic` events.
fn start_level_audio(
    commands: &mut Commands,
    audio: &mut AudioRes,
    streams: &mut Assets<StreamAudio>,
    options: &Options,
) {
    let (Some(dir), Some(map)) = (options.game_dir.clone(), options.map.clone()) else {
        return;
    };
    let level = match LevelAudio::discover(&dir, &map) {
        Ok(l) => l,
        Err(e) => {
            println!("[audio] level audio discovery failed: {e}");
            return;
        }
    };
    audio.stats.level_ambients = level.ambients.len();
    audio.stats.level_music = level.music.as_ref().map(|m| m.source.as_str().to_owned());
    let mut started = 0usize;
    for amb in &level.ambients {
        let Some(sound) = amb.sound.as_deref() else {
            audio.stats.record(
                0.0,
                format!("ambient {}", amb.actor),
                Outcome::Failure("no_sound_name"),
            );
            continue;
        };
        let stream = {
            let library = audio.library.clone();
            let library = library.lock().unwrap_or_else(|e| e.into_inner());
            let Some(r) = library.resolve_path(sound) else {
                audio.stats.record(
                    0.0,
                    format!("ambient {}", amb.actor),
                    Outcome::Failure("no_name_match"),
                );
                continue;
            };
            match library.open_stream(&r.entry) {
                Ok(s) => s,
                Err(e) => {
                    audio.stats.record(
                        0.0,
                        format!("ambient {}", amb.actor),
                        Outcome::Failure(reason_label(e.as_str())),
                    );
                    continue;
                }
            }
        };
        let channels = stream.channels();
        let sample_rate = stream.sample_rate();
        let handle = streams.add(StreamAudio::new(stream, true));
        // Emitter position: map coordinates (Unreal) -> Bevy metres via the shared policy.
        let bevy_pos = unreal_to_bevy(amb.location[0], amb.location[1], amb.location[2]);
        let attenuation = Attenuation::from_actor(
            amb.rolloff.saturation_distance,
            amb.rolloff.stabilisation_distance,
            amb.rolloff.stabilisation_volume_db,
        )
        .unwrap_or_default();
        commands.spawn((
            Name::new(format!("ambient {}", amb.actor)),
            AmbientEmitter {
                actor: amb.actor.clone(),
                sound: sound.to_owned(),
                position: bevy_pos,
                rolloff: attenuation,
                base_gain: 1.0,
                applied_gain: 1.0,
            },
            AudioPlayer(handle),
            Transform::from_translation(bevy_pos),
            PlaybackSettings::ONCE
                .with_spatial(true)
                .with_spatial_scale(NO_RODIO_ROLLOFF)
                .with_volume(Volume::Linear(1.0)),
            SpawnedSound { at: Instant::now() },
        ));
        started += 1;
        println!(
            "[audio] ambient {} -> {} ({} ch {sample_rate} Hz) at {:?} sat={} stab={}",
            amb.actor,
            sound,
            channels,
            bevy_pos,
            amb.rolloff.saturation_distance,
            amb.rolloff.stabilisation_distance
        );
    }
    audio.stats.level_ambients_started = started;

    if let Some(cue) = &level.music {
        start_level_music(commands, audio, streams, cue);
    }
    for (k, v) in &level.counters {
        println!("[audio] level counter {v} {k}");
    }
}

/// Streams the level's music cue on a non-spatial music channel, replacing any current track.
fn start_level_music(
    commands: &mut Commands,
    audio: &mut AudioRes,
    streams: &mut Assets<StreamAudio>,
    cue: &MusicCue,
) {
    let stream = {
        let library = audio.library.clone();
        let library = library.lock().unwrap_or_else(|e| e.into_inner());
        let Some(r) = library.resolve_path(&cue.sound) else {
            audio.stats.record(
                0.0,
                format!("music {}", cue.sound),
                Outcome::Failure("no_name_match"),
            );
            return;
        };
        match library.open_stream(&r.entry) {
            Ok(s) => s,
            Err(e) => {
                audio.stats.record(
                    0.0,
                    format!("music {}", cue.sound),
                    Outcome::Failure(reason_label(e.as_str())),
                );
                return;
            }
        }
    };
    let channels = stream.channels();
    let sample_rate = stream.sample_rate();
    let total_frames = stream.total_frames();
    let handle = streams.add(StreamAudio::new(stream, true));
    commands.spawn((
        Name::new(format!("music {} [{}]", cue.sound, cue.source.as_str())),
        MusicTrack,
        AudioPlayer(handle),
        SpawnedSound { at: Instant::now() },
        PlaybackSettings::ONCE.with_volume(Volume::Linear(1.0)),
    ));
    audio.stats.music += 1;
    audio.stats.record_res(
        0.0,
        format!("music {} ({})", cue.sound, cue.source.as_str()),
        Outcome::Music,
        format!("stream {} frames", total_frames),
    );
    println!(
        "[audio] level music {} (source {}, {} ch {sample_rate} Hz, {} frames)",
        cue.sound,
        cue.source.as_str(),
        channels,
        total_frames
    );
}

/// Resolves and decodes one request to an in-memory WAV `AudioSource` handle, plus a short
/// description of how it was resolved (rule, bank entry, candidate count/choice).
fn source_for(
    audio: &mut AudioRes,
    sources: &mut Assets<AudioSource>,
    req: &SoundRequest,
) -> Result<(Handle<AudioSource>, String), ResolveFailure> {
    let Some(path) = &req.sound else {
        return Err(ResolveFailure::NoSoundName);
    };
    let library = audio.library.clone();
    let mut library = library.lock().unwrap_or_else(|e| e.into_inner());
    let Some(r) = library.resolve_path(path) else {
        return Err(ResolveFailure::NoNameMatch);
    };
    let pcm = library.load(&r.entry)?;
    let file = r
        .entry
        .bank
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| r.entry.bank.display().to_string());
    let desc = format!(
        "{} {}#{} c{}/{}",
        r.rule.as_str(),
        file,
        r.entry.entry,
        r.candidates,
        r.chosen
    );
    Ok((
        sources.add(AudioSource {
            bytes: wav_bytes(&pcm).into(),
        }),
        desc,
    ))
}

/// Volume mapping hypothesis: UT-style `Volume` 0..255 -> linear gain `v/255`; absent/zero
/// means full volume. Applied only when the value is present and positive.
fn volume_of(req: &SoundRequest) -> Volume {
    match req.volume {
        Some(v) if v > 0 => Volume::Linear((v as f32 / 255.0).clamp(0.0, 4.0)),
        _ => Volume::Linear(1.0),
    }
}

/// Pitch mapping hypothesis: a positive `Pitch` is a playback speed multiplier; absent means 1.0.
fn speed_of(req: &SoundRequest) -> f32 {
    match req.pitch {
        Some(p) if p > 0 => (p as f32 / 100.0).clamp(0.25, 4.0),
        _ => 1.0,
    }
}

/// Distance attenuation for a request, from its decoded `radius` (`Param3`). The gain is applied
/// by scaling the playback volume because Bevy/rodio's spatial path has no configurable roll-off.
fn attenuation_of(req: &SoundRequest) -> Option<Attenuation> {
    req.radius.and_then(Attenuation::from_radius)
}

fn label(req: &SoundRequest) -> String {
    let kind = match req.kind {
        SoundKind::Sound => "sound",
        SoundKind::Music => "music",
        SoundKind::Rolloff => "rolloff",
        SoundKind::Footstep => "footstep",
    };
    format!(
        "{kind} {} {}",
        req.actor,
        req.sound.as_deref().unwrap_or("<no path>")
    )
}

fn play_sound(
    commands: &mut Commands,
    audio: &mut AudioRes,
    sources: &mut Assets<AudioSource>,
    req: &SoundRequest,
    positions: &impl Fn(&str) -> Option<Vec3>,
    listener: Option<Vec3>,
) {
    let (handle, resolution) = match source_for(audio, sources, req) {
        Ok(h) => h,
        Err(reason) => {
            audio
                .stats
                .record(req.time, label(req), Outcome::Failure(reason.as_str()));
            return;
        }
    };
    // Distance attenuation: when a radius is decoded, apply the roll-off gain relative to the
    // emitting actor's distance to the listener. Bevy cannot do this, so the host computes it.
    let pos = positions(&req.actor);
    let attenuation = attenuation_of(req);
    if req.kind == SoundKind::Footstep {
        audio.stats.footsteps += 1;
    }
    let mut gain = volume_of(req).to_linear();
    if let Some(att) = attenuation {
        audio.stats.radius_applied += 1;
        if let Some(p) = pos {
            let dist_m = listener.map_or(0.0, |l| l.distance(p));
            let dist_uu = dist_m * xiii_decode::common::UNREAL_UNITS_PER_METER;
            gain *= att.gain(dist_uu);
        }
    } else {
        audio.stats.radius_absent += 1;
    }
    let mut settings = PlaybackSettings::DESPAWN
        .with_volume(Volume::Linear(gain))
        .with_speed(speed_of(req))
        .with_spatial_scale(NO_RODIO_ROLLOFF);
    let mut entity = commands.spawn((
        Name::new(format!("audio {}", label(req))),
        AudioPlayer::new(handle),
        SpawnedSound { at: Instant::now() },
    ));
    match pos {
        Some(p) => {
            settings.spatial = true;
            entity.insert((Transform::from_translation(p), settings));
            audio.stats.spatial += 1;
        }
        None => {
            entity.insert(settings);
            audio.stats.spatial_fallback += 1;
        }
    }
    audio.stats.played += 1;
    audio
        .stats
        .record_res(req.time, label(req), Outcome::Played, resolution);
}

fn play_music(
    commands: &mut Commands,
    audio: &mut AudioRes,
    sources: &mut Assets<AudioSource>,
    req: &SoundRequest,
    music: &Query<Entity, With<MusicTrack>>,
) {
    let (handle, resolution) = match source_for(audio, sources, req) {
        Ok(h) => h,
        Err(reason) => {
            audio
                .stats
                .record(req.time, label(req), Outcome::Failure(reason.as_str()));
            return;
        }
    };
    // Stop/replace: despawn every previous music entity (dropping its sink stops playback).
    for e in music {
        commands.entity(e).despawn();
    }
    commands.spawn((
        Name::new(format!("audio {}", label(req))),
        MusicTrack,
        AudioPlayer::new(handle),
        SpawnedSound { at: Instant::now() },
        PlaybackSettings::DESPAWN
            .with_volume(Volume::Linear(
                volume_of(req).to_linear()
                    * 10.0f32.powf(f32::from_bits(MENU_MASTER_DB.load(Ordering::Relaxed)) / 20.0)
                    * if MENU_MUSIC_ENABLED.load(Ordering::Relaxed) == 0 {
                        0.0
                    } else {
                        1.0
                    },
            ))
            .with_speed(speed_of(req)),
    ));
    audio.stats.music += 1;
    audio.stats.played += 1;
    audio
        .stats
        .record_res(req.time, label(req), Outcome::Music, resolution);
}

/// Recomputes the distance gain of every ambient emitter from the listener position, on a slow
/// timer. Bevy only pans spatial audio; this is the roll-off the host adds. The live sink's
/// volume is set directly (`PlaybackSettings` changes do not affect an already-playing sink).
fn update_ambient_gain(
    mut gains: ResMut<AmbientGains>,
    mut emitters: Query<(&mut AmbientEmitter, Option<&mut SpatialAudioSink>)>,
    listener: Res<ListenerPos>,
    audio: Option<ResMut<AudioRes>>,
) {
    let Some(mut audio) = audio else {
        return;
    };
    let now = Instant::now();
    let elapsed = gains
        .last_update
        .map_or(ATTENUATION_PERIOD, |t| now.duration_since(t));
    if elapsed < ATTENUATION_PERIOD {
        return;
    }
    gains.last_update = Some(now);
    let Some(listener) = listener.0 else {
        return;
    };
    audio.stats.ambients.clear();
    for (mut emitter, sink) in &mut emitters {
        let dist_m = listener.distance(emitter.position);
        let dist_uu = dist_m * xiii_decode::common::UNREAL_UNITS_PER_METER;
        let gain = (emitter.rolloff.gain(dist_uu) * emitter.base_gain).clamp(0.0, 4.0);
        if (gain - emitter.applied_gain).abs() > 1e-4
            && let Some(mut sink) = sink
        {
            sink.set_volume(Volume::Linear(gain));
            emitter.applied_gain = gain;
        }
        audio.stats.ambients.push(AmbientStatus {
            actor: emitter.actor.clone(),
            sound: emitter.sound.clone(),
            distance_m: Some(dist_m),
        });
    }
}

/// Drops one-shot sound entities that never received a sink (no audio device) after
/// [`PENDING_TIMEOUT`]. Ambient and music players are excluded: they are a fixed, bounded set and
/// the overlay/report keeps listing them even without a device.
#[allow(clippy::type_complexity)]
fn expire_without_device(
    mut commands: Commands,
    mut audio: Option<ResMut<AudioRes>>,
    q: Query<
        (Entity, &SpawnedSound),
        (
            Without<AudioSink>,
            Without<AmbientEmitter>,
            Without<MusicTrack>,
        ),
    >,
) {
    let Some(audio) = audio.as_mut() else {
        return;
    };
    let now = Instant::now();
    for (entity, spawned) in &q {
        if now.duration_since(spawned.at) >= PENDING_TIMEOUT {
            commands.entity(entity).despawn();
            audio.stats.no_device_expired += 1;
        }
    }
}

fn overlay_audio(
    diagnostics: Option<Res<crate::play::DiagnosticOverlay>>,
    audio: Option<Res<AudioRes>>,
    cfg: Res<AudioConfig>,
    mut text: Query<(&mut Text, &mut Visibility), With<AudioOverlay>>,
    mut commands: Commands,
) {
    if cfg.options.mode == crate::cli::Mode::Menu {
        return;
    }
    let Some(audio) = audio else {
        return;
    };
    if text.is_empty() {
        commands.spawn((
            AudioOverlay,
            Visibility::Hidden,
            Text::new("audio init"),
            TextFont {
                font_size: FontSize::Px(12.0),
                ..default()
            },
            TextColor(Color::srgb(0.8, 0.9, 1.0)),
            Node {
                position_type: PositionType::Absolute,
                bottom: px(6),
                left: px(6),
                padding: UiRect::all(px(4)),
                max_width: px(560),
                ..default()
            },
            BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.5)),
        ));
        return;
    }
    let Ok((mut text, mut visibility)) = text.single_mut() else {
        return;
    };
    let visible = diagnostics.as_deref().is_some_and(|d| d.0);
    *visibility = if visible {
        Visibility::Visible
    } else {
        Visibility::Hidden
    };
    if !visible {
        return;
    }
    let s = &audio.stats;
    let mut out = format!(
        "audio {} | requests {} played {} music {} steps {} spatial {} (fallback {}) failed {} | radius applied {} absent {} | no-device {}\n",
        if s.enabled { "on" } else { "off" },
        s.requests,
        s.played,
        s.music,
        s.footsteps,
        s.spatial,
        s.spatial_fallback,
        s.failed.values().sum::<usize>(),
        s.radius_applied,
        s.radius_absent,
        s.no_device_expired,
    );
    out.push_str(&format!(
        "level: {} ambient emitter(s), {} started, music {} | \n",
        s.level_ambients,
        s.level_ambients_started,
        s.level_music.as_deref().unwrap_or("-")
    ));
    if !s.ambients.is_empty() {
        out.push_str("active ambients: ");
        let lines: Vec<String> = s
            .ambients
            .iter()
            .map(|a| match a.distance_m {
                Some(d) => format!("{} {} @{d:.1}m", a.actor, a.sound),
                None => format!("{} {}", a.actor, a.sound),
            })
            .collect();
        out.push_str(&lines.join(" | "));
        out.push('\n');
    }
    if s.last.is_empty() {
        out.push_str("last: (none)");
    } else {
        out.push_str("last: ");
        let lines: Vec<String> = s
            .last
            .iter()
            .map(|e| {
                if e.resolution.is_empty() {
                    format!("[{:.2}s] {} {}", e.time, e.outcome, e.label)
                } else {
                    format!(
                        "[{:.2}s] {} {} [{}]",
                        e.time, e.outcome, e.label, e.resolution
                    )
                }
            })
            .collect();
        out.push_str(&lines.join(" | "));
    }
    text.0 = out;
}

/// Encodes decoded PCM as a canonical PCM16 WAV in memory.
fn wav_bytes(pcm: &PcmAudio) -> Vec<u8> {
    xiii_audio::write_wav(pcm)
}

/// Mirrors the main play camera's translation into [`ListenerPos`] each frame, so attenuation and
/// the ambient overlay use the listener actually attached to the camera.
fn mirror_listener(
    cams: Query<&GlobalTransform, (With<Camera3d>, Without<crate::viewer::SkyCamera>)>,
    mut listener: ResMut<ListenerPos>,
) {
    for t in &cams {
        listener.0 = Some(t.translation());
    }
}

/// Converts Unreal units to Bevy metres using the single shared coordinate policy.
fn unreal_to_bevy(x: f32, y: f32, z: f32) -> Vec3 {
    let v = xiii_decode::common::to_bevy_position([x, y, z]);
    Vec3::new(v[0], v[1], v[2])
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_script::SoundEvent;

    fn event(kind: SoundKind, sound: Option<&str>, time: f64) -> PresentationEvent {
        let e = SoundEvent {
            actor: "XIIIPawn0".into(),
            sound: sound.map(str::to_owned),
            rolloff_actor: None,
            slot: None,
            volume: Some(200),
            radius: Some(80),
            pitch: Some(100),
            param5: None,
            time,
        };
        match kind {
            SoundKind::Sound => PresentationEvent::PlaySound(e),
            SoundKind::Music => PresentationEvent::PlayMusic(e),
            SoundKind::Rolloff => PresentationEvent::PlayRolloffSound(e),
            // Footsteps are host-synthesised with `SoundRequest::footstep`, not built from an
            // event; the test helper maps it to `PlaySound` for field coverage.
            SoundKind::Footstep => PresentationEvent::PlaySound(e),
        }
    }

    #[test]
    fn request_maps_event_fields() {
        let r = SoundRequest::from_event(
            &event(SoundKind::Sound, Some("XIIIsound.Guns.M16Fire1"), 3.5),
            SoundKind::Sound,
        );
        assert_eq!(r.kind, SoundKind::Sound);
        assert_eq!(r.sound.as_deref(), Some("XIIIsound.Guns.M16Fire1"));
        assert_eq!(r.volume, Some(200));
        assert_eq!(r.radius, Some(80));
        assert_eq!(r.pitch, Some(100));
        assert_eq!(r.time, 3.5);
    }

    #[test]
    fn volume_and_speed_hypotheses() {
        let mut r = SoundRequest::from_event(&event(SoundKind::Sound, None, 0.0), SoundKind::Sound);
        assert!((volume_of(&r).to_linear() - 200.0 / 255.0).abs() < 1e-4);
        r.volume = None;
        assert_eq!(volume_of(&r).to_linear(), 1.0);
        assert_eq!(speed_of(&r), 1.0);
        r.pitch = Some(200);
        assert!((speed_of(&r) - 2.0).abs() < 1e-4);
        r.pitch = Some(0);
        assert_eq!(speed_of(&r), 1.0);
    }

    /// The radius parameter selects a distance roll-off, so `radius_unsupported` is gone: it is
    /// now `radius_applied`. A request with no radius is counted `radius_absent`.
    #[test]
    fn radius_selects_attenuation() {
        let mut r = SoundRequest::from_event(&event(SoundKind::Sound, None, 0.0), SoundKind::Sound);
        let a = attenuation_of(&r).expect("radius 80 selects attenuation");
        assert_eq!(a.saturation_distance, 80.0);
        // Full volume at the emitter, quieter far away.
        assert_eq!(a.gain(0.0), 1.0);
        assert!(a.gain(1_000_000.0) < 1.0);
        r.radius = None;
        assert!(attenuation_of(&r).is_none());
        r.radius = Some(0);
        assert!(attenuation_of(&r).is_none());
    }

    /// The WAV built from a decoded sound decodes back to the same samples (round trip).
    #[test]
    fn wav_bytes_round_trip_header() {
        let pcm = PcmAudio {
            channels: 2,
            sample_rate: 22050,
            samples: vec![1, -1, 2, -2],
        };
        let w = wav_bytes(&pcm);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..12], b"WAVE");
        assert_eq!(u16::from_le_bytes([w[20], w[21]]), 1); // PCM
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 2); // stereo
        assert_eq!(u32::from_le_bytes([w[24], w[25], w[26], w[27]]), 22050);
        assert_eq!(u32::from_le_bytes([w[40], w[41], w[42], w[43]]), 8); // data bytes
    }

    /// `XIII_GOG_DIR` resolved against the workspace root, or `None` in CI.
    fn opt_in_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in corpus: run the **production** `play::session::Session` for 30 s of a scripted
    /// Plage00 walk, then build a minimal VM with `TriggerSound0` (whose map property is
    /// `Sound'XIIIsound.Interface.EndBig'`) to force a real emitted sound event, and resolve
    /// every emitted sound event through the real install's HX banks. Reports resolved/unresolved
    /// counts by reason for both.
    #[test]
    fn opt_in_plage00_emitted_sound_events_resolve() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        use xiii_package::Limits;
        use xiii_script::{ObjRef, ScriptSet, Value, Vm, VmLimits};
        use xiii_world::audio::LevelAudio;
        use xiii_world::runtime;

        let mut lib = SoundLibrary::scan(&game_dir);
        let stats = lib.stats();
        assert!(
            stats.banks_parsed > 0,
            "no HX banks parsed under the install"
        );
        {
            use crate::play::script::{Drive, Script};
            use crate::play::sim::PlayerSim;
            let mut session =
                crate::play::session::Session::open(&game_dir, "Plage00").expect("session");
            let mut sim = PlayerSim::new([3380.0, -44620.2, 4918.0], 0.0);
            sim.grounded = true;
            let script =
                Script::parse("t=0.0 forward 1\nt=0.5 forward 0\nt=1.0 jump\nt=2.0 jump\n")
                    .unwrap();
            let mut drive = Drive::new(&script);
            let dt = 1.0 / 60.0f32;
            for tick in 0..1800 {
                let elapsed = tick as f32 * dt;
                let _ = drive.advance(elapsed, &mut sim);
                session.step(
                    dt,
                    sim.location,
                    sim.yaw,
                    sim.velocity,
                    &crate::play::session::PlayerVMModes::default(),
                );
            }
            let prod_sound = session
                .events
                .iter()
                .filter(|(_, ev)| {
                    matches!(
                        ev,
                        PresentationEvent::PlaySound(_)
                            | PresentationEvent::PlayMusic(_)
                            | PresentationEvent::PlayRolloffSound(_)
                    )
                })
                .count();
            println!(
                "[audio test] production Plage00 session (30 s): {prod_sound} sound events emitted"
            );
        }

        let (mut set, failures) = runtime::load_install(&game_dir).expect("load install");
        assert!(failures.is_empty(), "script load failures: {failures:?}");
        let install =
            xiii_install::Installation::open(&game_dir, &xiii_install::OpenOptions::default())
                .expect("open install");
        let mut uax = 0usize;
        for entry in install.packages() {
            if entry.kind != xiii_install::PackageKind::Sound {
                continue;
            }
            let data = std::fs::read(&entry.path).expect("read uax");
            let p = xiii_script::ScriptPackage::load(
                &entry.name,
                data,
                &xiii_script::ScriptLimits::default(),
                &Limits::default(),
            )
            .expect("parse uax");
            set.add(p);
            uax += 1;
        }
        println!("[audio test] loaded {uax} .uax sound packages");
        let map_path = runtime::find_map(&game_dir, "Plage00")
            .expect("find map")
            .expect("map present");
        let map_data = std::fs::read(&map_path).expect("read map");
        let map_pkg = xiii_script::ScriptPackage::load(
            "Plage00",
            map_data,
            &xiii_script::ScriptLimits::default(),
            &Limits::default(),
        )
        .expect("parse map");
        let map_idx = set.add(map_pkg);
        let set: &'static ScriptSet = Box::leak(Box::new(set));
        let mut vm = Vm::new(set, VmLimits::default());
        let actors = vm
            .load_level(map_idx, &Limits::default())
            .expect("load level");
        for &id in &actors {
            vm.set_active(id, true);
        }
        let default_game = runtime::default_game_from_ini(&game_dir).expect("DefaultGame");
        let game_class = runtime::resolve_class_path(set, &default_game).expect("game class");
        let _ = runtime::begin_play_all(&mut vm, &actors, game_class);
        let mut with_path = 0usize;
        let mut resolved = 0usize;
        let mut failed: HashMap<&'static str, usize> = HashMap::new();
        for _ in 0..600 {
            let _ = vm.tick_suspending(0.05);
            for ev in vm.drain_events() {
                if !matches!(
                    ev,
                    PresentationEvent::PlaySound(_)
                        | PresentationEvent::PlayMusic(_)
                        | PresentationEvent::PlayRolloffSound(_)
                ) {
                    continue;
                }
                let req = SoundRequest::from_event(&ev, SoundKind::Sound);
                if let Some(path) = &req.sound {
                    with_path += 1;
                    match lib.resolve_path(path) {
                        Some(r) => match lib.load(&r.entry) {
                            Ok(_) => resolved += 1,
                            Err(e) => *failed.entry(e.as_str()).or_default() += 1,
                        },
                        None => {
                            *failed
                                .entry(ResolveFailure::NoNameMatch.as_str())
                                .or_default() += 1
                        }
                    }
                }
            }
        }
        println!(
            "[audio test] Plage00 with .uax loaded (30 s): {with_path} sound events with a path, \
             {resolved} resolved/decoded, failures {failed:?}"
        );
        assert_eq!(
            resolved, with_path,
            "every emitted sound event with a path must resolve: failures {failed:?}"
        );

        // ---- forced TriggerSound0 event (map property Sound'XIIISound.Interface.EndBig') ----
        let ts = vm
            .find_object("TriggerSound0")
            .expect("Plage00 has TriggerSound0");
        let pawn = vm
            .spawn(
                runtime::find_class(set, "xiii", "XIIIPlayerPawn").expect("player class"),
                "XIIIPlayerPawn(item6c)",
            )
            .expect("spawn test pawn");
        let arg = || Value::Object(Some(ObjRef::Instance(pawn)));
        vm.send_event(ts, "Trigger", vec![arg(), arg()])
            .expect("TriggerSound0.Trigger");
        let mut forced = 0usize;
        let mut forced_resolved = 0usize;
        for ev in vm.drain_events() {
            if !matches!(
                ev,
                PresentationEvent::PlaySound(_)
                    | PresentationEvent::PlayMusic(_)
                    | PresentationEvent::PlayRolloffSound(_)
            ) {
                continue;
            }
            forced += 1;
            let req = SoundRequest::from_event(&ev, SoundKind::Sound);
            if let Some(path) = &req.sound
                && let Some(r) = lib.resolve_path(path)
                && lib.load(&r.entry).is_ok()
            {
                forced_resolved += 1;
                println!("[audio test] forced TriggerSound0 resolved {path}");
            }
        }
        assert!(forced > 0, "TriggerSound0.Trigger emitted no sound event");
        assert_eq!(
            forced_resolved, forced,
            "the real TriggerSound0 event must resolve through the HX banks"
        );

        // ---- item6d: level audio discovery (ambients + music) resolves through the library ----
        let level = LevelAudio::discover(&game_dir, "Plage00").expect("level audio");
        println!(
            "[audio test] level audio Plage00: {} ambient(s), music {:?}",
            level.ambients.len(),
            level.music.as_ref().map(|m| m.sound.as_str())
        );
        assert!(
            !level.ambients.is_empty(),
            "Plage00 must have placed ambient emitters"
        );
        for amb in &level.ambients {
            let sound = amb.sound.as_deref().expect("non-null AmbientSound");
            let r = lib
                .resolve_path(sound)
                .unwrap_or_else(|| panic!("ambient {sound} did not resolve"));
            let stream = lib
                .open_stream(&r.entry)
                .unwrap_or_else(|e| panic!("ambient {sound} stream: {e:?}"));
            println!(
                "[audio test] ambient {} -> {} ({} ch {} Hz, {} frames)",
                amb.actor,
                sound,
                stream.channels(),
                stream.sample_rate(),
                stream.total_frames()
            );
        }
        let music = level.music.as_ref().expect("a music cue");
        let r = lib
            .resolve_path(&music.sound)
            .unwrap_or_else(|| panic!("music {} did not resolve", music.sound));
        let stream = lib
            .open_stream(&r.entry)
            .unwrap_or_else(|e| panic!("music stream: {e:?}"));
        println!(
            "[audio test] music {} (source {}) -> {}#{} ({} ch {} Hz, {} frames)",
            music.sound,
            music.source.as_str(),
            r.entry.bank.display(),
            r.entry.entry,
            stream.channels(),
            stream.sample_rate(),
            stream.total_frames(),
        );
        assert!(
            stream.total_frames() > 0,
            "level music decoded to zero frames"
        );
    }

    /// The pump forwards only sound/music events, once each, even when the retained event window
    /// grows and is later trimmed; a non-sound event is ignored.
    #[test]
    fn pump_forwards_new_sound_events_once() {
        reset_queue();
        let refresh = |t: f64| {
            (
                t,
                PresentationEvent::RefreshDisplaying {
                    actor: "X".into(),
                    time: t,
                },
            )
        };
        let mut events = vec![
            (1.0, event(SoundKind::Sound, Some("A"), 1.0)),
            refresh(1.0),
            (2.0, event(SoundKind::Music, Some("B"), 2.0)),
        ];
        pump(events.iter());
        assert_eq!(forwarded_count(), 2, "one sound + one music forwarded");
        let queued = |n: usize| {
            let p = PENDING.lock().expect("lock");
            assert_eq!(p.queue.len(), n);
        };

        pump(events.iter());
        assert_eq!(forwarded_count(), 2);
        queued(2);

        events.push((3.0, event(SoundKind::Rolloff, Some("C"), 3.0)));
        pump(events.iter());
        assert_eq!(forwarded_count(), 3);
        queued(3);

        events.remove(0);
        events.remove(0);
        pump(events.iter());
        assert_eq!(forwarded_count(), 3);
        reset_queue();
    }
}
