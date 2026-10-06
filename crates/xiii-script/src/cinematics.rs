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

/// item19: `CineController2.Steering(Vector vTargetLocation, float dt, float fDetectionDistance,
/// bool bDisableAvoidance, bool bFinalLocation)` (native, XIDCine.dll RVA 0x21a0).
///
/// `CineController2.PlayingSequence.Tick` calls `Steering` every tick while a `movseq`/`movseqb`
/// action is moving the possessed cine pawn. The engine native steers that pawn toward the target
/// through the physics tick and the engine dispatches the controller's `EndOfMove` when the move
/// finishes; the sequence's own `EndOfMove` -> `NextMove` -> `EndOfSeq` chain then clears the
/// `endofseq` pause bit that gates the next action. The VM runs no NPC physics tick, so this
/// native performs the swept-horizontal move itself through the world provider
/// ([`Vm::move_pawn_step`], the same step the latent `MoveTo` uses) and dispatches the
/// controller's own `EndOfMove` on arrival. Without it the Plage01 intro suspended at the first
/// `movseqb` (`UnimplementedNative`), so the level-start cutscene never reached its end.
fn cine_steering(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let target = match a.first() {
        Some(Value::Vector(v)) => *v,
        Some(other) => {
            return Err(vm.err(VmErrorKind::TypeMismatch {
                expected: "vector",
                found: other.type_name(),
            }));
        }
        None => return Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    };
    let dt = match a.get(1) {
        Some(Value::Float(v)) => *v,
        _ => return val(Value::Void),
    };
    // The steered actor is the possessed cine pawn (`Controller.Pawn`); `Player` is only the
    // watched player pawn.
    let Some(pawn) = vm.obj_prop(c.this, "Pawn") else {
        return val(Value::Void);
    };
    let speed = vm.f32_prop(pawn, "GroundSpeed");
    // During a `movseq` the sequence turns the cine pawn's world collision off (`collisionoff`)
    // so it follows the authored path through geometry; `Move` would instead stop at the walls.
    let arrived = if vm.bool_prop(pawn, "bCollideWorld") {
        vm.move_pawn_step(pawn, target, speed, dt)?
    } else {
        let loc = vm.vector_prop(pawn, "Location").unwrap_or(target);
        let radius = vm.f32_prop(pawn, "CollisionRadius");
        let (next, arrived) = crate::navigation::move_step(loc, target, speed, dt, radius);
        if next != loc {
            vm.set_property(pawn, "Location", 0, Value::Vector(next));
        }
        arrived
    };
    if std::env::var_os("XIII_CINE_TRACE").is_some_and(|v| v != "0" && !v.is_empty())
        && vm.objects[pawn as usize]
            .name
            .eq_ignore_ascii_case("Cine11")
    {
        let from = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let delta = [
            target[0] - from[0],
            target[1] - from[1],
            target[2] - from[2],
        ];
        let distance = (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt();
        println!(
            "[cine-trace-steering] t={:.3}s tick={} controller={} pawn={} dt={dt:.5} speed={speed:.3} from={from:?} target={target:?} distance={distance:.3} collide-world={} arrived={arrived}",
            vm.time,
            vm.tick_count,
            vm.objects[c.this as usize].name,
            vm.objects[pawn as usize].name,
            vm.bool_prop(pawn, "bCollideWorld")
        );
    }
    if arrived {
        vm.send_event(c.this, "EndOfMove", Vec::new())?;
    }
    val(Value::Void)
}

/// item19: return an actor-space Coords placeholder for a skeletal bone query. The camera-only
/// `BeachInBedWithXIII.LookAtBimbo.Tick` reads `.Origin`; the VM has no posed-bone transform
/// provider, so the actor origin is returned and the Partial is visible in the native catalog.
/// This keeps the wake-up state alive to execute the map's GetUpStandUp/control-return chain.
fn get_bone_coords_partial(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _: &mut [Value],
) -> VmResult<NativeOutcome> {
    let origin = vm.vector_prop(c.this, "Location").unwrap_or([0.0; 3]);
    vm.note(TraceKind::Note(format!(
        "{}: skeletal bone pose unavailable; Coords.Origin uses actor Location (item19 Partial)",
        c.path
    )));
    val(Value::Struct(vec![
        ("Origin".into(), Value::Vector(origin)),
        ("XAxis".into(), Value::Vector([1.0, 0.0, 0.0])),
        ("YAxis".into(), Value::Vector([0.0, 1.0, 0.0])),
        ("ZAxis".into(), Value::Vector([0.0, 0.0, 1.0])),
    ]))
}

/// item19: bone rotation is likewise presentation-only for the remaining decoded cine path;
/// return the actor's rotation with an explicit Partial note rather than failing the wake-up.
fn get_bone_rotation_partial(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _: &mut [Value],
) -> VmResult<NativeOutcome> {
    let rotation = vm.rotation_prop(c.this).unwrap_or([0; 3]);
    vm.note(TraceKind::Note(format!(
        "{}: skeletal bone pose unavailable; rotation uses actor Rotation (item19 Partial)",
        c.path
    )));
    val(Value::Rotator(rotation))
}

/// item19 natives. Kept in one block so a parallel registry edit merges without touching it;
/// `registry::builtin_defs` extends the built-in table with [`item19_defs`].
pub fn item19_defs() -> Vec<NativeDef> {
    vec![
        def(
            "CineController2.Steering",
            "native(0) function Steering(struct<Vector> vTargetLocation, float dt, float \
             fDetectionDistance, bool bDisableAvoidance, bool bFinalLocation)",
            "XIDCine.dll ?execSteering@ACineController2 RVA 0x21a0 (steers Controller.Pawn toward \
             vTargetLocation; the engine's EndOfMove ends the move); xidcine.u \
             CineController2.PlayingSequence.Tick 0x06DA calls it every tick of a movseq/movseqb \
             action. The VM has no NPC physics tick, so it performs the world-provider step and \
             dispatches EndOfMove on arrival.",
            cine_steering,
        ),
        partial(
            "skeletal bone transform unavailable; returns Coords with actor Location as Origin",
            "Engine.Actor.GetBoneCoords",
            "native(410) final native function Coords GetBoneCoords(name BoneName)",
            "engine.u Actor.GetBoneCoords; XIDCine.BeachInBedWithXIII.LookAtBimbo.Tick reads \
             GetBoneCoords('X Neck').Origin to aim the wake-up camera. Headless VM has no posed-bone \
             query, so item19 supplies an explicit actor-origin Partial so the state can reach \
             GetUpStandUp and its game-owned PlayerWalking transition.",
            get_bone_coords_partial,
        ),
        partial(
            "skeletal bone rotation unavailable; returns actor Rotation",
            "Engine.Actor.GetBoneRotation",
            "native(409) final native function Rotator GetBoneRotation(name BoneName, optional int Space)",
            "engine.u Actor.GetBoneRotation; item19's cutscene path receives an explicit actor-rotation \
             Partial rather than an unimplemented-native suspension",
            get_bone_rotation_partial,
        ),
    ]
}
