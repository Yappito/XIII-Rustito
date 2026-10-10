//! Headless voice-duration provider for the dialogue natives.
//!
//! `Actor.GetWaveDuration(string SoundName)` and `Actor.PlayStrVoice(string SoundName, object<Actor>
//! RollOffActor)` need the length of a decoded dialogue wave to build the subtitle lifetime. The VM
//! is filesystem-free, so the host installs a provider (the Bevy runtime wraps `xiii-audio`'s
//! `SoundLibrary`); without one the duration is unknown and the native reports `0` with a visible
//! note (never a silently invented value).
//!
//! The trait is deliberately small: a sound name (the string the script builds, e.g.
//! `Plage00_XIIIa_00`) maps to its length in seconds.

/// Resolves a script `SoundName` to a decoded wave duration in seconds.
pub trait VoiceDuration {
    /// Duration of `sound_name` in seconds, or `None` when the name is unknown/undecoded.
    fn duration(&self, sound_name: &str) -> Option<f32>;
}

/// A provider with a fixed answer for every name (unit tests and synthetic corpora).
#[derive(Debug, Clone, Copy)]
pub struct FixedVoiceDuration {
    seconds: f32,
}

impl FixedVoiceDuration {
    /// Provider returning `seconds` for every query.
    pub fn new(seconds: f32) -> Self {
        Self { seconds }
    }
}

impl VoiceDuration for FixedVoiceDuration {
    fn duration(&self, _sound_name: &str) -> Option<f32> {
        Some(self.seconds)
    }
}

/// Resolves a script `SoundName` to the engine's `Actor.WaveHasPosition` answer (item48).
///
/// The decoded engine native (Engine.dll 0x103e3580) returns false when no audio subsystem is
/// installed, otherwise queries `UHXAUDIOSubsystem::WaveHasPosition` (HXAudio.dll 0x100226b0):
/// a name-keyed map lookup at `subsystem+0x88`; a hit answers bit 3 of the loaded entry's flag
/// byte at +0x54. That flag byte is assembled from per-entry state pointers (HXAudio
/// 0x100173f6-0x10017450, measured): bit 1 = pointer +0x30 non-null, bit 2 = +0x38 non-null,
/// bit 3 = +0x34 non-null — so `WaveHasPosition` is "the named resource's loaded state has a
/// position object". Which `.hxc`/`.uax` byte (if any) feeds that state pointer at load time is
/// not decoded, so the provider — not the VM — owns the classification. `None` means the
/// provider cannot classify the name (the native then reports `false` with a visible note,
/// mirroring the engine's own no-subsystem answer).
pub trait WavePosition {
    /// Whether the named wave is positional (3D), or `None` when unknown/unresolvable.
    fn has_position(&self, sound_name: &str) -> Option<bool>;
}

/// A provider with a fixed answer for known names (unit tests and synthetic corpora).
#[derive(Debug, Clone)]
pub struct FixedWavePosition {
    answer: bool,
}

impl FixedWavePosition {
    /// Provider returning `answer` for every query it can answer.
    pub fn new(answer: bool) -> Self {
        Self { answer }
    }
}

impl WavePosition for FixedWavePosition {
    fn has_position(&self, _sound_name: &str) -> Option<bool> {
        Some(self.answer)
    }
}
