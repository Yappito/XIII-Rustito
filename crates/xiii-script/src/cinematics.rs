//! Cinematic/dialogue natives (`xidcine`, `engine.u`).
//!
//! These are the natives on the cutscene-dialogue path the decoded scripts actually call. The
//! `DialogueManager` state machine (`xidcine.u`) computes a voice name from
//! `Level.Title + "_" + PawnName + "_" + zero-padded SentenceIndex` and:
//!
//! 1. calls `Actor.GetWaveDuration(SoundName)` (native 357) to size the on-screen lifetime, then
//! 2. calls `Actor.PlayStrVoice(SoundName, SpeakingSpeaker.Pawn)` (native 354) to speak the line.
//!
//! The headless VM cannot decode audio, so `PlayStrVoice` emits a typed
//! [`crate::events::PresentationEvent::Dialogue`] carrying the emitter, the speaking pawn, the
//! voice name, the subtitle text read from the `DialogueManager`'s own `Lines`/`Speakers` map
//! data, and the duration from the host [`crate::voice::VoiceDuration`] provider. The engine's
//! `Actor.PlayVoice` (native 353, a `Sound` object rather than a name) is registered as a sound
//! event so a caller of that form is not silently dropped either.
//!
//! Every entry is kept in this one block so a parallel registry edit can merge without touching
//! it; `registry::builtin_defs` extends the built-in table with [`cinematic_defs`].

use crate::registry::{NativeCtx, NativeDef, NativeFn, NativeOutcome, NativeStatus};
use crate::value::{ObjectId, Value};
use crate::vm::{TraceKind, Vm, VmErrorKind, VmResult};

fn def(
    path: &'static str,
    signature: &'static str,
    evidence: &'static str,
    f: NativeFn,
) -> NativeDef {
    NativeDef {
        path,
        signature,
        evidence,
        status: NativeStatus::Implemented,
        short_circuit: None,
        f,
    }
}

fn partial(
    status: &'static str,
    path: &'static str,
    signature: &'static str,
    evidence: &'static str,
    f: NativeFn,
) -> NativeDef {
    NativeDef {
        status: NativeStatus::Partial(status),
        ..def(path, signature, evidence, f)
    }
}

fn val(v: Value) -> VmResult<NativeOutcome> {
    Ok(NativeOutcome::Value(v))
}

fn string(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<String> {
    match a.get(i) {
        Some(Value::Str(v)) | Some(Value::Name(v)) => Ok(v.clone()),
        Some(v) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "string",
            found: v.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

/// `Actor.GetWaveDuration(string SoundName) -> float` (native 357).
///
/// The decoded call site is `WaveLength = GetWaveDuration(SoundName) + 1.0`, and a `0` result
/// makes the script use its own 3-second fallback, so an unknown duration is harmless. Without a
/// host provider the call returns `0` and records a visible note (never a guessed length).
fn get_wave_duration(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = string(vm, a, 0)?;
    match vm.voice_duration(&name) {
        Some(d) => val(Value::Float(d)),
        None => {
            vm.note(TraceKind::Note(format!(
                "GetWaveDuration({name:?}): no voice-duration provider, reported 0"
            )));
            val(Value::Float(0.0))
        }
    }
}

/// `Actor.PlayStrVoice(string SoundName, object<Actor> RollOffActor) -> bool` (native 354).
///
/// Emits a [`crate::events::PresentationEvent::Dialogue`] and returns whether the voice started.
/// The subtitle text is read from the emitting actor when it is the `DialogueManager` on the
/// decoded dialogue path (its `LineIndex` -> `Lines[..].SpeakerIndex/SentenceIndex` ->
/// `Speakers[..].Sentences[..].Sentences`); for any other actor the text is `None`.
fn play_str_voice(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let sound = string(vm, a, 0)?;
    let speaker = match a.get(1) {
        Some(Value::Object(Some(r))) => Some(vm.obj_label(r)),
        _ => None,
    };
    let text = dialogue_line_text(vm, c.this);
    let duration = vm.voice_duration(&sound);
    if sound.is_empty() {
        return val(Value::Bool(false));
    }
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    // The voice is also a sound to play. The Bevy audio layer (item6b) consumes
    // `PlaySound`/`PlayMusic`/`PlayRolloffSound`; emitting the resolved voice name as a
    // `PlaySound` makes the line audible through the existing path without new audio wiring,
    // while the `Dialogue` event carries the subtitle/speaker for the cutscene HUD.
    vm.emit_event(crate::events::PresentationEvent::PlaySound(
        crate::events::SoundEvent {
            actor: actor.clone(),
            sound: Some(sound.clone()),
            rolloff_actor: speaker.clone(),
            slot: None,
            volume: None,
            radius: None,
            pitch: None,
            param5: None,
            time,
        },
    ));
    vm.emit_event(crate::events::PresentationEvent::Dialogue(
        crate::events::DialogueEvent {
            actor,
            speaker,
            sound,
            text,
            duration,
            time,
        },
    ));
    // The engine's audio subsystem fires `EndOfVoice` on the speaking actor when the wave ends;
    // the headless VM has no audio callback, so it schedules the actor's `Timer` event for the
    // same delay (`DialogueManager.STA_HeadAnimation.Timer` calls `EndOfVoice`). Without a host
    // [`crate::voice::VoiceDuration`] provider the native reports the voice as not started, so the
    // script takes its own `STA_HeadAnimation.NoSound` 2 s timer branch (never a silent success).
    match duration {
        Some(d) => {
            vm.set_timer(c.this, d.max(0.01), false);
            val(Value::Bool(true))
        }
        None => val(Value::Bool(false)),
    }
}

/// `Actor.PlayVoice(object<Sound> Sound, optional int Param1..Param5)` (native 353): the
/// `Sound`-object form of `PlayStrVoice`. It emits the same `PlaySound` presentation event the
/// other sound natives use so a non-dialogue caller is not dropped; the subtitle text is not
/// available in this form.
fn play_voice(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let sound = a.first().and_then(|v| vm.obj_path(v));
    let Some(sound) = sound else {
        return val(Value::Void);
    };
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(crate::events::PresentationEvent::Dialogue(
        crate::events::DialogueEvent {
            actor,
            speaker: None,
            sound,
            text: None,
            duration: None,
            time,
        },
    ));
    val(Value::Void)
}

/// Reads the current subtitle text of a `DialogueManager` from its own map data. Returns `None`
/// for any actor without the decoded `LineIndex`/`Lines`/`Speakers` shape or an out-of-range
/// index. The nested struct member names come from the decoded class layout; matching is
/// case-insensitive because the loader preserves the declaration's casing.
pub fn dialogue_line_text(vm: &Vm<'_>, this: ObjectId) -> Option<String> {
    let line_index = match vm.get_property(this, "LineIndex") {
        Some(Value::Int(i)) => *i,
        _ => return None,
    };
    if line_index < 0 {
        return None;
    }
    let lines = vm.get_property(this, "Lines")?;
    let speakers = vm.get_property(this, "Speakers")?;
    line_text_from_values(line_index, lines, speakers)
}

/// Pure helper behind [`dialogue_line_text`], split out so the index/member lookups can be tested
/// without a full VM fixture.
pub fn line_text_from_values(line_index: i32, lines: &Value, speakers: &Value) -> Option<String> {
    if line_index < 0 {
        return None;
    }
    let line = match lines {
        Value::Array(items) => items.get(line_index as usize)?,
        _ => return None,
    };
    let (speaker_index, sentence_index) = match line {
        Value::Struct(fields) => (
            struct_int(fields, "SpeakerIndex")?,
            struct_int(fields, "SentenceIndex")?,
        ),
        _ => return None,
    };
    if speaker_index < 0 || sentence_index < 0 {
        return None;
    }
    let speaker = match speakers {
        Value::Array(items) => items.get(speaker_index as usize)?,
        _ => return None,
    };
    let sentences = match speaker {
        Value::Struct(fields) => struct_member(fields, "Sentences")?,
        _ => return None,
    };
    // `SSpeaker.Sentences` is an `array<string>` (the decoded layout), so the element at the line's
    // `SentenceIndex` is the text directly.
    let sentence = match sentences {
        Value::Array(items) => items.get(sentence_index as usize)?,
        _ => return None,
    };
    match sentence {
        Value::Str(s) => Some(s.clone()),
        _ => None,
    }
}

fn struct_member<'a>(fields: &'a [(String, Value)], name: &str) -> Option<&'a Value> {
    fields
        .iter()
        .find(|(n, _)| n.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

fn struct_int(fields: &[(String, Value)], name: &str) -> Option<i32> {
    match struct_member(fields, name)? {
        Value::Int(v) => Some(*v),
        _ => None,
    }
}

/// The cinematic natives. `registry::builtin_defs` extends the built-in table with these.
pub fn cinematic_defs() -> Vec<NativeDef> {
    vec![
        partial(
            "voice duration comes from the host provider; without one the call reports 0 (the \
             script then uses its own 3-second fallback), never a guessed length",
            "Engine.Actor.GetWaveDuration",
            "native(357) final static function float GetWaveDuration(string SoundName)",
            "engine.u Actor.GetWaveDuration decoded; xidcine.DialogueManager.Speak uses it to \
             size the message lifetime; Engine.dll ?execGetWaveDuration@AActor",
            get_wave_duration,
        ),
        def(
            "Engine.Actor.PlayStrVoice",
            "native(354) final static function bool PlayStrVoice(string SoundName, object<Actor> \
             RollOffActor)",
            "engine.u Actor.PlayStrVoice decoded; xidcine.DialogueManager.Speak passes the \
             Level.Title_PawnName_Index voice name and the speaking pawn; emits \
             PresentationEvent::Dialogue",
            play_str_voice,
        ),
        partial(
            "the Sound-object form has no subtitle text; emitted as a Dialogue event carrying \
             only the resolved Sound path",
            "Engine.Actor.PlayVoice",
            "native(353) final static function PlayVoice(object<Sound> Sound, optional int Param1, \
             optional int Param2, optional int Param3, optional int Param4, optional int Param5)",
            "engine.u Actor.PlayVoice decoded (Sound + five optional ints, void); the \
             Sound-object counterpart of PlayStrVoice; Engine.dll ?execPlayVoice@AActor",
            play_voice,
        ),
    ]
}
