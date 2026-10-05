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
