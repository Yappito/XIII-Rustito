//! Cartoon-panel natives (`xiii.XIIIBaseHud`, `Engine.RenderTargetMaterial`).
//!
//! The comic-panel "cartoon effect" is script-driven: `XIIIBaseHud.DrawCartoonWindowBis`
//! records its panels through the normal `Canvas` natives and, for the panel content, reads the
//! HUD's `CWndMat` `Engine.RenderTargetMaterial`. Three natives on that path were declared but
//! unregistered, so the script aborted at the first call (the item3l blocker):
//!
//! * `Actor.PlayMenu(object<Sound>, int Type, int Param1..Param5)` (native 351): the intro
//!   jingles (`XIIIBaseHud.hIntroBeginMap*`) played when the cartoon appears/switches/ends.
//!   The decoded parameters beyond `Sound` are generic; the implementation emits the existing
//!   `PlaySound` presentation event (audio resolves it) and returns void.
//! * `RenderTargetMaterial.Update(int X, int Y, int width, int Height, Vector CamLocation,
//!   Rotator CamRotation, float ViewFOV, Color FilterColor, float HighLight, Material
//!   FilterTexture)`: renders the current camera view into the panel material. The VM records a
//!   typed [`crate::events::PresentationEvent::RenderTarget`] carrying the pose so the host can
//!   render it to a texture; the material's pixel work is host presentation, not simulation.
//! * `RenderTargetMaterial.AllocRect`/`FreeRect`: the focus-window (`HudCartoonFocus`) rectangle
//!   allocator. There is no native render-target allocator in the headless VM, so each call is
//!   recorded as a visible `Partial` note (never a silent success).
//!
//! Every entry lives in this one block so the parallel registry edit merges without touching it;
//! `registry::builtin_defs` extends the built-in table with [`cartoon_defs`].

use crate::events::{PresentationEvent, RenderTargetEvent, SoundEvent};
use crate::registry::{NativeCtx, NativeDef, NativeFn, NativeOutcome, NativeStatus};
use crate::value::{ObjRef, ObjectId, Value};
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

fn int(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<i32> {
    match a.get(i) {
        Some(Value::Int(v)) => Ok(*v),
        Some(Value::Byte(v)) => Ok(i32::from(*v)),
        Some(other) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "int",
            found: other.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn float(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<f32> {
    match a.get(i) {
        Some(Value::Float(v)) => Ok(*v),
        Some(other) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "float",
            found: other.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn vector(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<[f32; 3]> {
    match a.get(i) {
        Some(Value::Vector(v)) => Ok(*v),
        Some(other) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "vector",
            found: other.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn rotator(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<[i32; 3]> {
    match a.get(i) {
        Some(Value::Rotator(r)) => Ok(*r),
        Some(other) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "rotator",
            found: other.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

/// `Actor.PlayMenu(object<Sound> Sound, int Type, int Param1..Param5)` (native 351).
///
/// The decoded call sites are `XIIIBaseHud.DrawCartoonWindowBis`'s intro jingles; the call
/// passes only the `Sound` and omits the rest. The `Type`/`Param*` names are generic, so the
/// semantic mapping used here (slot = `Type`, volume = `Param1`, ...) is a **hypothesis**; the
/// sound object path is preserved exactly. Emits `PlaySound` so the item6b audio layer resolves
/// it, and returns void (the decoded return).
fn play_menu(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let sound = a.first().and_then(|v| vm.obj_path(v));
    let opt = |a: &[Value], i: usize| match a.get(i) {
        Some(Value::Int(v)) => Some(*v),
        Some(Value::Byte(v)) => Some(i32::from(*v)),
        _ => None,
    };
    let actor = vm.objects[c.this as usize].name.clone();
    vm.emit_event(PresentationEvent::PlaySound(SoundEvent {
        actor,
        sound,
        rolloff_actor: None,
        slot: opt(a, 1),
        volume: opt(a, 2),
        radius: opt(a, 3),
        pitch: opt(a, 4),
        param5: opt(a, 5),
        time: vm.time,
    }));
    val(Value::Void)
}

/// `RenderTargetMaterial.Update(...)`: records the camera pose for the host's render-to-texture.
fn render_target_update(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let x = int(vm, a, 0)?;
    let y = int(vm, a, 1)?;
    let width = int(vm, a, 2)?;
    let height = int(vm, a, 3)?;
    let cam_location = vector(vm, a, 4)?;
    let cam_rotation = rotator(vm, a, 5)?;
    let fov = float(vm, a, 6)?;
    let actor = vm.objects[c.this as usize].name.clone();
    vm.emit_event(PresentationEvent::RenderTarget(RenderTargetEvent {
        actor,
        x,
        y,
        width,
        height,
        cam_location,
        cam_rotation,
        fov,
        time: vm.time,
    }));
    val(Value::Void)
}

/// `RenderTargetMaterial.AllocRect(int width, int Height, int X, int Y)`: no native allocator;
/// a visible note is recorded (the focus window then early-outs on a negative rect).
fn render_target_alloc(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (width, height, x, y) = (
        int(vm, a, 0)?,
        int(vm, a, 1)?,
        int(vm, a, 2)?,
        int(vm, a, 3)?,
    );
    vm.note(TraceKind::Note(format!(
        "RenderTargetMaterial.AllocRect({width}, {height}, {x}, {y}) on {}: no native render-target allocator (Partial)",
        vm.objects[c.this as usize].name
    )));
    val(Value::Void)
}

/// `RenderTargetMaterial.FreeRect(int X, int Y, int width, int Height)`: no native allocator;
/// a visible note is recorded.
fn render_target_free(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y, width, height) = (
        int(vm, a, 0)?,
        int(vm, a, 1)?,
        int(vm, a, 2)?,
        int(vm, a, 3)?,
    );
    vm.note(TraceKind::Note(format!(
        "RenderTargetMaterial.FreeRect({x}, {y}, {width}, {height}) on {}: no native render-target allocator (Partial)",
        vm.objects[c.this as usize].name
    )));
    val(Value::Void)
}

/// The cartoon-panel natives. `registry::builtin_defs` extends the built-in table with these.
pub fn cartoon_defs() -> Vec<NativeDef> {
    let mut v = vec![
        partial(
            "the decoded parameters beyond Sound are generic; they are forwarded to PlaySound \
             (slot/volume/radius/pitch mapping is a hypothesis) and the sound object path is \
             preserved exactly",
            "Engine.Actor.PlayMenu",
            "native(351) final native static function PlayMenu(object<Sound> Sound, int Type, \
             int Param1, int Param2, int Param3, int Param4, int Param5)",
            "engine.u Actor.PlayMenu decoded; xiii.XIIIBaseHud.DrawCartoonWindowBis plays \
             hIntroBeginMap/hIntroBeginMapFocus/hIntroBeginMapPlayerMove through it; \
             Engine.dll ?execPlayMenu@AActor",
            play_menu,
        ),
        def(
            "Engine.RenderTargetMaterial.Update",
            "native(0) function Update(int X, int Y, int width, int Height, struct<Vector> \
             CamLocation, struct<Rotator> CamRotation, float ViewFOV, struct<Color> FilterColor, \
             float HighLight, object<Material> FilterTexture)",
            "engine.u RenderTargetMaterial.Update decoded; xiii.CWndFullScreen.Timer calls \
             CWndMat.Update(0, 0, 255, 255, CameraLocation, CameraRotation, DefaultFOV, \
             FilterColor, 0.0, None) so the comic panels sample the player view; emits \
             PresentationEvent::RenderTarget for the host render-to-texture",
            render_target_update,
        ),
        partial(
            "no native render-target allocator in the VM; a visible note is recorded",
            "Engine.RenderTargetMaterial.AllocRect",
            "native(0) function AllocRect(int width, int Height, int X, int Y)",
            "engine.u RenderTargetMaterial.AllocRect decoded; xiii.HudCartoonFocus.AppearZoom \
             calls it to reserve a vignette rect",
            render_target_alloc,
        ),
        partial(
            "no native render-target allocator in the VM; a visible note is recorded",
            "Engine.RenderTargetMaterial.FreeRect",
            "native(0) function FreeRect(int X, int Y, int width, int Height)",
            "engine.u RenderTargetMaterial.FreeRect decoded; xiii.HudCartoonWindow/HudCartoonFocus \
             RemoveMe release their rect through it",
            render_target_free,
        ),
    ];
    v.extend(cine_parse_defs());
    v.extend(missing_core_defs());
    v
}

// ---------------------------------------------------------------------------------------------
// `xidcine.CineController2` action-parsing natives. `CineController2.Interpret` (the sequence
// interpreter the dialogue action runs through) splits each action string with `GetFirstWord`
// and looks actors up with `FindAnActor`; all three were declared but unregistered, so the
// sequence stopped at its first `dial` action and the Plage00 opening never spoke. Semantics are
// read from XIDCine.dll (`?GetFirstWord`/`?execCharIsNum`/`?execFindAnActor`).

/// `CineController2.GetFirstWord(out string S) -> string`.
///
/// XIDCine.dll `?GetFirstWord@ACineController2` RVA 0x1100: skips leading spaces, returns the
/// pointer to the token start, replaces the terminating space with `\0` and advances the `out`
/// pointer past it (so repeated calls iterate the words). Empty input returns the empty string.
fn get_first_word(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = match a.first() {
        Some(Value::Str(s)) | Some(Value::Name(s)) => s.clone(),
        _ => String::new(),
    };
    let (word, rest) = first_word_rest(&s);
    if let Some(slot) = a.first_mut() {
        *slot = Value::Str(rest);
    }
    let _ = vm;
    val(Value::Str(word))
}

/// Pure split behind [`get_first_word`]: skip leading spaces, take the first token, and return the
/// remainder after exactly one terminating space (the next call skips any further spaces). Empty
/// input returns two empty strings; a single token returns the whole token and an empty remainder.
pub(crate) fn first_word_rest(s: &str) -> (String, String) {
    let trimmed = s.trim_start_matches(' ');
    match trimmed.find(' ') {
        Some(i) => (trimmed[..i].to_owned(), trimmed[i + 1..].to_owned()),
        None => (trimmed.to_owned(), String::new()),
    }
}

/// `CineController2.CharIsNum(string S) -> bool`.
///
/// XIDCine.dll `?execCharIsNum@ACineController2` RVA 0x12b0: true iff the first character of `S`
/// is an ASCII digit (`'0'..='9'`). Used by `Interpret` to choose a line index over a dialogue
/// tag for the `dial` action.
fn char_is_num(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = match a.first() {
        Some(Value::Str(s)) | Some(Value::Name(s)) => s.as_str(),
        _ => "",
    };
    let _ = vm;
    val(Value::Bool(
        s.chars().next().is_some_and(|c| c.is_ascii_digit()),
    ))
}

/// `CineController2.FindAnActor(string ActorName) -> Actor`.
///
/// XIDCine.dll `?execFindAnActor@ACineController2` RVA 0x1e70: walks the level's live actors and
/// returns the one whose name matches `ActorName` (FName comparison, case-insensitive). The VM
/// searches every live actor, which is the same set `Interpret` addresses.
fn find_an_actor(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = match a.first() {
        Some(Value::Str(s)) | Some(Value::Name(s)) => s.clone(),
        _ => String::new(),
    };
    let found = (0..vm.objects.len() as ObjectId).find(|&i| {
        let o = &vm.objects[i as usize];
        o.is_actor && !o.deleted && o.name.eq_ignore_ascii_case(&name)
    });
    let v = match found {
        Some(id) => Value::Object(Some(ObjRef::Instance(id))),
        None => Value::Object(None),
    };
    val(v)
}

/// The `CineController2` action-parsing natives. Called from [`cartoon_defs`].
fn cine_parse_defs() -> Vec<NativeDef> {
    vec![
        def(
            "CineController2.GetFirstWord",
            "native(0) final native static function string GetFirstWord(out string S)",
            "xidcine.u CineController2.GetFirstWord declared; XIDCine.dll \
             ?GetFirstWord@ACineController2 RVA 0x1100 (skip spaces, null-terminate the token, \
             advance the out pointer); CineController2.Interpret splits every action with it",
            get_first_word,
        ),
        def(
            "CineController2.CharIsNum",
            "native(0) final native static function bool CharIsNum(string S)",
            "xidcine.u CineController2.CharIsNum declared; XIDCine.dll \
             ?execCharIsNum@ACineController2 RVA 0x12b0 (first char in '0'..='9'); selects \
             ForceLine vs StartDialogue in the Interpret dial action",
            char_is_num,
        ),
        def(
            "CineController2.FindAnActor",
            "native(0) final native static function Actor FindAnActor(string strActorName)",
            "xidcine.u CineController2.FindAnActor declared; XIDCine.dll \
             ?execFindAnActor@ACineController2 RVA 0x1e70 (live-actor name match, \
             case-insensitive FName compare); used by Interpret for DialMan/doors/Fall/Jump",
            find_an_actor,
        ),
    ]
}

// ---------------------------------------------------------------------------------------------
// Core operator natives on the cartoon/cine path that the registry did not yet implement.
// `CineController2.PlayingSequence.Tick` (the sequence interpreter) uses `Object.Or_IntInt` for
// its pause flags; the pawn/HUD `Tick` events use `Object.NotEqual_VectorVector` and (for the
// helicopter decor) `Object.GetAxes`. Their UE2 semantics are the `Core.Object` operators.

/// `Object.Or_IntInt(int A, int B) -> int` (native 158): bitwise OR.
fn or_ii(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let _ = vm;
    val(Value::Int(int(vm, a, 0)? | int(vm, a, 1)?))
}

/// `Object.NotEqual_VectorVector(Vector A, Vector B) -> bool` (native 218): componentwise `!A==B`
/// (`FVector::operator!=`, the negation of the decoded `EqualEqual_VectorVector`).
fn not_equal_vv(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let _ = vm;
    val(Value::Bool(vector(vm, a, 0)? != vector(vm, a, 1)?))
}

/// Core operator natives on the cartoon/cine path. Called from [`cartoon_defs`].
fn missing_core_defs() -> Vec<NativeDef> {
    vec![
        def(
            "Object.Or_IntInt",
            "native(158) final native operator static function int Or_IntInt(int A, int B)",
            "core.u Object.Or_IntInt decoded (native 158); used by \
             CineController2.PlayingSequence.Tick for its pause-flag masks",
            or_ii,
        ),
        def(
            "Object.NotEqual_VectorVector",
            "native(218) final native operator static function bool NotEqual_VectorVector(struct<Vector> A, \
             struct<Vector> B)",
            "core.u Object.NotEqual_VectorVector decoded (native 218); negation of the \
             EqualEqual_VectorVector operator; used by pawn/cine Tick velocity checks",
            not_equal_vv,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::first_word_rest;

    /// XIDCine `GetFirstWord`: leading spaces skipped, one token returned, the remainder starts
    /// after its terminating space. Boundary and degenerate inputs.
    #[test]
    fn first_word_splits_like_the_cine_interpreter() {
        assert_eq!(
            first_word_rest("dial dial_debut"),
            ("dial".into(), "dial_debut".into())
        );
        assert_eq!(
            first_word_rest("wait   event pam"),
            ("wait".into(), "  event pam".into())
        );
        assert_eq!(
            first_word_rest("  pauseplayer"),
            ("pauseplayer".into(), String::new())
        );
        assert_eq!(first_word_rest(""), (String::new(), String::new()));
        assert_eq!(first_word_rest("   "), (String::new(), String::new()));
        // A single trailing space yields an empty remainder, not a dropped space.
        assert_eq!(first_word_rest("anim "), ("anim".into(), String::new()));
    }
}
