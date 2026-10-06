//! Host `VoiceDuration` provider for the dialogue natives, backed by the decoded HX library.
//!
//! `Actor.GetWaveDuration`/`Actor.PlayStrVoice` need the length of a decoded dialogue wave. The
//! VM is filesystem-free, so the app installs [`LibraryVoiceDuration`] over the same
//! [`xiii_audio::SoundLibrary`] the audio layer already scanned ([`crate::audio::AudioRes`]).
//! `PlayStrVoice` then takes the engine's voice-completion path (`EndOfVoice` emulated by the
//! actor's `Timer`) instead of the script's `NoSound` fallback.
//!
//! A name the library cannot resolve (or a wave it cannot decode) returns `None` and increments
//! the shared counter, so `PlayStrVoice` keeps returning `false` for that line only and the host
//! can report the unresolved names.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use xiii_audio::SoundLibrary;
use xiii_script::VoiceDuration;

/// Resolves a script voice name to its decoded wave length via the shared [`SoundLibrary`].
pub struct LibraryVoiceDuration {
    library: Arc<Mutex<SoundLibrary>>,
    unresolved: Arc<AtomicU64>,
}

impl LibraryVoiceDuration {
    /// Wraps `library`; the returned counter is shared with the host so it can report unresolved
    /// voice names (each `None` answer increments it).
    pub fn new(library: Arc<Mutex<SoundLibrary>>) -> (Self, Arc<AtomicU64>) {
        let unresolved = Arc::new(AtomicU64::new(0));
        (
            Self {
                library,
                unresolved: unresolved.clone(),
            },
            unresolved,
        )
    }

    fn count_unresolved(&self) {
        self.unresolved.fetch_add(1, Ordering::Relaxed);
    }
}

impl VoiceDuration for LibraryVoiceDuration {
    fn duration(&self, sound_name: &str) -> Option<f32> {
        let library = self.library.clone();
        let mut library = library.lock().unwrap_or_else(|e| e.into_inner());
        let Some(resolved) = library.resolve_path(sound_name) else {
            self.count_unresolved();
            return None;
        };
        let pcm = match library.load(&resolved.entry) {
            Ok(pcm) => pcm,
            Err(_) => {
                self.count_unresolved();
                return None;
            }
        };
        let frames = pcm.samples.len() as f32 / f32::from(pcm.channels.max(1));
        (pcm.sample_rate > 0).then(|| frames / pcm.sample_rate as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::LibraryVoiceDuration;
    use std::sync::{Arc, Mutex};
    use xiii_audio::SoundLibrary;
    use xiii_script::VoiceDuration;

    /// An empty library resolves nothing: every query is `None` and counted, never a guessed
    /// length. This is the failure mode the native must see as "voice did not start".
    #[test]
    fn empty_library_returns_none_and_counts() {
        let (provider, unresolved) =
            LibraryVoiceDuration::new(Arc::new(Mutex::new(SoundLibrary::empty())));
        assert_eq!(provider.duration("Plage00_Pam_00"), None);
        assert_eq!(provider.duration("anything"), None);
        assert_eq!(unresolved.load(std::sync::atomic::Ordering::Relaxed), 2);
    }
}
