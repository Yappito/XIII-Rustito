//! VM sound/music playback for `--play` (Bevy audio).
//!
//! The script VM is headless and emits [`PresentationEvent`]s; this plugin is the presentation
//! side that turns `PlaySound`/`PlayMusic`/`PlayRolloffSound` events into Bevy audio:
//!
//! * [`xiii_audio::SoundLibrary`] resolves a `Sound` object's leaf name to an HX bank entry and
//!   decodes it to PCM, which is wrapped as an in-memory WAV [`AudioSource`];
//! * `PlaySound`/`PlayRolloffSound` are played **spatially** when the emitting actor's current
//!   position can be read from the render entities, with the [`SpatialListener`] attached to the
//!   player camera; otherwise they fall back to non-spatial playback (counted, never silent);
//! * `PlayMusic` is played on a **non-spatial** channel with stop/replace semantics (a new track
//!   stops the previous one).
//!
//! The play loop pushes each newly drained event into a process-global queue with one call
//! ([`pump`] from `play::fixed_step`); this module owns the queue and the plugin that consumes it,
//! so the only change to `play/` is that hook line.
//!
//! Semantic mapping of the decoded `Param1..Param5` (slot/volume/radius/pitch) is a
//! **hypothesis** (see `xiii-script`'s `events.rs`); the raw values are preserved. Bevy's spatial
//! audio has no distance attenuation, so the decoded radius cannot be applied (counted as
//! unsupported). Absence of an audio device is logged and never fatal (Bevy drops the queued
//! sinks); such entities are expired and counted so an unattended run continues.

use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bevy::audio::Volume;
use bevy::prelude::*;

use xiii_audio::{PcmAudio, ResolveFailure, SoundLibrary};
use xiii_script::PresentationEvent;

use crate::cli::Options;

/// Maximum number of unresolved/queued requests kept for the overlay.
const OVERLAY_HISTORY: usize = 8;
/// Audio entities that never receive a sink (no audio device) are dropped after this long.
const PENDING_TIMEOUT: Duration = Duration::from_secs(5);
/// Ear gap of the listener (metres). Hypothesis: a human head is ~0.2 m wide.
const EAR_GAP_M: f32 = 0.2;

/// Which native emitted a request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SoundKind {
    /// `Actor.PlaySound`.
    Sound,
    /// `Actor.PlayMusic`.
    Music,
    /// `Actor.PlayRolloffSound`.
    Rolloff,
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
    /// Radius parameters that Bevy spatial audio cannot apply.
    pub radius_unsupported: usize,
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
    /// Bounded history for the overlay.
    pub last: Vec<PlayedEntry>,
}

impl AudioStats {
    fn record(&mut self, time: f64, label: String, outcome: &str) {
        self.record_res(time, label, outcome, String::new());
    }

    fn record_res(&mut self, time: f64, label: String, outcome: &str, resolution: String) {
        if outcome != "played" && outcome != "music" {
            *self.failed.entry(reason_label(outcome)).or_default() += 1;
        }
        self.last.push(PlayedEntry {
            time,
            label,
            outcome: outcome.to_owned(),
            resolution,
        });
        if self.last.len() > OVERLAY_HISTORY {
            self.last.remove(0);
        }
    }
}

/// `&'static str` label for a failure outcome (already one of the known reasons).
fn reason_label(s: &str) -> &'static str {
    match s {
        "no_sound_name" => "no_sound_name",
        "no_name_match" => "no_name_match",
        "decode_failed" => "decode_failed",
        "stream_missing" => "stream_missing",
        "audio_off" => "audio_off",
        "no_position" => "no_position",
        _ => "other",
    }
}

/// Resource owning the resolver library and the stats.
#[derive(Resource)]
pub struct AudioRes {
    /// Resolver library (empty when `--audio off`).
    pub library: SoundLibrary,
    /// Playback statistics.
    pub stats: AudioStats,
}

/// Marker for the plugin's own overlay text (separate from the play overlay).
#[derive(Component)]
struct AudioOverlay;

/// Marker for a music player entity (stop/replace semantics).
#[derive(Component)]
struct MusicTrack;

/// Marker for a spawned sound entity with its spawn time, so a missing audio device can be
/// detected and the entity expired.
#[derive(Component)]
struct SpawnedSound {
    at: Instant,
}

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
        .add_systems(Startup, setup_audio)
        .add_systems(
            Update,
            (
                ensure_listener,
                consume_requests,
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
        "[audio] exit: enabled={} requests={} played={} music={} spatial={} fallback={} radius_unsupported={} no_device_expired={} failures=[{}]",
        s.enabled,
        s.requests,
        s.played,
        s.music,
        s.spatial,
        s.spatial_fallback,
        s.radius_unsupported,
        s.no_device_expired,
        failures.join(", ")
    );
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
            library: SoundLibrary::empty(),
            stats,
        });
        println!("[audio] disabled (--audio off): VM sound events are counted, not played");
        return;
    }
    let Some(dir) = cfg.options.game_dir.clone() else {
        commands.insert_resource(AudioRes {
            library: SoundLibrary::empty(),
            stats,
        });
        println!("[audio] no --game-dir; sound playback disabled");
        return;
    };
    let started = Instant::now();
    let library = SoundLibrary::scan(&dir);
    let s = library.stats();
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
fn consume_requests(
    mut commands: Commands,
    audio: Option<ResMut<AudioRes>>,
    mut sources: ResMut<Assets<AudioSource>>,
    names: Query<(&Name, &GlobalTransform)>,
    music: Query<Entity, With<MusicTrack>>,
) {
    let Some(mut audio) = audio else {
        return;
    };
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
            audio.stats.record(req.time, label(&req), "audio_off");
            continue;
        }
        match req.kind {
            SoundKind::Music => play_music(&mut commands, &mut audio, &mut sources, &req, &music),
            SoundKind::Sound | SoundKind::Rolloff => {
                play_sound(&mut commands, &mut audio, &mut sources, &req, &positions)
            }
        }
    }
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

fn label(req: &SoundRequest) -> String {
    let kind = match req.kind {
        SoundKind::Sound => "sound",
        SoundKind::Music => "music",
        SoundKind::Rolloff => "rolloff",
    };
    format!(
        "{kind} {} {}",
        req.actor,
        req.sound.as_deref().unwrap_or("<no path>")
    )
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
    let Some(r) = audio.library.resolve_path(path) else {
        return Err(ResolveFailure::NoNameMatch);
    };
    let pcm = audio.library.load(&r.entry)?;
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

fn play_sound(
    commands: &mut Commands,
    audio: &mut AudioRes,
    sources: &mut Assets<AudioSource>,
    req: &SoundRequest,
    positions: &impl Fn(&str) -> Option<Vec3>,
) {
    let (handle, resolution) = match source_for(audio, sources, req) {
        Ok(h) => h,
        Err(reason) => {
            audio.stats.record(req.time, label(req), reason.as_str());
            return;
        }
    };
    if req.radius.is_some() {
        audio.stats.radius_unsupported += 1;
    }
    let pos = positions(&req.actor);
    let mut settings = PlaybackSettings::DESPAWN
        .with_volume(volume_of(req))
        .with_speed(speed_of(req));
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
        .record_res(req.time, label(req), "played", resolution);
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
            audio.stats.record(req.time, label(req), reason.as_str());
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
            .with_volume(volume_of(req))
            .with_speed(speed_of(req)),
    ));
    audio.stats.music += 1;
    audio.stats.played += 1;
    audio
        .stats
        .record_res(req.time, label(req), "music", resolution);
}

/// Drops entities that never received a sink (no audio device) after [`PENDING_TIMEOUT`].
fn expire_without_device(
    mut commands: Commands,
    mut audio: Option<ResMut<AudioRes>>,
    q: Query<(Entity, &SpawnedSound), Without<AudioSink>>,
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
    audio: Option<Res<AudioRes>>,
    mut text: Query<&mut Text, With<AudioOverlay>>,
    mut done: Local<bool>,
    mut commands: Commands,
) {
    let Some(audio) = audio else {
        return;
    };
    if !*done {
        commands.spawn((
            AudioOverlay,
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
        *done = true;
        return;
    }
    let Ok(mut text) = text.single_mut() else {
        return;
    };
    let s = &audio.stats;
    let mut out = format!(
        "audio {} | requests {} played {} music {} spatial {} (fallback {}) failed {} | no-device expired {}\n",
        if s.enabled { "on" } else { "off" },
        s.requests,
        s.played,
        s.music,
        s.spatial,
        s.spatial_fallback,
        s.failed.values().sum::<usize>(),
        s.no_device_expired,
    );
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
        // Present positive volume 200 -> ~0.784; absent -> 1.0.
        assert!((volume_of(&r).to_linear() - 200.0 / 255.0).abs() < 1e-4);
        r.volume = None;
        assert_eq!(volume_of(&r).to_linear(), 1.0);
        // Present positive pitch 100 -> 1.0; absent -> 1.0.
        assert_eq!(speed_of(&r), 1.0);
        r.pitch = Some(200);
        assert!((speed_of(&r) - 2.0).abs() < 1e-4);
        r.pitch = Some(0);
        assert_eq!(speed_of(&r), 1.0);
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
        use xiii_world::runtime;

        // ---- production session: does a real 30 s Plage00 run emit sound events? ------------
        // Uses exactly `play::session::Session` (the runtime's VM bridge), which does not load
        // `.uax` sound packages today. This is the acceptance-relevant measurement.
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
            let prod_with_path = session
                .events
                .iter()
                .filter(|(_, ev)| match ev {
                    PresentationEvent::PlaySound(e)
                    | PresentationEvent::PlayMusic(e)
                    | PresentationEvent::PlayRolloffSound(e) => e.sound.is_some(),
                    _ => false,
                })
                .count();
            // Resolve each production event too, so the report has resolved/unresolved by reason.
            let mut prod_resolved = 0usize;
            let mut prod_failed: HashMap<&'static str, usize> = HashMap::new();
            for (_, ev) in session.events.iter() {
                if !matches!(
                    ev,
                    PresentationEvent::PlaySound(_)
                        | PresentationEvent::PlayMusic(_)
                        | PresentationEvent::PlayRolloffSound(_)
                ) {
                    continue;
                }
                let req = SoundRequest::from_event(ev, SoundKind::Sound);
                match req.sound.as_deref() {
                    None => *prod_failed.entry("no_sound_name").or_default() += 1,
                    Some(p) => match lib.resolve_path(p) {
                        Some(r) => match lib.load(&r.entry) {
                            Ok(_) => prod_resolved += 1,
                            Err(e) => *prod_failed.entry(e.as_str()).or_default() += 1,
                        },
                        None => {
                            *prod_failed
                                .entry(ResolveFailure::NoNameMatch.as_str())
                                .or_default() += 1
                        }
                    },
                }
            }
            println!(
                "[audio test] production Plage00 resolution: {prod_resolved} resolved, \
                 unresolved by reason {prod_failed:?}"
            );
            println!(
                "[audio test] production Plage00 session (30 s): {prod_sound} sound events emitted \
                 ({prod_with_path} with a path); first script error: {}",
                session
                    .first_error()
                    .map(|e| e.lines().next().unwrap_or(""))
                    .unwrap_or("-")
            );
            for (t, ev) in session.events.iter() {
                if matches!(
                    ev,
                    PresentationEvent::PlaySound(_)
                        | PresentationEvent::PlayMusic(_)
                        | PresentationEvent::PlayRolloffSound(_)
                ) {
                    println!("[audio test]   prod [{t:.3}s] {ev}");
                }
            }
            assert!(
                prod_sound > 0,
                "the production Plage00 session emitted no sound events at all"
            );
        }

        // ---- with `.uax` loaded: does the Plage00 lifecycle emit sound events with paths? -----
        // The production runtime does not load `.uax` (out of this task's ownership); this
        // demonstrates the resolver+decoder works on the real Plage00 events once it does.
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
        let mut without_path = 0usize;
        let mut resolved = 0usize;
        let mut failed: HashMap<&'static str, usize> = HashMap::new();
        let mut sample: Vec<String> = Vec::new();
        for _ in 0..600 {
            // Tolerant tick, exactly as `play::session::Session::step` does: an actor with an
            // unimplemented native is suspended, and the remaining actors keep running.
            let _ = vm.tick_suspending(0.05);
            for ev in vm.drain_events() {
                let is_sound = matches!(
                    ev,
                    PresentationEvent::PlaySound(_)
                        | PresentationEvent::PlayMusic(_)
                        | PresentationEvent::PlayRolloffSound(_)
                );
                if !is_sound {
                    continue;
                }
                let req = SoundRequest::from_event(&ev, SoundKind::Sound);
                match &req.sound {
                    None => {
                        without_path += 1;
                        if sample.len() < 8 {
                            sample.push(format!("{} <null path> ({})", req.actor, ev));
                        }
                    }
                    Some(path) => {
                        with_path += 1;
                        if sample.len() < 8 {
                            sample.push(format!("{} {}", req.actor, path));
                        }
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
        }
        println!(
            "[audio test] Plage00 with .uax loaded (30 s): {} sound events with a path, {} without; \
             {resolved} resolved/decoded, failures {failed:?}",
            with_path, without_path
        );
        for s in &sample {
            println!("[audio test]   sample: {s}");
        }

        // ---- forced TriggerSound0 event (map property Sound'XIIISound.Interface.EndBig') --------
        let ts = vm
            .find_object("TriggerSound0")
            .expect("Plage00 has TriggerSound0");
        let pawn = vm
            .spawn(
                runtime::find_class(set, "xiii", "XIIIPlayerPawn").expect("player class"),
                "XIIIPlayerPawn(item6b)",
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
        println!(
            "[audio test] forced TriggerSound0: {forced} event(s), {forced_resolved} resolved"
        );

        // The `.uax`-loaded lifecycle emits the same null-path mover events (their class
        // defaults are `Object(None)`), so a path-bearing event only appears when an actor
        // actually calls a sound native with a real Sound. If any do fire, every one must
        // resolve; the forced real TriggerSound0 event is the guaranteed case.
        assert_eq!(
            resolved, with_path,
            "every emitted sound event with a path must resolve: failures {failed:?}"
        );
        assert!(forced > 0, "TriggerSound0.Trigger emitted no sound event");
        assert_eq!(
            forced_resolved, forced,
            "the real TriggerSound0 event must resolve through the HX banks"
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

        // Re-pumping the same window forwards nothing.
        pump(events.iter());
        assert_eq!(forwarded_count(), 2);
        queued(2);

        // A new event at a later time is forwarded; older events are not replayed.
        events.push((3.0, event(SoundKind::Rolloff, Some("C"), 3.0)));
        pump(events.iter());
        assert_eq!(forwarded_count(), 3);
        queued(3);

        // Trimming the front (the retained deque drops old entries) must not cause a replay.
        events.remove(0);
        events.remove(0);
        pump(events.iter());
        assert_eq!(forwarded_count(), 3);
        reset_queue();
    }
}
