//! Typed outbound presentation events emitted by script natives.
//!
//! The VM is headless: it has no audio device, no renderer and no projector. Natives that in
//! the retail engine drive those subsystems (sound/music playback, texture replacement, the
//! display refresh, projector attachment) do **not** silently succeed here: they append a
//! [`PresentationEvent`] carrying the actor, the decoded arguments and the VM time, and return
//! the value their decoded declaration returns (all of these return `void` in XIII). The host
//! (Bevy runtime, a test, the `xiii-tool --events` harness) calls [`crate::vm::Vm::drain_events`]
//! to consume them.
//!
//! Evidence conventions:
//!
//! - `Actor.PlaySound`/`Actor.PlayMusic` are decoded in `engine.u` as
//!   `object<Sound> Sound, int Param1, int Param2, int Param3, int Param4, int Param5` with no
//!   return value. The retail parameter names are generic; the semantic labels used here
//!   (`slot`, `volume`, `radius`, `pitch`) follow the task spec's UT2003/2004-style reading and
//!   are a **hypothesis**, not decoded evidence. The raw decoded values are preserved exactly,
//!   and an omitted optional argument is `None` (never a zero standing in for "absent").
//! - `Projector.AttachProjector`/`DetachProjector`/`AbandonProjector` have no decoded return.
//! - `ReplaceATextureByAnOther` carries the two decoded `Texture` object paths.

/// A sound/music playback request decoded from `Actor.PlaySound` or `Actor.PlayMusic`.
///
/// The five trailing `int`s are the decoded `Param1..Param5`; `None` means the caller omitted
/// that optional argument (UE2 `EX_Nothing`), so a present `0` is distinct from "absent".
#[derive(Debug, Clone, PartialEq)]
pub struct SoundEvent {
    /// Object the native ran on (display name).
    pub actor: String,
    /// Decoded `Sound` object path (`Package.Object`), `None` for a null argument.
    pub sound: Option<String>,
    /// Decoded `RollOffActor` (only `Actor.PlayRolloffSound`; `None` otherwise).
    pub rolloff_actor: Option<String>,
    /// `Param1` (semantic name `slot` — hypothesis).
    pub slot: Option<i32>,
    /// `Param2` (semantic name `volume` — hypothesis).
    pub volume: Option<i32>,
    /// `Param3` (semantic name `radius` — hypothesis).
    pub radius: Option<i32>,
    /// `Param4` (semantic name `pitch` — hypothesis).
    pub pitch: Option<i32>,
    /// `Param5` (no semantic name decoded).
    pub param5: Option<i32>,
    /// VM time in seconds when the native ran.
    pub time: f64,
}

/// A dialogue line decoded from `Actor.PlayStrVoice` (native 354).
///
/// `DialogueManager.Speak` (`xidcine.u`) builds `SoundName` as
/// `Level.Title + "_" + PawnName + "_" + zero-padded SentenceIndex` and calls
/// `PlayStrVoice(SoundName, SpeakingSpeaker.Pawn)` (native 354), with the subtitle text taken from
/// the speaker's `Sentences[SentenceIndex]` map property. The native records both so the host can
/// play the wave and draw the subtitle.
#[derive(Debug, Clone, PartialEq)]
pub struct DialogueEvent {
    /// Object the native ran on (the `DialogueManager`).
    pub actor: String,
    /// The `RollOffActor` argument (the speaking pawn), `None` when null.
    pub speaker: Option<String>,
    /// The decoded `SoundName` string the script built (the audio path/key, e.g.
    /// `Plage00_XIIIa_00`); never a `Sound` object path.
    pub sound: String,
    /// Subtitle text read from the emitting `DialogueManager`'s current
    /// `Speakers[..].Sentences[..].Sentences` member; `None` when the actor is not a
    /// `DialogueManager` or the line cannot be read.
    pub text: Option<String>,
    /// Duration in seconds from the host voice provider, `None` when unavailable.
    pub duration: Option<f32>,
    /// VM time in seconds when the native ran.
    pub time: f64,
}

/// One outbound presentation command.
#[derive(Debug, Clone, PartialEq)]
pub enum PresentationEvent {
    /// `Actor.PlaySound` (native 264).
    PlaySound(SoundEvent),
    /// `Actor.PlayMusic` (native 358).
    PlayMusic(SoundEvent),
    /// `Actor.PlayRolloffSound` (native 350): a positional sound with a roll-off actor.
    PlayRolloffSound(SoundEvent),
    /// `Actor.ReplaceATextureByAnOther`: swap `source` for `destination` in the display.
    ReplaceTexture {
        /// Object the native ran on.
        actor: String,
        /// Decoded `SrcTexture` path.
        source: Option<String>,
        /// Decoded `DestTexture` path.
        destination: Option<String>,
        /// VM time.
        time: f64,
    },
    /// `Actor.RefreshDisplaying`: the display was asked to refresh.
    RefreshDisplaying {
        /// Object the native ran on.
        actor: String,
        /// VM time.
        time: f64,
    },
    /// `LevelInfo.SetInjuredEffect`: the injured-screen effect changed.
    SetInjuredEffect {
        /// Object the native ran on.
        actor: String,
        /// Decoded `NewState`.
        new_state: bool,
        /// Decoded `Delay`.
        delay: f32,
        /// VM time.
        time: f64,
    },
    /// `Projector.AttachProjector`: attach the projector to its owner's mesh.
    ProjectorAttach {
        /// Object the native ran on.
        actor: String,
        /// VM time.
        time: f64,
    },
    /// `Projector.DetachProjector`: detach the projector.
    ProjectorDetach {
        /// Object the native ran on.
        actor: String,
        /// Decoded `Force`.
        force: bool,
        /// VM time.
        time: f64,
    },
    /// `Projector.AbandonProjector`: let the projector expire after `lifetime`.
    ProjectorAbandon {
        /// Object the native ran on.
        actor: String,
        /// Decoded `Lifetime`.
        lifetime: f32,
        /// VM time.
        time: f64,
    },
    /// `Actor.StopVoice`: stop the actor's current voice/dialogue playback.
    StopVoice {
        /// Object the native ran on.
        actor: String,
        /// VM time.
        time: f64,
    },
    /// `Actor.StopSound`: stop one sound on the actor (`sound` is the decoded object path).
    StopSound {
        /// Object the native ran on.
        actor: String,
        /// Decoded `Sound` path, `None` for a null argument.
        sound: Option<String>,
        /// VM time.
        time: f64,
    },
    /// `Actor.PlaySndPNJOno`: play an onomatopoeia sound (`SndOno`) with a mesh code and timbre.
    PlaySndPNJOno {
        /// Object the native ran on.
        actor: String,
        /// Decoded `SndOno` path, `None` for a null argument.
        sound: Option<String>,
        /// Decoded `CodeMesh`.
        code_mesh: i32,
        /// Decoded `Timbre`.
        timbre: i32,
        /// VM time.
        time: f64,
    },
    /// `Actor.PlayStrVoice`: a named dialogue voice with its subtitle (see [`DialogueEvent`]).
    Dialogue(DialogueEvent),
}

impl PresentationEvent {
    /// The actor (display name) the native ran on.
    pub fn actor(&self) -> &str {
        match self {
            Self::PlaySound(e) | Self::PlayMusic(e) | Self::PlayRolloffSound(e) => &e.actor,
            Self::ReplaceTexture { actor, .. }
            | Self::RefreshDisplaying { actor, .. }
            | Self::SetInjuredEffect { actor, .. }
            | Self::ProjectorAttach { actor, .. }
            | Self::ProjectorDetach { actor, .. }
            | Self::ProjectorAbandon { actor, .. }
            | Self::StopVoice { actor, .. }
            | Self::StopSound { actor, .. }
            | Self::PlaySndPNJOno { actor, .. } => actor,
            Self::Dialogue(e) => &e.actor,
        }
    }

    /// VM time in seconds.
    pub fn time(&self) -> f64 {
        match self {
            Self::PlaySound(e) | Self::PlayMusic(e) | Self::PlayRolloffSound(e) => e.time,
            Self::ReplaceTexture { time, .. }
            | Self::RefreshDisplaying { time, .. }
            | Self::SetInjuredEffect { time, .. }
            | Self::ProjectorAttach { time, .. }
            | Self::ProjectorDetach { time, .. }
            | Self::ProjectorAbandon { time, .. }
            | Self::StopVoice { time, .. }
            | Self::StopSound { time, .. }
            | Self::PlaySndPNJOno { time, .. } => *time,
            Self::Dialogue(e) => e.time,
        }
    }
}

impl std::fmt::Display for PresentationEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let opt = |v: Option<i32>| v.map_or_else(|| "-".to_owned(), |x| x.to_string());
        let path = |v: &Option<String>| v.clone().unwrap_or_else(|| "None".to_owned());
        match self {
            Self::PlaySound(e) => write!(
                f,
                "PlaySound {} sound={} slot={} volume={} radius={} pitch={} param5={}",
                e.actor,
                path(&e.sound),
                opt(e.slot),
                opt(e.volume),
                opt(e.radius),
                opt(e.pitch),
                opt(e.param5)
            ),
            Self::PlayMusic(e) => write!(
                f,
                "PlayMusic {} sound={} slot={} volume={} radius={} pitch={} param5={}",
                e.actor,
                path(&e.sound),
                opt(e.slot),
                opt(e.volume),
                opt(e.radius),
                opt(e.pitch),
                opt(e.param5)
            ),
            Self::PlayRolloffSound(e) => write!(
                f,
                "PlayRolloffSound {} sound={} rolloff={} slot={} volume={} radius={} pitch={} param5={}",
                e.actor,
                path(&e.sound),
                path(&e.rolloff_actor),
                opt(e.slot),
                opt(e.volume),
                opt(e.radius),
                opt(e.pitch),
                opt(e.param5)
            ),
            Self::ReplaceTexture {
                actor,
                source,
                destination,
                ..
            } => write!(
                f,
                "ReplaceATextureByAnOther {actor} {} -> {}",
                path(source),
                path(destination)
            ),
            Self::RefreshDisplaying { actor, .. } => write!(f, "RefreshDisplaying {actor}"),
            Self::SetInjuredEffect {
                actor,
                new_state,
                delay,
                ..
            } => write!(
                f,
                "SetInjuredEffect {actor} new_state={new_state} delay={delay:?}"
            ),
            Self::ProjectorAttach { actor, .. } => write!(f, "AttachProjector {actor}"),
            Self::ProjectorDetach { actor, force, .. } => {
                write!(f, "DetachProjector {actor} force={force}")
            }
            Self::ProjectorAbandon {
                actor, lifetime, ..
            } => write!(f, "AbandonProjector {actor} lifetime={lifetime:?}"),
            Self::StopVoice { actor, .. } => write!(f, "StopVoice {actor}"),
            Self::StopSound { actor, sound, .. } => {
                write!(f, "StopSound {actor} sound={}", path(sound))
            }
            Self::PlaySndPNJOno {
                actor,
                sound,
                code_mesh,
                timbre,
                ..
            } => write!(
                f,
                "PlaySndPNJOno {actor} sound={} codeMesh={code_mesh} timbre={timbre}",
                path(sound)
            ),
            Self::Dialogue(e) => write!(
                f,
                "Dialogue {} speaker={} sound={} duration={} text={}",
                e.actor,
                e.speaker.as_deref().unwrap_or("-"),
                e.sound,
                e.duration
                    .map_or_else(|| "-".to_owned(), |d| format!("{d:.3}")),
                e.text.as_deref().unwrap_or("<none>")
            ),
        }
    }
}
