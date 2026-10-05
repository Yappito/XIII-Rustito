//! Residual intro/checkpoint natives for the Plage00 opening.
//!
//! The natives the level-start checkpoint path, the helicopter decor and the dialogue jaw
//! animation needed were still declared but unregistered (item3o):
//!
//! * `Actor.SaveAtCheckpoint(string TeleporterName, string ContentDescription) -> bool` — the
//!   final native `XIIISaveGameTrigger.GoSaving.DoSave` calls after building the
//!   `XIIIThingsToSave` actor (`0x03D9`). The VM is headless and read-only over the installation,
//!   so it emits a typed [`crate::events::PresentationEvent::SaveCheckpoint`] for the host and
//!   returns `true`, matching the decoded "return bool" contract. Nothing is written to disk here.
//! * `Object.OrthoRotation(vector X, vector Y, vector Z) -> rotator` (native 197) — the
//!   `xidcine.HelicoDeco.HelicoTick` rotation-correction step. Implemented as the inverse of the
//!   `FRotationMatrix` basis used by `Object.GetAxes`/`vector >> rotator` (see
//!   [`rotator_from_basis`]).
//! * `Object.Multiply_RotatorFloat`/`MultiplyMultiply_FloatFloat`/`Multiply_FloatRotator`, the
//!   operator natives the helicopter and dialogue-animation `Tick`s call.
//! * `Pawn.PressingFire`, `Actor.SetBoneRotation`/`SetBoneLocation`/`SetBoneScale`, the remaining
//!   per-frame natives on the weapon/dialogue path.
//!
//! Every entry lives in this one block so a parallel registry edit merges without touching it;
//! `registry::builtin_defs` extends the built-in table with [`residual_defs`].

use crate::events::{PresentationEvent, SaveCheckpointEvent};
use crate::registry::{NativeCtx, NativeDef, NativeFn, NativeOutcome, NativeStatus};
use crate::value::{ObjRef, Value};
use crate::vm::{Vm, VmErrorKind, VmResult};

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

fn val(v: Value) -> VmResult<NativeOutcome> {
    Ok(NativeOutcome::Value(v))
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

fn vector(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<[f32; 3]> {
    match a.get(i) {
        Some(Value::Vector(v)) => Ok(*v),
        Some(v) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "vector",
            found: v.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn rotator(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<[i32; 3]> {
    match a.get(i) {
        Some(Value::Rotator(r)) => Ok(*r),
        Some(v) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "rotator",
            found: v.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn float(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<f32> {
    match a.get(i) {
        Some(Value::Float(v)) => Ok(*v),
        Some(v) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "float",
            found: v.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn name(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<String> {
    match a.get(i) {
        Some(Value::Name(v)) => Ok(v.clone()),
        Some(v) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "name",
            found: v.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn int(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<i32> {
    match a.get(i) {
        Some(Value::Int(v)) => Ok(*v),
        Some(Value::Byte(v)) => Ok(i32::from(*v)),
        Some(v) => Err(vm.err(VmErrorKind::TypeMismatch {
            expected: "int",
            found: v.type_name(),
        })),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

/// `Actor.SetBoneRotation(name, rotator, int Space, float Alpha)` (native 397): recorded for the
/// renderer, no skeletal transform evaluated (documented `Partial`).
fn set_bone_rotation(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let bone = name(vm, a, 0)?;
    let turn = rotator(vm, a, 1)?;
    let space = if c.omitted(2) { 0 } else { int(vm, a, 2)? };
    let alpha = if c.omitted(3) { 0.0 } else { float(vm, a, 3)? };
    vm.add_bone_rotation(c.this, bone, turn, space, alpha);
    val(Value::Void)
}

/// `Actor.SetBoneLocation(name, vector, float Alpha)` (native 398): recorded for the renderer.
fn set_bone_location(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let bone = name(vm, a, 0)?;
    let trans = vector(vm, a, 1)?;
    let alpha = if c.omitted(2) { 0.0 } else { float(vm, a, 2)? };
    vm.add_bone_location(c.this, bone, trans, alpha);
    val(Value::Void)
}

/// `Actor.SetBoneScale(int Slot, float BoneScale, name BoneName)` (native 401): the uniform form,
/// recorded per actor for the renderer.
fn set_bone_scale(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let slot = int(vm, a, 0)?;
    let scale = float(vm, a, 1)?;
    let bone = name(vm, a, 2)?;
    vm.add_bone_scale(c.this, slot, [scale, scale, scale], bone);
    val(Value::Void)
}

/// `Object.MultiplyMultiply_FloatFloat(float A, float B) -> float` (native 170): `A ** B`, the
/// UE2 `appPow` float exponent.
fn multiply_multiply_ff(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    val(Value::Float(x.powf(y)))
}

/// `Object.Multiply_FloatRotator(float A, rotator B) -> rotator` (native 288): the float-first
/// form of UE2 `FRotator::operator*(float)`, componentwise truncated:
/// `xidcine.DialogueManager.STA_HeadAnimation.Tick` builds its jaw rotation with it.
fn multiply_float_rotator(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let s = float(vm, a, 0)?;
    let r = rotator(vm, a, 1)?;
    val(Value::Rotator([
        (r[0] as f32 * s) as i32,
        (r[1] as f32 * s) as i32,
        (r[2] as f32 * s) as i32,
    ]))
}

/// `Pawn.PressingFire() -> bool`: whether the controlling player is holding the fire input.
///
/// UE2 `APawn::PressingFire` returns `Controller && Controller->bFire`. The host sets `bFire` on
/// the controller (`--play` firing, item14) and the campaign reads it through `XIIIWeapon`'s
/// state code; a missing controller or flag is `false` (no input). Both the `bool` and `byte`
/// encodings the engine/corpus use are accepted.
fn pressing_fire(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let controller = match vm.get_property(c.this, "Controller") {
        Some(Value::Object(Some(ObjRef::Instance(i)))) if *i != c.this => Some(*i),
        _ => None,
    };
    let pressed = controller.is_some_and(|ctrl| {
        matches!(
            vm.get_property(ctrl, "bFire"),
            Some(Value::Bool(true)) | Some(Value::Byte(1)) | Some(Value::Int(1))
        )
    });
    val(Value::Bool(pressed))
}

/// `Actor.SaveAtCheckpoint`: record the checkpoint and report success.
fn save_at_checkpoint(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let teleporter_name = string(vm, a, 0)?;
    let description = string(vm, a, 1)?;
    let actor = vm.objects[c.this as usize].name.clone();
    vm.emit_event(PresentationEvent::SaveCheckpoint(SaveCheckpointEvent {
        actor,
        teleporter_name,
        description,
        time: vm.time,
    }));
    val(Value::Bool(true))
}

/// `Object.Multiply_RotatorFloat(rotator A, float B) -> rotator` (native 287).
///
/// UE2 `FRotator::operator*(FLOAT)`: each component scaled and truncated toward zero (the C++
/// float-to-`INT` conversion), the exact form used by `xidcine.HelicoDeco.HelicoTick` to damp the
/// corrected rotation (`Multiply_RotatorFloat(R, 1 - inertia*dt*100)`).
fn multiply_rotator_float(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let r = rotator(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    val(Value::Rotator([
        (r[0] as f32 * s) as i32,
        (r[1] as f32 * s) as i32,
        (r[2] as f32 * s) as i32,
    ]))
}

/// `Object.OrthoRotation(X, Y, Z) -> rotator` (native 197).
///
/// The three arguments are the orthonormal basis of a coordinate system (forward, right, up);
/// the native returns the rotator whose `FRotationMatrix` has that basis. This is the inverse of
/// [`rotator_basis`], the same `Pitch`/`Yaw`/`Roll` extraction as UE1 `FCoords::OrthoRotation`
/// (`XAxis` pitch/yaw, then roll from the residual right/up axes).
fn ortho_rotation(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = vector(vm, a, 0)?;
    let y = vector(vm, a, 1)?;
    let z = vector(vm, a, 2)?;
    val(Value::Rotator(rotator_from_basis(x, y, z)))
}

/// Inverse of [`rotator_basis`]: recover `(pitch, yaw, roll)` in Unreal rotator units (`65536`
/// per turn) from an orthonormal basis `(X forward, Y right, Z up)`.
///
/// * pitch from `X.z` and the horizontal length of `X` (the UE1
///   `appAtan2(XAxis.Z, appSqrt(XAxis.X^2+XAxis.Y^2))`),
/// * yaw from `X.y, X.x`,
/// * roll from the right/up components `Y.z`/`Z.z`, which are `-sin(roll)*cos(pitch)` and
///   `cos(roll)*cos(pitch)`.
///
/// A degenerate horizontal basis (`cos(pitch) == 0`, the gimbal-lock pole) leaves yaw and roll at
/// zero, matching UE1's `appAtan2(0,0) == 0`. The result is verified against `rotator_basis` in
/// the unit tests.
pub(crate) fn rotator_from_basis(x: [f32; 3], y: [f32; 3], z: [f32; 3]) -> [i32; 3] {
    const UNITS_PER_RADIAN: f32 = 65536.0 / std::f32::consts::TAU;
    let horiz = (x[0] * x[0] + x[1] * x[1]).sqrt();
    let pitch = x[2].atan2(horiz);
    let yaw = x[1].atan2(x[0]);
    let roll = if horiz > f32::EPSILON {
        (-y[2]).atan2(z[2])
    } else {
        0.0
    };
    let to_units = |r: f32| {
        let mut u = (r * UNITS_PER_RADIAN).round() as i64;
        u = u.rem_euclid(65536);
        u as i32
    };
    [to_units(pitch), to_units(yaw), to_units(roll)]
}

/// The residual natives. Called from [`crate::registry::builtin_defs`].
pub fn residual_defs() -> Vec<NativeDef> {
    vec![
        def(
            "Engine.Actor.SaveAtCheckpoint",
            "native(0) final native static function bool SaveAtCheckpoint(string TeleporterName, \
             string ContentDescription)",
            "engine.u Actor.SaveAtCheckpoint decoded (final native static, return bool); \
             xiii.XIIISaveGameTrigger.GoSaving.DoSave 0x03D9 passes TeleporterName and the \
             SaveDescription; emits PresentationEvent::SaveCheckpoint and returns true; nothing \
             is written to disk by the VM",
            save_at_checkpoint,
        ),
        def(
            "Object.OrthoRotation",
            "native(197) final native static function Rotator OrthoRotation(struct<Vector> X, \
             struct<Vector> Y, struct<Vector> Z)",
            "core.u Object.OrthoRotation decoded (native 197); UE1 UnMath.cpp \
             FCoords::OrthoRotation pitch/yaw from XAxis then roll from the residual Y/Z axes; \
             inverse of this crate's FRotationMatrix basis; xidcine.HelicoDeco.HelicoTick 0x0131 \
             calls OrthoRotation(Y, -X, Z) to correct the helicopter rotation",
            ortho_rotation,
        ),
        def(
            "Object.Multiply_RotatorFloat",
            "native(287) final native operator static function Rotator Multiply_RotatorFloat(\
             struct<Rotator> A, float B)",
            "core.u Object.Multiply_RotatorFloat decoded (native 287); UE2 FRotator::operator*(float) \
             componentwise truncation; xidcine.HelicoDeco.HelicoTick 0x01EC damps the corrected \
             rotation with it",
            multiply_rotator_float,
        ),
        def(
            "Object.MultiplyMultiply_FloatFloat",
            "native(170) final native operator static function float MultiplyMultiply_FloatFloat(\
             float A, float B)",
            "core.u Object.MultiplyMultiply_FloatFloat decoded (native 170); UE2 appPow; \
             xidcine.HelicoDeco.HelicoTick 0x01EC computes inertia**(dt*100) with it",
            multiply_multiply_ff,
        ),
        def(
            "Object.Multiply_FloatRotator",
            "native(288) final native operator static function Rotator Multiply_FloatRotator(\
             float A, struct<Rotator> B)",
            "core.u Object.Multiply_FloatRotator decoded (native 288); UE2 FRotator::operator*(float) \
             float-first, componentwise truncation; xidcine.DialogueManager.STA_HeadAnimation.Tick \
             0x039D builds its jaw rotation with it",
            multiply_float_rotator,
        ),
        def(
            "Engine.Pawn.PressingFire",
            "native(0) final simulated native function bool PressingFire()",
            "engine.u Pawn.PressingFire decoded (return bool); UE2 APawn::PressingFire = \
             Controller && Controller.bFire; xiii.XIIIWeapon firing states poll it; the host sets \
             Controller.bFire from player input (item14), so a headless pawn reads false",
            pressing_fire,
        ),
        partial(
            "the request is recorded per actor for the renderer; no skeletal transform is evaluated",
            "Engine.Actor.SetBoneRotation",
            "native(397) final native static function SetBoneRotation(name BoneName, \
             struct<Rotator> BoneTurn, int Space, float Alpha)",
            "engine.u Actor.SetBoneRotation decoded (native 397); the dialogue jaw/head animation \
             calls it in xidcine.DialogueManager.STA_HeadAnimation.Tick 0x03D9",
            set_bone_rotation,
        ),
        partial(
            "the request is recorded per actor for the renderer; no skeletal transform is evaluated",
            "Engine.Actor.SetBoneLocation",
            "native(398) final native static function SetBoneLocation(name BoneName, \
             struct<Vector> BoneTrans, float Alpha)",
            "engine.u Actor.SetBoneLocation decoded (native 398)",
            set_bone_location,
        ),
        partial(
            "the request is recorded per actor for the renderer; no skeletal transform is evaluated",
            "Engine.Actor.SetBoneScale",
            "native(401) final native static function SetBoneScale(int Slot, float BoneScale, \
             name BoneName)",
            "engine.u Actor.SetBoneScale decoded (native 401); xidcine.DialogueManager \
             STA_HeadAnimation.Tick 0x00E6 resets the jaw bone scale",
            set_bone_scale,
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::rotator_from_basis;
    use crate::registry::rotator_basis;

    /// The extraction must invert `rotator_basis` for a spread of rotations, including the
    /// degenerate pole, negative angles and values that wrap around a full turn. Compared as a
    /// basis (the angles are ambiguous at the pole, where yaw and roll share an axis).
    #[test]
    fn ortho_rotation_inverts_rotator_basis() {
        let cases: [[i32; 3]; 9] = [
            [0, 0, 0],
            [0, 0, 16384],
            [16384, 0, 0],
            [0, 16384, 0],
            [new_pitch(30.0), new_yaw(45.0), new_roll(-60.0)],
            [new_pitch(-75.0), new_yaw(180.0), new_roll(179.0)],
            [new_pitch(89.0), new_yaw(123.0), new_roll(45.0)],
            [new_pitch(0.0), new_yaw(-90.0), new_roll(0.0)],
            [new_pitch(15.0), new_yaw(-170.0), new_roll(-30.0)],
        ];
        for r in cases {
            let (x, y, z) = rotator_basis(r);
            let back = rotator_from_basis(x, y, z);
            let (x2, y2, z2) = rotator_basis(back);
            // The forward axis is always unambiguous.
            for k in 0..3 {
                assert!(
                    (x[k] - x2[k]).abs() < 1e-3,
                    "forward[{k}]: input {r:?} -> {back:?} basis {x:?} vs {x2:?}"
                );
            }
            // At the gimbal-lock pole (cos(pitch) == 0) yaw and roll share an axis, so the
            // right/up axes are only determined up to that rotation; compare them off the pole.
            let horizontal = (1.0 - x[2] * x[2]).sqrt();
            if horizontal > 1e-3 {
                for (axis, (a, b)) in [("y", (y, y2)), ("z", (z, z2))] {
                    for k in 0..3 {
                        assert!(
                            (a[k] - b[k]).abs() < 1e-3,
                            "axis {axis}[{k}]: input {r:?} -> {back:?} basis {a:?} vs {b:?}"
                        );
                    }
                }
            }
        }
    }

    /// A degenerate (non-orthonormal) basis must not panic or produce NaN: the horizontal length
    /// is zero at the pole, so yaw and roll fall back to zero.
    #[test]
    fn ortho_rotation_handles_pole_basis() {
        let r = rotator_from_basis([0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]);
        assert_eq!(r[0], 16384);
        assert_eq!(r[1], 0);
        assert_eq!(r[2], 0);
    }

    fn new_pitch(deg: f32) -> i32 {
        (deg / 360.0 * 65536.0).round() as i32
    }
    fn new_yaw(deg: f32) -> i32 {
        (deg / 360.0 * 65536.0).round() as i32
    }
    fn new_roll(deg: f32) -> i32 {
        (deg / 360.0 * 65536.0).round() as i32
    }
}
