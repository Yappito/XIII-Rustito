//! Audio path check: a sine tone synthesised into an in-memory WAV and played
//! through Bevy's regular `AudioSource` (rodio decoder) path. No files needed.

use std::f32::consts::TAU;

use bevy::prelude::*;

pub struct TonePlugin;

impl Plugin for TonePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ToneStats>()
            .add_message::<PlayTone>()
            .add_systems(Startup, create_tone)
            .add_systems(
                Update,
                (request_tone_on_space, play_tone, detect_playback).chain(),
            );
    }
}

/// Request to play the diagnostic tone once.
#[derive(Message, Default, Clone, Copy)]
pub struct PlayTone;

#[derive(Resource, Default, Debug)]
pub struct ToneStats {
    /// Number of tone entities spawned.
    pub requested: u32,
    /// Number of times Bevy attached an `AudioSink` (playback actually started).
    pub started: u32,
}

#[derive(Resource)]
struct ToneHandle(Handle<AudioSource>);

#[derive(Component)]
struct ToneMarker;

const SAMPLE_RATE: u32 = 44_100;

fn create_tone(mut commands: Commands, mut sources: ResMut<Assets<AudioSource>>) {
    let wav = sine_wav(440.0, 0.35, SAMPLE_RATE, 0.4);
    let handle = sources.add(AudioSource { bytes: wav.into() });
    commands.insert_resource(ToneHandle(handle));
}

fn request_tone_on_space(keys: Res<ButtonInput<KeyCode>>, mut out: MessageWriter<PlayTone>) {
    if keys.just_pressed(KeyCode::Space) {
        out.write(PlayTone);
    }
}

fn play_tone(
    mut commands: Commands,
    mut requests: MessageReader<PlayTone>,
    tone: Res<ToneHandle>,
    mut stats: ResMut<ToneStats>,
) {
    for _ in requests.read() {
        stats.requested += 1;
        commands.spawn((
            Name::new("diagnostic tone"),
            ToneMarker,
            AudioPlayer::new(tone.0.clone()),
            PlaybackSettings::DESPAWN,
        ));
    }
}

fn detect_playback(
    started: Query<(), (With<ToneMarker>, Added<AudioSink>)>,
    mut stats: ResMut<ToneStats>,
) {
    let n = started.iter().count() as u32;
    if n > 0 {
        stats.started += n;
        info!(
            "audio: tone playback started (sink attached), total {}",
            stats.started
        );
    }
}

/// Builds a mono 16-bit PCM RIFF/WAVE file containing a sine tone with a short
/// linear fade in/out to avoid clicks.
pub fn sine_wav(freq_hz: f32, seconds: f32, sample_rate: u32, amplitude: f32) -> Vec<u8> {
    let n = (seconds * sample_rate as f32).round() as u32;
    let fade = (sample_rate / 100).max(1); // 10 ms
    let data_len = n * 2;
    let mut out = Vec::with_capacity(44 + data_len as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // fmt chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    for i in 0..n {
        let env = (i.min(n - 1 - i) as f32 / fade as f32).min(1.0);
        let s = (TAU * freq_hz * i as f32 / sample_rate as f32).sin() * amplitude * env;
        out.extend_from_slice(&((s * i16::MAX as f32) as i16).to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_consistent() {
        let w = sine_wav(440.0, 0.1, 8000, 0.5);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(&w[8..16], b"WAVEfmt ");
        let riff_len = u32::from_le_bytes(w[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff_len + 8, w.len());
        let data_len = u32::from_le_bytes(w[40..44].try_into().unwrap()) as usize;
        assert_eq!(data_len, 800 * 2);
        assert_eq!(w.len(), 44 + data_len);
    }
}
