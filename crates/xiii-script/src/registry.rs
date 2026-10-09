//! Native-function registry for the interpreter.
//!
//! Natives are keyed by `Class.Function` (lowercase) of their script declaration; the native
//! index of a call token is first resolved to its declaring function by the VM (duplicate
//! indices are separated by argument count), so the registry never guesses from an index.
//! Every entry records its signature, the source of its semantics and its status. A native
//! that is declared but not registered fails with `VmErrorKind::UnimplementedNative`.

use std::collections::BTreeMap;

use crate::events::{PresentationEvent, SoundEvent, TravelRequest, TravelSource};
use crate::linker::GlobalRef;
use crate::value::{ObjRef, ObjectId, Value};
use crate::vm::{Latent, TraceKind, Vm, VmErrorKind, VmResult};

/// Context of one native invocation.
#[derive(Debug, Clone)]
pub struct NativeCtx {
    /// Object the native runs on.
    pub this: ObjectId,
    /// True when called from the object's own state code (latent natives allowed).
    pub in_state_code: bool,
    /// `Class.Function`.
    pub path: String,
    /// One flag per argument slot in `args`: `true` when the caller *omitted* the optional
    /// argument (UE2 `EX_Nothing`). Natives read this for `P_GET_*_OPTX` semantics instead of
    /// guessing from a zero value.
    pub omitted: Vec<bool>,
}

impl NativeCtx {
    /// True when the argument at `i` was omitted by the caller (optional parameter).
    pub fn omitted(&self, i: usize) -> bool {
        self.omitted.get(i).copied().unwrap_or(false)
    }
}

/// Result of a native.
#[derive(Debug, Clone, PartialEq)]
pub enum NativeOutcome {
    /// Return value (`Void` for none). A latent native also sets the VM's pending latent.
    Value(Value),
    /// Iterator rows; each row supplies the iterator's out parameters in declaration order.
    Iterate(Vec<Vec<Value>>),
}

/// Native implementation.
pub type NativeFn = fn(&mut Vm<'_>, &NativeCtx, &mut [Value]) -> VmResult<NativeOutcome>;

/// Implementation status.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeStatus {
    /// Full semantics for the supported argument types.
    Implemented,
    /// Implemented with a documented simplification.
    Partial(&'static str),
}

/// One registered native.
#[derive(Debug, Clone)]
pub struct NativeDef {
    /// `Class.Function` as declared.
    pub path: &'static str,
    /// Declared signature (from the decoded declaration).
    pub signature: &'static str,
    /// Where the semantics come from.
    pub evidence: &'static str,
    /// Status.
    pub status: NativeStatus,
    /// `&&`/`||` short circuit: stop when the first operand equals this.
    pub short_circuit: Option<bool>,
    /// Implementation.
    pub f: NativeFn,
}

/// Registry of native implementations.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    defs: BTreeMap<String, NativeDef>,
}

impl Registry {
    /// Registry with the built-in natives.
    pub fn builtin() -> Self {
        let mut r = Self::default();
        for d in builtin_defs() {
            r.defs.insert(d.path.to_ascii_lowercase(), d);
        }
        r
    }

    /// Entry by lowercase `class.function`.
    pub fn get(&self, key: &str) -> Option<&NativeDef> {
        self.defs.get(key)
    }

    /// All entries.
    pub fn defs(&self) -> impl Iterator<Item = &NativeDef> {
        self.defs.values()
    }

    /// Adds or replaces an entry (tests, extensions).
    pub fn insert(&mut self, def: NativeDef) {
        self.defs.insert(def.path.to_ascii_lowercase(), def);
    }
}

fn type_err(vm: &Vm<'_>, expected: &'static str, v: &Value) -> crate::vm::VmError {
    vm.err(VmErrorKind::TypeMismatch {
        expected,
        found: v.type_name(),
    })
}

fn int(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<i32> {
    match a.get(i) {
        Some(Value::Int(v)) => Ok(*v),
        Some(Value::Byte(v)) => Ok(i32::from(*v)),
        Some(v) => Err(type_err(vm, "int", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn float(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<f32> {
    match a.get(i) {
        Some(Value::Float(v)) => Ok(*v),
        Some(v) => Err(type_err(vm, "float", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn boolean(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<bool> {
    match a.get(i) {
        Some(Value::Bool(v)) => Ok(*v),
        Some(v) => Err(type_err(vm, "bool", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn name(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<String> {
    match a.get(i) {
        Some(Value::Name(v)) => Ok(v.clone()),
        Some(v) => Err(type_err(vm, "name", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn byte(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<u8> {
    match a.get(i) {
        Some(Value::Byte(v)) => Ok(*v),
        Some(Value::Int(v)) => u8::try_from(*v).map_err(|_| {
            vm.err(VmErrorKind::TypeMismatch {
                expected: "byte 0..255",
                found: "int",
            })
        }),
        Some(v) => Err(type_err(vm, "byte", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn string(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<String> {
    match a.get(i) {
        Some(Value::Str(v)) => Ok(v.clone()),
        Some(v) => Err(type_err(vm, "string", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn object(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<Option<ObjRef>> {
    match a.get(i) {
        Some(Value::Object(o)) => Ok(*o),
        Some(v) => Err(type_err(vm, "object", v)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn val(v: Value) -> VmResult<NativeOutcome> {
    Ok(NativeOutcome::Value(v))
}

macro_rules! int2 {
    ($name:ident, $op:expr) => {
        fn $name(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
            let (x, y) = (int(vm, a, 0)?, int(vm, a, 1)?);
            let f: fn(i32, i32) -> Value = $op;
            val(f(x, y))
        }
    };
}

macro_rules! float2 {
    ($name:ident, $op:expr) => {
        fn $name(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
            let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
            let f: fn(f32, f32) -> Value = $op;
            val(f(x, y))
        }
    };
}

int2!(add_ii, |x, y| Value::Int(x.wrapping_add(y)));
int2!(sub_ii, |x, y| Value::Int(x.wrapping_sub(y)));
int2!(mul_ii, |x, y| Value::Int(x.wrapping_mul(y)));
int2!(lt_ii, |x, y| Value::Bool(x < y));
int2!(gt_ii, |x, y| Value::Bool(x > y));
int2!(le_ii, |x, y| Value::Bool(x <= y));
int2!(ge_ii, |x, y| Value::Bool(x >= y));
int2!(eq_ii, |x, y| Value::Bool(x == y));
int2!(ne_ii, |x, y| Value::Bool(x != y));
int2!(and_ii, |x, y| Value::Int(x & y));
float2!(add_ff, |x, y| Value::Float(x + y));
float2!(sub_ff, |x, y| Value::Float(x - y));
float2!(mul_ff, |x, y| Value::Float(x * y));
float2!(lt_ff, |x, y| Value::Bool(x < y));
float2!(gt_ff, |x, y| Value::Bool(x > y));
float2!(le_ff, |x, y| Value::Bool(x <= y));
float2!(ge_ff, |x, y| Value::Bool(x >= y));
float2!(eq_ff, |x, y| Value::Bool(x == y));
float2!(ne_ff, |x, y| Value::Bool(x != y));

/// Compound-assignment operator over two `int`s: the first argument is `out` in the decoded
/// declaration (`AddEqual_IntInt(int A, int B)`), so the new value is also written to `args[0]`,
/// which the VM stores back into the lvalue. The native returns the new value.
macro_rules! int_assign2 {
    ($name:ident, $op:expr) => {
        fn $name(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
            let (x, y) = (int(vm, a, 0)?, int(vm, a, 1)?);
            let f: fn(i32, i32) -> i32 = $op;
            let r = f(x, y);
            a[0] = Value::Int(r);
            val(Value::Int(r))
        }
    };
}

/// Compound-assignment operator over two `float`s (see [`int_assign2`]).
macro_rules! float_assign2 {
    ($name:ident, $op:expr) => {
        fn $name(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
            let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
            let f: fn(f32, f32) -> f32 = $op;
            let r = f(x, y);
            a[0] = Value::Float(r);
            val(Value::Float(r))
        }
    };
}

int_assign2!(add_eq_ii, |x, y| x.wrapping_add(y));
int_assign2!(sub_eq_ii, |x, y| x.wrapping_sub(y));
float_assign2!(add_eq_ff, |x, y| x + y);
float_assign2!(sub_eq_ff, |x, y| x - y);
float_assign2!(mul_eq_ff, |x, y| x * y);
float_assign2!(div_eq_ff, |x, y| x / y);

/// `int *= float` / `int /= float` (decoded `MultiplyEqual_IntFloat`/`DivideEqual_IntFloat`).
fn mul_eq_if(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = int(vm, a, 0)?;
    let y = float(vm, a, 1)?;
    let r = (x as f32 * y) as i32;
    a[0] = Value::Int(r);
    val(Value::Int(r))
}

fn div_eq_if(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = int(vm, a, 0)?;
    let y = float(vm, a, 1)?;
    if y == 0.0 {
        return Err(vm.err(VmErrorKind::DivisionByZero));
    }
    let r = (x as f32 / y) as i32;
    a[0] = Value::Int(r);
    val(Value::Int(r))
}

/// Compound vector assignment: `A += B` / `A -= B`, first argument `out`.
fn add_eq_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (vector2(vm, a, 0)?, vector2(vm, a, 1)?);
    let r = [x[0] + y[0], x[1] + y[1], x[2] + y[2]];
    a[0] = Value::Vector(r);
    val(Value::Vector(r))
}

fn sub_eq_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (vector2(vm, a, 0)?, vector2(vm, a, 1)?);
    let r = [x[0] - y[0], x[1] - y[1], x[2] - y[2]];
    a[0] = Value::Vector(r);
    val(Value::Vector(r))
}

fn mul_eq_vf(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = vector2(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    let r = [x[0] * s, x[1] * s, x[2] * s];
    a[0] = Value::Vector(r);
    val(Value::Vector(r))
}

fn div_eq_vf(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = vector2(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    if s == 0.0 {
        return Err(vm.err(VmErrorKind::DivisionByZero));
    }
    let r = [x[0] / s, x[1] / s, x[2] / s];
    a[0] = Value::Vector(r);
    val(Value::Vector(r))
}

fn mul_eq_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (vector2(vm, a, 0)?, vector2(vm, a, 1)?);
    let r = [x[0] * y[0], x[1] * y[1], x[2] * y[2]];
    a[0] = Value::Vector(r);
    val(Value::Vector(r))
}

/// Compound rotator assignment (`A += B` etc.), component-wise.
fn add_eq_rr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (rotator2(vm, a, 0)?, rotator2(vm, a, 1)?);
    let r = [
        x[0].wrapping_add(y[0]),
        x[1].wrapping_add(y[1]),
        x[2].wrapping_add(y[2]),
    ];
    a[0] = Value::Rotator(r);
    val(Value::Rotator(r))
}

fn sub_eq_rr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (rotator2(vm, a, 0)?, rotator2(vm, a, 1)?);
    let r = [
        x[0].wrapping_sub(y[0]),
        x[1].wrapping_sub(y[1]),
        x[2].wrapping_sub(y[2]),
    ];
    a[0] = Value::Rotator(r);
    val(Value::Rotator(r))
}

fn mul_eq_rf(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = rotator2(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    let r = [
        (x[0] as f32 * s) as i32,
        (x[1] as f32 * s) as i32,
        (x[2] as f32 * s) as i32,
    ];
    a[0] = Value::Rotator(r);
    val(Value::Rotator(r))
}

fn div_eq_rf(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = rotator2(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    if s == 0.0 {
        return Err(vm.err(VmErrorKind::DivisionByZero));
    }
    let r = [
        (x[0] as f32 / s) as i32,
        (x[1] as f32 / s) as i32,
        (x[2] as f32 / s) as i32,
    ];
    a[0] = Value::Rotator(r);
    val(Value::Rotator(r))
}

fn div_ii(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (int(vm, a, 0)?, int(vm, a, 1)?);
    if y == 0 {
        return Err(vm.err(VmErrorKind::DivisionByZero));
    }
    val(Value::Int(x.wrapping_div(y)))
}

fn div_ff(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    if y == 0.0 {
        return Err(vm.err(VmErrorKind::DivisionByZero));
    }
    val(Value::Float(x / y))
}

/// `Object.MultiplyMultiply_FloatFloat` (170): `A ** B`, UE2 `appPow` (C `pow`). Decoded call
/// site `xidcine.HelicoDeco.HelicoTick` 0x01FF (the exponential damping factor). A negative base
/// with a non-integral exponent yields `NaN`, matching `pow` (never silently clamped).
fn pow_ff(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    val(Value::Float(x.powf(y)))
}

/// `Object.Percent_FloatFloat` (173): `A % B`, UE2 `appFmod`/C `fmod` (remainder keeps the
/// dividend's sign). Decoded call site `xiii.m60.RumbleFX` 0x004E (`ReloadCount % 1`), reached by
/// the m60/kalash/m16 fire path. A zero divisor yields `NaN`, never a silent clamp.
fn percent_ff(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    val(Value::Float(x % y))
}

fn not_b(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(!boolean(vm, a, 0)?))
}

fn and_bb(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(boolean(vm, a, 0)? && boolean(vm, a, 1)?))
}

fn or_bb(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(boolean(vm, a, 0)? || boolean(vm, a, 1)?))
}

fn eq_bb(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(boolean(vm, a, 0)? == boolean(vm, a, 1)?))
}

fn ne_bb(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(boolean(vm, a, 0)? != boolean(vm, a, 1)?))
}

fn neg_i(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Int(int(vm, a, 0)?.wrapping_neg()))
}

fn neg_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(-float(vm, a, 0)?))
}

/// `++A` / `A++` / `--A` / `A--` on an `out int`.
fn incdec(vm: &Vm<'_>, a: &mut [Value], delta: i32, post: bool) -> VmResult<NativeOutcome> {
    let old = int(vm, a, 0)?;
    let new = old.wrapping_add(delta);
    a[0] = Value::Int(new);
    val(Value::Int(if post { old } else { new }))
}

fn preinc_i(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    incdec(vm, a, 1, false)
}
fn postinc_i(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    incdec(vm, a, 1, true)
}
fn predec_i(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    incdec(vm, a, -1, false)
}
fn postdec_i(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    incdec(vm, a, -1, true)
}

fn eq_nn(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(
        name(vm, a, 0)?.eq_ignore_ascii_case(&name(vm, a, 1)?),
    ))
}

fn ne_nn(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(
        !name(vm, a, 0)?.eq_ignore_ascii_case(&name(vm, a, 1)?),
    ))
}

fn eq_oo(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(object(vm, a, 0)? == object(vm, a, 1)?))
}

fn ne_oo(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(object(vm, a, 0)? != object(vm, a, 1)?))
}

fn concat_ss(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Str(string(vm, a, 0)? + &string(vm, a, 1)?))
}

fn at_ss(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Str(format!(
        "{} {}",
        string(vm, a, 0)?,
        string(vm, a, 1)?
    )))
}

fn left_ss(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // `Left(S,i)` = S.substr(0, i); i is clamped as an unsigned index to [0, Len].
    let s = string(vm, a, 0)?;
    let n = clamp_len(int(vm, a, 1)?, s.chars().count());
    let s: String = s.chars().take(n).collect();
    val(Value::Str(s))
}

fn right_ss(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // `Right(S,i)` = S.substr(max(Len-i,0), i); i is clamped to [0, Len].
    let s = string(vm, a, 0)?;
    let len = s.chars().count();
    let n = clamp_len(int(vm, a, 1)?, len);
    let s: String = s.chars().skip(len - n).collect();
    val(Value::Str(s))
}

fn mid_ssi(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // `Mid(S,i,j)` = S.substr(i, j): the start and the end (`i + j`) are clamped as *unsigned*
    // indices to [0, Len]. A negative index therefore clamps to Len, so a negative start yields
    // "" (reviewer contract; see the report). `j` omitted means "the rest of the string".
    let s = string(vm, a, 0)?;
    let len = s.chars().count();
    let raw_i = int(vm, a, 1)?;
    let start = clamp_unsigned(raw_i, len);
    let end = if c.omitted(2) {
        len
    } else {
        clamp_unsigned(raw_i.wrapping_add(int(vm, a, 2)?), len)
    };
    let s: String = s
        .chars()
        .skip(start)
        .take(end.saturating_sub(start))
        .collect();
    val(Value::Str(s))
}

fn len_s(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Int(string(vm, a, 0)?.chars().count() as i32))
}

fn instr_ss(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `execInStr` uses `appStrstr` (a case-sensitive `wcsstr`) and returns -1 when absent.
    // The VM strings are char-based; a case-sensitive byte/char search matches the ASCII script
    // data in the corpus.
    let s = string(vm, a, 0)?;
    let t = string(vm, a, 1)?;
    let r = s.find(&t).map_or(-1, |i| i as i32);
    val(Value::Int(r))
}

/// Clamps a requested count to `[0, len]` (UE2 `Left`/`Right`).
fn clamp_len(n: i32, len: usize) -> usize {
    n.clamp(0, len as i32) as usize
}

/// Clamps a string index as an *unsigned* quantity to `[0, len]`: a negative index wraps to a
/// large value and therefore clamps to `len` (UE2 `FString::Mid`; a negative start yields "").
fn clamp_unsigned(v: i32, len: usize) -> usize {
    if v < 0 { len } else { (v as usize).min(len) }
}

fn caps_s(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Str(string(vm, a, 0)?.to_ascii_uppercase()))
}

/// `Object.Localize(string SectionName, string KeyName, string PackageName)` (native 199).
///
/// Measured (`Core.dll` `?execLocalize@UObject` RVA `0x1DD40` -> `Localize` RVA `0x281A0`): the
/// engine looks the key up in the package's active-language `.int`, falling back as configured,
/// and on a miss returns the literal `"<?%s?%s.%s.%s?>"` formatted with the active language,
/// package, section and key (observed format bytes at `0x10179574`), logging
/// `"No localization for ..."`. The host provider owns the file lookup and fallback; the VM
/// builds the placeholder from the provider's language. Without a provider the call fails
/// explicitly.
fn localize_native(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let section = string(vm, a, 0)?;
    let key = string(vm, a, 1)?;
    let package = string(vm, a, 2)?;
    let value = vm
        .localize_or_placeholder(&package, &section, &key)
        .ok_or_else(|| {
            vm.err(VmErrorKind::NoLocalizationProvider {
                native: "Object.Localize".to_owned(),
            })
        })?;
    val(Value::Str(value))
}

fn class_is_child_of(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let test = class_ref(vm, a, 0)?;
    let parent = class_ref(vm, a, 1)?;
    let r = match (test, parent) {
        (Some(t), Some(p)) => vm.is_child_of_class(t, p),
        _ => false,
    };
    val(Value::Bool(r))
}

fn class_ref(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<Option<GlobalRef>> {
    match a.get(i) {
        Some(Value::Object(Some(ObjRef::Static(g)))) => Ok(Some(*g)),
        _ => {
            let _ = vm;
            Ok(None)
        }
    }
}

/// Native engine-class base relationships for `DynamicLoadObject`'s requested-class check.
///
/// The decoded packages carry no `Core.Class` export for the engine mesh classes (measured:
/// `engine.u` exports `Actor`, `Texture`, `LevelInfo`, `MeshSkinList`, ... but **not**
/// `Mesh`/`SkeletalMesh`/`StaticMesh`), so their inheritance is not available from the corpus.
/// These are the UE2 `Engine` relationships needed by the retail
/// call sites: `Weapon.PostBeginPlay` loads a `SkeletalMesh` with `class'Engine.Mesh'`
/// (`XIII_Game/system/engine.u`, `Weapon.PostBeginPlay` token `DynamicLoadObject(MeshName,
/// class'Engine.Mesh')`), and `USkeletalMesh`/`UStaticMesh` derive from `UMesh` upstream.
/// `(subclass, base)`, lowercase leaf names.
const NATIVE_CLASS_BASES: &[(&str, &str)] = &[
    ("skeletalmesh", "mesh"),
    ("staticmesh", "mesh"),
    ("mesh", "primitive"),
    ("skeletalmeshinstance", "meshinstance"),
    ("staticmeshinstance", "meshinstance"),
    ("meshinstance", "primitive"),
];

/// True when native class `actual` is `requested` or derives from it under
/// [`NATIVE_CLASS_BASES`]. Both are `Package.Object` paths or bare leaf names; only the leaf is
/// compared. The walk is bounded so an accidental cycle cannot loop.
pub(crate) fn native_class_is_a(actual: &str, requested: &str) -> bool {
    let leaf = |p: &str| p.rsplit('.').next().unwrap_or(p).to_ascii_lowercase();
    let actual = leaf(actual);
    let requested = leaf(requested);
    if actual == requested {
        return true;
    }
    let mut cur = actual.as_str();
    for _ in 0..16 {
        let Some((_, base)) = NATIVE_CLASS_BASES.iter().find(|(sub, _)| *sub == cur) else {
            return false;
        };
        if *base == requested {
            return true;
        }
        cur = base;
    }
    false
}

fn dynamic_load_object(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = match a.first() {
        Some(Value::Str(s)) | Some(Value::Name(s)) => s.clone(),
        _ => return val(Value::Object(None)),
    };
    // UE2 `execDynamicLoadObject`: resolve the name, then require the loaded object's class to
    // be `ObjectClass` or a subclass, else the load fails (NULL). This matters because e.g.
    // `DynamicLoadObject(MeshName, class'Engine.Mesh')` must accept a `SkeletalMesh` (the
    // retail `Weapon.PostBeginPlay` path) but must not return a non-Mesh object.
    let requested = match a.get(1) {
        Some(Value::NativeClass(n)) => Some(n.clone()),
        Some(Value::Object(Some(ObjRef::Instance(i)))) => {
            Some(vm.short_path(vm.objects[*i as usize].class))
        }
        // A class argument (`class'Engine.Texture'` / `obj'Engine.Texture'`) is itself a class
        // object whose `class_path_of` is `Core.Class`; the requested class is the class's own
        // path (`Engine.Texture`). A non-class static object uses its class path.
        Some(Value::Object(Some(ObjRef::Static(g)))) => {
            if matches!(
                vm.set().object(*g),
                Some(crate::reflect::ScriptObject::Class(_))
            ) {
                Some(vm.set().path(*g))
            } else {
                vm.class_path_of(*g)
            }
        }
        _ => None,
    };
    match vm.find_loaded_object(&name) {
        Some(g) => {
            if let Some(req) = &requested {
                // The VM has no runtime class hierarchy for arbitrary natives; the small
                // native table above covers the engine mesh classes, and everything else is an
                // exact leaf match.
                let actual = vm.class_path_of(g);
                let ok = actual.as_deref().is_some_and(|a| native_class_is_a(a, req));
                if !ok {
                    vm.note(TraceKind::Note(format!(
                        "DynamicLoadObject: {name} is not a {req} (class {})",
                        actual.as_deref().unwrap_or("?")
                    )));
                    return val(Value::Object(None));
                }
            }
            val(Value::Object(Some(ObjRef::Static(g))))
        }
        None => {
            // Non-script packages registered by the runtime (`.utx` textures, `.usx` meshes,
            // `.uax` sounds) are not part of the loaded script set, so `find_loaded_object`
            // misses them. `XIDInterf.XIIIMenu.Created` loads its button/background textures
            // this way (`DynamicLoadObject("XIIIMenuStart.continue01gris",
            // class'Engine.Texture')`); resolve them to an `ObjRef::External` so the external
            // property provider answers `USize`/`VSize` and the Canvas sees the texture path.
            match vm.set().external_lookup(&name) {
                crate::linker::ExternalLookup::Found(class) => {
                    if let Some(req) = &requested
                        && !native_class_is_a(&class, req)
                    {
                        vm.note(TraceKind::Note(format!(
                            "DynamicLoadObject: {name} is not a {req} (class {class})"
                        )));
                        return val(Value::Object(None));
                    }
                    let id = vm.intern_external(&name, Some(class));
                    val(Value::Object(Some(ObjRef::External(id))))
                }
                crate::linker::ExternalLookup::MissingPackage
                | crate::linker::ExternalLookup::MissingExport => {
                    vm.note(TraceKind::Note(format!(
                        "DynamicLoadObject: {name} not found in its package"
                    )));
                    val(Value::Object(None))
                }
                crate::linker::ExternalLookup::Unknown => val(Value::Object(None)),
            }
        }
    }
}

fn fmax_ff(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    val(Value::Float(if x >= y { x } else { y }))
}

fn fmin_ff(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (float(vm, a, 0)?, float(vm, a, 1)?);
    val(Value::Float(if x <= y { x } else { y }))
}

fn fclamp_fff(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = float(vm, a, 0)?;
    let lo = float(vm, a, 1)?;
    let hi = float(vm, a, 2)?;
    val(Value::Float(v.clamp(lo.min(hi), hi.max(lo))))
}

fn min_ii(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Int(int(vm, a, 0)?.min(int(vm, a, 1)?)))
}

fn max_ii(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Int(int(vm, a, 0)?.max(int(vm, a, 1)?)))
}

fn lerp_fff(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let alpha = float(vm, a, 0)?;
    let v0 = float(vm, a, 1)?;
    let v1 = float(vm, a, 2)?;
    val(Value::Float(v0 + (v1 - v0) * alpha))
}

fn abs_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(float(vm, a, 0)?.abs()))
}

fn sqrt_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(float(vm, a, 0)?.sqrt()))
}

fn exp_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(float(vm, a, 0)?.exp()))
}

fn loge_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(float(vm, a, 0)?.ln()))
}

fn sin_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(float(vm, a, 0)?.sin()))
}

fn cos_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(float(vm, a, 0)?.cos()))
}

fn tan_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(float(vm, a, 0)?.tan()))
}

fn atan_f(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(float(vm, a, 0)?.atan()))
}

fn complement_equal_ss(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UnrealScript `~=` is the case-insensitive *equality* (`appStricmp(A,B) == 0`), the same
    // result as `==` on strings.
    val(Value::Bool(
        string(vm, a, 0)?.eq_ignore_ascii_case(&string(vm, a, 1)?),
    ))
}

fn clamp_iii(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = int(vm, a, 0)?;
    let lo = int(vm, a, 1)?;
    let hi = int(vm, a, 2)?;
    val(Value::Int(v.clamp(lo.min(hi), hi.max(lo))))
}

fn multiply_fv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = float(vm, a, 0)?;
    match a.get(1) {
        Some(Value::Vector(v)) => val(Value::Vector([v[0] * s, v[1] * s, v[2] * s])),
        Some(other) => Err(type_err(vm, "vector", other)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn set_rotation(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let r = match a.first() {
        Some(Value::Rotator(r)) => *r,
        Some(other) => return Err(type_err(vm, "rotator", other)),
        None => return Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    };
    vm.set_property(c.this, "Rotation", 0, Value::Rotator(r));
    val(Value::Bool(true))
}

fn f_rand(vm: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Float(vm.rand_float()))
}

fn rand_i(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let max = int(vm, a, 0)?;
    val(Value::Int(vm.rand_int(max)))
}

fn vector2(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<[f32; 3]> {
    match a.get(i) {
        Some(Value::Vector(v)) => Ok(*v),
        Some(other) => Err(type_err(vm, "vector", other)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn rotator2(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<[i32; 3]> {
    match a.get(i) {
        Some(Value::Rotator(r)) => Ok(*r),
        Some(other) => Err(type_err(vm, "rotator", other)),
        None => Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    }
}

fn add_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (vector2(vm, a, 0)?, vector2(vm, a, 1)?);
    val(Value::Vector([x[0] + y[0], x[1] + y[1], x[2] + y[2]]))
}

fn sub_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (vector2(vm, a, 0)?, vector2(vm, a, 1)?);
    val(Value::Vector([x[0] - y[0], x[1] - y[1], x[2] - y[2]]))
}

fn mul_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (vector2(vm, a, 0)?, vector2(vm, a, 1)?);
    val(Value::Vector([x[0] * y[0], x[1] * y[1], x[2] * y[2]]))
}

/// `Object.Multiply_VectorFloat` (212): componentwise `vector * float`.
fn mul_vf(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = vector2(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    val(Value::Vector([v[0] * s, v[1] * s, v[2] * s]))
}

/// `Object.Divide_VectorFloat` (214): componentwise `vector / float` (UE2
/// `operator/(FVector, FLOAT)`). Division by zero yields IEEE infinities/NaN, as in UE2 (not
/// silently clamped).
fn div_vf(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = vector2(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    val(Value::Vector([v[0] / s, v[1] / s, v[2] / s]))
}

/// `Object.EqualEqual_VectorVector` (217): exact componentwise equality (UE2 `FVector::operator==`).
fn eq_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (vector2(vm, a, 0)?, vector2(vm, a, 1)?);
    val(Value::Bool(x == y))
}

fn dot_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (vector2(vm, a, 0)?, vector2(vm, a, 1)?);
    val(Value::Float(x[0] * y[0] + x[1] * y[1] + x[2] * y[2]))
}

fn vsize_v(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = vector2(vm, a, 0)?;
    val(Value::Float(
        (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt(),
    ))
}

fn normal_v(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = vector2(vm, a, 0)?;
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    let r = if len > 0.0 {
        [v[0] / len, v[1] / len, v[2] / len]
    } else {
        [0.0, 0.0, 0.0]
    };
    val(Value::Vector(r))
}

/// Unreal rotator (`pitch, yaw, roll`; 65536 per turn) as its orthonormal basis axes
/// (X forward, Y right, Z up), matching `FRotationMatrix`.
pub(crate) fn rotator_basis(r: [i32; 3]) -> ([f32; 3], [f32; 3], [f32; 3]) {
    let to_rad = |u: i32| (u as f32) * std::f32::consts::TAU / 65536.0;
    let (p, y, rl) = (to_rad(r[0]), to_rad(r[1]), to_rad(r[2]));
    let (sp, cp) = (p.sin(), p.cos());
    let (sy, cy) = (y.sin(), y.cos());
    let (sr, cr) = (rl.sin(), rl.cos());
    (
        [cp * cy, cp * sy, sp],
        [sr * sp * cy - cr * sy, sr * sp * sy + cr * cy, -sr * cp],
        [-(cr * sp * cy + sr * sy), cy * sr - cr * sp * sy, cr * cp],
    )
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

/// `vector >> rotator` (276): rotate a local vector into world space (`FRotationMatrix`
/// transform). Evidence: `xidcine.HelicoDeco.PostBeginPlay` uses it for part offsets.
fn greater_greater_vr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = vector2(vm, a, 0)?;
    let r = rotator2(vm, a, 1)?;
    let (x, y, z) = rotator_basis(r);
    val(Value::Vector([
        v[0] * x[0] + v[1] * y[0] + v[2] * z[0],
        v[0] * x[1] + v[1] * y[1] + v[2] * z[1],
        v[0] * x[2] + v[1] * y[2] + v[2] * z[2],
    ]))
}

/// `vector << rotator` (275): rotate a world vector into local space (inverse transform).
/// Evidence: `xidcine.HelicoDeco.PostBeginPlay` uses it to get a local position offset.
fn less_less_vr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = vector2(vm, a, 0)?;
    let r = rotator2(vm, a, 1)?;
    let (x, y, z) = rotator_basis(r);
    val(Value::Vector([dot3(v, x), dot3(v, y), dot3(v, z)]))
}

fn set_physics(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let p = match a.first() {
        Some(Value::Byte(b)) => *b,
        Some(Value::Int(i)) => *i as u8,
        Some(other) => return Err(type_err(vm, "byte", other)),
        None => return Err(vm.err(VmErrorKind::Other("missing argument".into()))),
    };
    vm.set_property(c.this, "Physics", 0, Value::Byte(p));
    val(Value::Void)
}

fn add_rr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (rotator2(vm, a, 0)?, rotator2(vm, a, 1)?);
    val(Value::Rotator([
        x[0].wrapping_add(y[0]),
        x[1].wrapping_add(y[1]),
        x[2].wrapping_add(y[2]),
    ]))
}

fn sub_rr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (rotator2(vm, a, 0)?, rotator2(vm, a, 1)?);
    val(Value::Rotator([
        x[0].wrapping_sub(y[0]),
        x[1].wrapping_sub(y[1]),
        x[2].wrapping_sub(y[2]),
    ]))
}

/// `Object.Multiply_RotatorFloat` (287): componentwise `rotator * float`. The decoded call
/// site `xidcine.HelicoDeco.HelicoTick` 0x01F4 scales the helicopter's rotation by
/// `1 - exp(-inertia * dt)`. Same conversion as the existing compound `*=` (`mul_eq_rf`):
/// each component is multiplied in `f32` and truncated toward zero (hypothesis: UE2's
/// `FRotator` scalar operator converts with a plain C cast; the compound operator in this
/// registry already does this).
fn mul_rf(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = rotator2(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    val(Value::Rotator([
        (x[0] as f32 * s) as i32,
        (x[1] as f32 * s) as i32,
        (x[2] as f32 * s) as i32,
    ]))
}

/// `Object.Multiply_FloatRotator` (288): the reversed operand order of [`mul_rf`].
fn mul_fr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = float(vm, a, 0)?;
    let x = rotator2(vm, a, 1)?;
    val(Value::Rotator([
        (x[0] as f32 * s) as i32,
        (x[1] as f32 * s) as i32,
        (x[2] as f32 * s) as i32,
    ]))
}

/// `Object.Divide_RotatorFloat` (289): componentwise `rotator / float`; a zero divisor is an
/// explicit error, matching the compound `div_eq_rf`.
fn div_rf(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = rotator2(vm, a, 0)?;
    let s = float(vm, a, 1)?;
    if s == 0.0 {
        return Err(vm.err(VmErrorKind::DivisionByZero));
    }
    val(Value::Rotator([
        (x[0] as f32 / s) as i32,
        (x[1] as f32 / s) as i32,
        (x[2] as f32 / s) as i32,
    ]))
}

/// `Object.EqualEqual_RotatorRotator` (142): exact componentwise equality. Decoded call site
/// `xiii.MitraillTop.GoToWaitingPos.Tick` 0x0035 compares the remembered `OldRotation` with
/// the current `Rotation` (UE2 `FRotator::operator==` compares the three integer components).
fn eq_rr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (rotator2(vm, a, 0)?, rotator2(vm, a, 1)?);
    val(Value::Bool(x == y))
}

/// `Object.NotEqual_RotatorRotator` (203): the negation of [`eq_rr`].
fn ne_rr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let (x, y) = (rotator2(vm, a, 0)?, rotator2(vm, a, 1)?);
    val(Value::Bool(x != y))
}

fn all_actors(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => Some(g),
        None => None,
        Some(ObjRef::Instance(_)) | Some(ObjRef::External(_)) => {
            return Err(vm.err(VmErrorKind::Other(
                "AllActors base class is an instance".into(),
            )));
        }
    };
    let tag = match a.get(2) {
        Some(Value::Name(n)) if !n.eq_ignore_ascii_case("None") => Some(n.clone()),
        _ => None,
    };
    let items: Vec<Value> = vm
        .all_actors(base, tag.as_deref())
        .into_iter()
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(
        items.into_iter().map(|v| vec![v]).collect(),
    ))
}

/// `Actor.CollidingActors` (native 321): actors of `BaseClass` near the caller. **Partial**: the
/// VM uses the same distance filter as `RadiusActors` (its collision cylinders are not swept);
/// `XIIIMover.Timer` re-checks `FastTrace`/vision on each result, so this is sufficient for the
/// door-warning timer and keeps a mover from being suspended on its own timer.
fn colliding_actors(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => Some(g),
        None => None,
        Some(ObjRef::Instance(_)) | Some(ObjRef::External(_)) => {
            return Err(vm.err(VmErrorKind::Other(
                "CollidingActors base class is an instance".into(),
            )));
        }
    };
    let radius = float(vm, a, 2)?;
    let loc = if c.omitted(3) {
        vm.vector_prop(c.this, "Location").unwrap_or([0.0; 3])
    } else {
        vector2(vm, a, 3)?
    };
    let items: Vec<Value> = vm
        .radius_actors(base, radius, loc)
        .into_iter()
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(
        items.into_iter().map(|v| vec![v]).collect(),
    ))
}

fn radius_actors(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => Some(g),
        None => None,
        Some(ObjRef::Instance(_)) | Some(ObjRef::External(_)) => {
            return Err(vm.err(VmErrorKind::Other(
                "RadiusActors base class is an instance".into(),
            )));
        }
    };
    let radius = float(vm, a, 2)?;
    let loc = if c.omitted(3) {
        vm.vector_prop(c.this, "Location").unwrap_or([0.0; 3])
    } else {
        vector2(vm, a, 3)?
    };
    let items: Vec<Value> = vm
        .radius_actors(base, radius, loc)
        .into_iter()
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(
        items.into_iter().map(|v| vec![v]).collect(),
    ))
}

fn eq_ss(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 string == is case-insensitive.
    val(Value::Bool(
        string(vm, a, 0)?.eq_ignore_ascii_case(&string(vm, a, 1)?),
    ))
}

fn ne_ss(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(
        !string(vm, a, 0)?.eq_ignore_ascii_case(&string(vm, a, 1)?),
    ))
}

fn goto_state(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `GotoState(, Label)` means continue in the current state at Label, not GotoState(None).
    // `BeachInBedWithXIII.Waiting.Timer` uses `GotoState(, 'blink')`; treating the omitted first
    // argument as the name `None` exited Waiting, permanently skipping its Tick/GetUpStandUp
    // release path on Plage01.
    let state = if c.omitted(0) {
        vm.state_name(c.this).unwrap_or_else(|| "None".to_owned())
    } else {
        name(vm, a, 0)?
    };
    let label = match a.get(1) {
        Some(Value::Name(l)) if !l.eq_ignore_ascii_case("None") => l.clone(),
        _ => "Begin".to_owned(),
    };
    vm.do_goto_state(c.this, &state, &label)?;
    val(Value::Void)
}

fn is_a(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let n = name(vm, a, 0)?;
    val(Value::Bool(vm.is_a(c.this, &n)))
}

fn is_in_state(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let n = name(vm, a, 0)?;
    val(Value::Bool(vm.is_in_state(c.this, &n)))
}

fn get_state_name(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Name(
        vm.state_name(c.this).unwrap_or_else(|| "None".to_owned()),
    ))
}

fn disable(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let n = name(vm, a, 0)?;
    vm.disable_probe(c.this, &n, true);
    val(Value::Void)
}

fn enable(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let n = name(vm, a, 0)?;
    vm.disable_probe(c.this, &n, false);
    val(Value::Void)
}

fn log(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = string(vm, a, 0)?;
    vm.note(TraceKind::Log(s));
    val(Value::Void)
}

fn sleep(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let seconds = float(vm, a, 0)?;
    if !c.in_state_code {
        return Err(vm.err(VmErrorKind::LatentOutsideState {
            path: c.path.clone(),
        }));
    }
    vm.pending_latent = Some(Latent::Sleep {
        seconds,
        remaining: seconds,
        started: vm.time_now(),
    });
    val(Value::Void)
}

fn set_timer(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let rate = float(vm, a, 0)?;
    let repeat = boolean(vm, a, 1)?;
    vm.set_timer(c.this, rate, repeat);
    val(Value::Void)
}

/// `Actor.FinishInterpolation` (native 301): latent; suspends the state code of a `Mover` until
/// its `bInterpolating` flag clears. The per-tick `PHYS_MovingBrush` advance that clears it is
/// [`crate::vm::Vm::advance_interpolation`]. Evidence: every `Engine.Mover` open/close state
/// (`OpenTimedMover`, `TriggerToggle`, `TriggerControl`, `BumpOpenTimed`, `BumpButton`,
/// `TriggerPound`) calls it immediately after `DoOpen`/`DoClose` and expects to resume when the
/// brush reaches its key; the Plage01 item8a run stopped at this native (#301).
fn finish_interpolation(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _: &mut [Value],
) -> VmResult<NativeOutcome> {
    if !c.in_state_code {
        return Err(vm.err(VmErrorKind::LatentOutsideState {
            path: c.path.clone(),
        }));
    }
    vm.pending_latent = Some(Latent::Interp {
        started: vm.time_now(),
    });
    val(Value::Void)
}

/// `Object.VRand`: a unit vector with an approximately uniform direction (upstream uses the
/// engine RNG; the VM's deterministic PRNG is used instead — the exact sequence is not
/// reproduced).
fn vrand(vm: &mut Vm<'_>, _c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let u1 = vm.rand_float().clamp(0.0, 1.0);
    let u2 = vm.rand_float().clamp(0.0, 1.0);
    let z = 2.0 * u1 - 1.0;
    let r = (1.0 - z * z).max(0.0).sqrt();
    let theta = std::f32::consts::TAU * u2;
    val(Value::Vector([r * theta.cos(), r * theta.sin(), z]))
}

/// `Actor.SetTimer2`: the decoded declaration is identical to `SetTimer` (`float`, `bool`), so
/// the same timer semantics are used (hypothesis for XIII; the second timer's role is not
/// established without DLL disassembly).
fn set_timer2(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let rate = float(vm, a, 0)?;
    let repeat = boolean(vm, a, 1)?;
    vm.set_timer_named(c.this, rate, repeat, "Timer2");
    val(Value::Void)
}

/// `Controller.SetTimer3(float NewTimerRate, bool bLoop)`: like `SetTimer` but dispatches the
/// `Timer3` event. Used by `IAController.Attaque` (enemy-position refresh) and other combat states.
fn set_timer3(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let rate = float(vm, a, 0)?;
    let repeat = boolean(vm, a, 1)?;
    vm.set_timer_named(c.this, rate, repeat, "Timer3");
    val(Value::Void)
}

fn object_out(id: Option<ObjectId>) -> Value {
    match id {
        Some(id) => Value::Object(Some(ObjRef::Instance(id))),
        None => Value::Object(None),
    }
}

fn find_path_toward(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !vm.navigation_ready(
        "Controller.FindPathToward",
        None,
        c.this,
        Value::Object(None),
    )? {
        return val(Value::Object(None));
    }
    let Some(target) = instance_arg(vm, a, 0)? else {
        return val(Value::Object(None));
    };
    let goal = vm.vector_prop(target, "Location").unwrap_or([0.0; 3]);
    val(object_out(vm.nav_find_path_to(c.this, goal)?))
}

fn find_path_to(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !vm.navigation_ready("Controller.FindPathTo", None, c.this, Value::Object(None))? {
        return val(Value::Object(None));
    }
    let goal = vector2(vm, a, 0)?;
    val(object_out(vm.nav_find_path_to(c.this, goal)?))
}

/// `IAController.FindBestPathToward(Actor Desired, float xyMargin, float heightMargin) -> bool`.
///
/// Disassembly evidence (XIDPawn.dll `?execFindBestPathToward@AIAController` RVA 0x1AB0): it reads
/// `Desired` (default `None`), `xyMargin` (default `70.0`) and `heightMargin` (default `160.0`),
/// returns false when `Desired == None`, otherwise runs the engine's A* (`FindBestPathTo`) from the
/// controller pawn to `Desired.Location` and, on success, writes the first path node at
/// `IAController+0x228` and its location at `+0x230`. The headless VM reuses the decoded-ReachSpec
/// path (`Vm::nav_find_path_to`, which also fills `RouteCache`/`RouteDist`) and returns whether a
/// path was found. The `xyMargin`/`heightMargin` goal tolerance is accepted but the nav provider
/// already chooses the nearest reachable node to the goal location.
fn find_best_path_toward(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let Some(target) = instance_arg(vm, a, 0)? else {
        return val(Value::Bool(false));
    };
    let goal = vm.vector_prop(target, "Location").unwrap_or([0.0; 3]);
    let first = vm.nav_find_path_to(c.this, goal)?;
    val(Value::Bool(first.is_some()))
}

fn find_random_dest(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !vm.navigation_ready(
        "Controller.FindRandomDest",
        None,
        c.this,
        Value::Object(None),
    )? {
        return val(Value::Object(None));
    }
    val(object_out(vm.nav_find_random_dest(c.this)?))
}

fn point_reachable(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let point = vector2(vm, a, 0)?;
    val(Value::Bool(vm.nav_point_reachable(c.this, point)?))
}

fn actor_reachable(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(other) = instance_arg(vm, a, 0)? else {
        return val(Value::Bool(false));
    };
    val(Value::Bool(vm.nav_actor_reachable(c.this, other)?))
}

fn line_of_sight_to(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(other) = instance_arg(vm, a, 0)? else {
        return val(Value::Bool(false));
    };
    val(Value::Bool(vm.nav_line_of_sight_to(c.this, other)?))
}

/// `Controller.CanSee(Pawn Other) -> bool` (native 533): Engine.dll `execCanSee` (VA 0x1036f070)
/// calls `AController::SeePawn(Other, 0)` (VA 0x1036dc40) — LOS-only for the controller's `Enemy`,
/// otherwise the SightRadius/Visibility range gate and the decoded `|delta| > PeripheralVision`
/// check on top of `LineOfSightTo(Other, 0)`.
fn controller_can_see(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(other) = instance_arg(vm, a, 0)? else {
        return val(Value::Bool(false));
    };
    val(Value::Bool(vm.nav_can_see(c.this, other)?))
}

/// `Pawn.PressingFire() -> bool` (native 0): Engine.dll `APawn::execPressingFire` returns false
/// without a controller and otherwise reads the controller's `bFire` field. It does not inspect
/// the pawn, instigator, or XIII's script-only `IAController.bTire` field.
fn pawn_pressing_fire(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let fire = vm
        .obj_prop(c.this, "Controller")
        .is_some_and(|ctrl| vm.bool_prop(ctrl, "bFire"));
    val(Value::Bool(fire))
}

/// `Actor.PlayerCanSeeMe() -> bool`.
///
/// Disassembly evidence (Engine.dll `?execPlayerCanSeeMe@AActor` RVA 0xB3850): for standalone/client
/// net modes it takes the render-time fallback (`Level.TimeSeconds - LastRenderTime` against 0.0);
/// otherwise it walks `Level.ControllerList` and returns true when `TestCanSeeMe(this, controller)`
/// holds for any player controller (`?TestCanSeeMe@AActor` RVA 0xB0A60). The headless VM has no
/// renderer, so when a physics provider is installed it uses the controller path's intent — a clear
/// line trace from a player pawn's eye to this actor — and otherwise reports false with a visible
/// note (never a silent success). The render-time fallback is not reproduced.
fn player_can_see_me(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(target) = vm.vector_prop(c.this, "Location") else {
        return val(Value::Bool(false));
    };
    let mut controllers = Vec::new();
    for i in 0..vm.objects.len() {
        let id = i as ObjectId;
        if vm.objects[i].deleted || !vm.objects[i].is_actor {
            continue;
        }
        if vm.is_a(id, "PlayerController") {
            controllers.push(id);
        }
    }
    if controllers.is_empty() {
        return val(Value::Bool(false));
    }
    if vm.physics.is_none() {
        vm.note(TraceKind::Note(
            "PlayerCanSeeMe: no physics provider, visibility not evaluated (false)".into(),
        ));
        return val(Value::Bool(false));
    }
    for pc in controllers {
        let Some(pawn) = vm.obj_prop(pc, "Pawn") else {
            continue;
        };
        let loc = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let eye = [loc[0], loc[1], loc[2] + vm.f32_prop(pawn, "BaseEyeHeight")];
        if vm
            .physics
            .as_mut()
            .and_then(|p| p.trace(eye, target, [0.0; 3]))
            .is_none()
        {
            return val(Value::Bool(true));
        }
    }
    val(Value::Bool(false))
}

/// `PositionInfo.AutoPosition()` (xidcine).
///
/// Disassembly evidence (XIDCine.dll `?execAutoPosition@APositionInfo` RVA 0x1990): it takes
/// `Location.Z - 1000` (the literal at VA 0x100056C8), line-checks the level upward to the actor's
/// `Location`, copies the hit point back into `Location`, then adds `fAltitude` (default 178, the
/// `PositionInfo` default) to `Location.Z`. So the anchor is snapped to the first surface between
/// 1000 UU below it and its current position, then floated `fAltitude` above that surface. The
/// headless VM performs that trace through the physics provider; without one the call fails
/// explicitly (it is not a silent no-op).
fn auto_position(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !vm.physics_ready("PositionInfo.AutoPosition", None, c.this, Value::Void)? {
        return val(Value::Void);
    }
    let Some(loc) = vm.vector_prop(c.this, "Location") else {
        return val(Value::Void);
    };
    let altitude = vm.f32_prop(c.this, "fAltitude");
    let start = [loc[0], loc[1], loc[2] - 1000.0];
    let end = loc;
    if let Some(hit) = vm
        .physics
        .as_mut()
        .and_then(|p| p.trace(start, end, [0.0; 3]))
    {
        vm.set_property(
            c.this,
            "Location",
            0,
            Value::Vector([end[0], end[1], hit.location[2] + altitude]),
        );
    }
    val(Value::Void)
}

fn move_to(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let destination = vector2(vm, a, 0)?;
    let speed = if c.omitted(2) { 1.0 } else { float(vm, a, 2)? };
    let focus = if c.omitted(1) {
        None
    } else {
        instance_arg(vm, a, 1)?
    };
    vm.set_property(
        c.this,
        "Focus",
        0,
        Value::Object(focus.map(ObjRef::Instance)),
    );
    if focus.is_none() {
        vm.set_property(c.this, "FocalPoint", 0, Value::Vector(destination));
    }
    vm.set_property(c.this, "MoveTarget", 0, Value::Object(None));
    vm.start_move(
        c.this,
        destination,
        speed,
        "Controller.MoveTo",
        c.in_state_code,
    )?;
    val(Value::Void)
}

fn move_toward(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(target) = instance_arg(vm, a, 0)? else {
        return val(Value::Void);
    };
    let speed = if c.omitted(2) { 1.0 } else { float(vm, a, 2)? };
    let focus = if c.omitted(1) {
        Some(target)
    } else {
        instance_arg(vm, a, 1)?
    };
    vm.set_property(
        c.this,
        "Focus",
        0,
        Value::Object(focus.map(ObjRef::Instance)),
    );
    let next = if c.omitted(3) {
        None
    } else {
        instance_arg(vm, a, 3)?
    };
    vm.set_property(
        c.this,
        "NextMoveTarget",
        0,
        Value::Object(next.map(ObjRef::Instance)),
    );
    let destination = vm.vector_prop(target, "Location").unwrap_or([0.0; 3]);
    vm.set_property(
        c.this,
        "MoveTarget",
        0,
        Value::Object(Some(ObjRef::Instance(target))),
    );
    vm.start_move(
        c.this,
        destination,
        speed,
        "Controller.MoveToward",
        c.in_state_code,
    )?;
    val(Value::Void)
}

/// `Controller.FinishRotation`: enable physics rotation toward Focus/FocalPoint and suspend the
/// current state until the pawn reaches the target orientation.
fn finish_rotation(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !c.in_state_code {
        return Err(vm.err(VmErrorKind::LatentOutsideState {
            path: c.path.clone(),
        }));
    }
    if let Some(pawn) = vm.obj_prop(c.this, "Pawn") {
        vm.set_property(pawn, "bRotateToDesired", 0, Value::Bool(true));
    }
    vm.pending_latent = Some(Latent::Rotation {
        started: vm.time_now(),
    });
    val(Value::Void)
}

/// `IAController.AllianceLevel(Pawn Newenemy) -> int`.
///
/// Disassembly evidence (XIDPawn.dll): `?execAllianceLevel@AIAController` (RVA 0x19E0) reads the
/// single `Newenemy` object parameter and calls `?AllianceLevel@AIAController` (RVA 0x1920). That
/// function returns -1 when `Newenemy` is the controller's `XIII` (`this+0x3A8`) or when `BaseS`
/// (`this+0x3B0`) is null; otherwise it scans four `BaseS.InitialAlliances` entries (stride 8 at
/// `BaseS+0x530`, `(FName, float)`) and returns `InitialAlliances[i].AllianceLevel` (truncated)
/// when `InitialAlliances[i].AllianceName == Newenemy.Alliance` (`Newenemy+0x3BC`) and
/// `Newenemy.Alliance != 'None'`; no match returns 0. The property names are calibrated by the
/// script `IAController.SwitchToEnemy`, which implements the same algorithm symbolically.
///
/// The engine also returns 1 when a `Level` flag word at `Level+0x380` has bit 0x100 set. The
/// decoded reflection does not serialize property offsets, so that anonymous bool is not
/// reproduced (documented `Partial`).
fn alliance_level(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(newenemy) = instance_arg(vm, a, 0)? else {
        return val(Value::Int(-1));
    };
    if vm.obj_prop(c.this, "XIII") == Some(newenemy) {
        return val(Value::Int(-1));
    }
    let Some(base_s) = vm.obj_prop(c.this, "BaseS") else {
        return val(Value::Int(-1));
    };
    let ne_alliance = match vm.get_property(newenemy, "Alliance") {
        Some(Value::Name(n)) => n.clone(),
        _ => "None".to_owned(),
    };
    if ne_alliance.eq_ignore_ascii_case("None") {
        return val(Value::Int(0));
    }
    for i in 0..4 {
        let Some(element) = vm.get_property_elem(base_s, "InitialAlliances", i) else {
            continue;
        };
        let Value::Struct(fields) = element else {
            continue;
        };
        let mut name = None;
        let mut level = None;
        for (n, v) in fields {
            if n.eq_ignore_ascii_case("alliancename") {
                name = Some(v.clone());
            } else if n.eq_ignore_ascii_case("alliancelevel") {
                level = Some(v.clone());
            }
        }
        if let Some(Value::Name(n)) = name
            && n.eq_ignore_ascii_case(&ne_alliance)
        {
            // The native loads the stored float and runs `_ftol` (truncation toward zero).
            let level = match level {
                Some(Value::Float(f)) => f as i32,
                _ => 0,
            };
            return val(Value::Int(level));
        }
    }
    val(Value::Int(0))
}

/// `IAController.HalteAuFeu()`.
///
/// Disassembly evidence (XIDPawn.dll `?execHalteAuFeu@AIAController` RVA 0x1F00): no pawn means
/// return; otherwise the controller clears native flag words at `+0x2C` (bits 0x180000), `+0x212`
/// (byte) and `+0x514` (bit 2), drops the four pointers at `+0x48..+0x54`, and clears bit 0x10000
/// of the pawn flags (`Pawn+0x1F8`); if the pawn's mesh (`Pawn+0x138`) is a `SkeletalMesh` it
/// then resets the mesh instance's bone/aim controllers. The flag words' property names are not
/// serialized in the decoded reflection, so only the bone-control reset (and a visible trace
/// note) is reproduced; the engine flag cleanup is not modelled (documented `Partial`).
fn halte_au_feu(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    if let Some(pawn) = vm.obj_prop(c.this, "Pawn") {
        vm.reset_bone_state(pawn);
        vm.note(TraceKind::Note(format!(
            "HalteAuFeu {}: bone-control reset (Partial: anonymous flag words +0x2C/+0x212/+0x514 not modelled)",
            vm.objects[c.this as usize].name
        )));
    }
    val(Value::Void)
}

/// `IAController.NearWall(float walldist) -> bool`.
///
/// Disassembly evidence (XIDPawn.dll `?execNearWall@AIAController` RVA 0x2E00): with no pawn it
/// returns false; it builds a point at the top of the pawn (`Pawn.Location + (0,0,h)`), derives a
/// direction from the controller's rotation via `FGlobalMath` and probes the world with
/// `ULevel`'s line-check, storing a push-back vector and returning true when geometry is hit
/// close by. The exact multi-trace/projection sequence is not reproduced; the model here is a
/// single forward world line trace of length `walldist` at the top of the pawn (documented
/// `Partial`).
fn near_wall(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let walldist = float(vm, a, 0)?;
    if !vm.physics_ready("IAController.NearWall", None, c.this, Value::Bool(false))? {
        return val(Value::Bool(false));
    }
    let Some(pawn) = vm.obj_prop(c.this, "Pawn") else {
        return val(Value::Bool(false));
    };
    let loc = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
    let rot = match vm.get_property(pawn, "Rotation") {
        Some(Value::Rotator(r)) => *r,
        _ => [0, 0, 0],
    };
    let forward = rotator_basis(rot).0;
    let start = [
        loc[0],
        loc[1],
        loc[2] + vm.f32_prop(pawn, "CollisionHeight"),
    ];
    let end = [
        start[0] + forward[0] * walldist,
        start[1] + forward[1] * walldist,
        start[2] + forward[2] * walldist,
    ];
    let hit = vm
        .physics
        .as_mut()
        .and_then(|p| p.trace(start, end, [0.0; 3]));
    val(Value::Bool(hit.is_some()))
}

/// `IAController.TestDirection(float mindist, float dist, vector Dir, out vector pick) -> bool`.
///
/// Disassembly evidence (XIDPawn.dll `?execTestDirection@AIAController` RVA 0x36D0): it scales
/// `Dir` by `dist`, line-checks from the pawn toward the far point (and a second, adjusted trace
/// on a hit), writes the resulting point to `pick`, and returns whether `pick` is at least
/// `mindist` from the pawn. The model here is one line trace from the top of the pawn toward
/// `Dir * dist`, with `pick` = the hit location or the clear end point (documented `Partial`).
fn test_direction(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let mindist = float(vm, a, 0)?;
    let dist = float(vm, a, 1)?;
    let dir = vector2(vm, a, 2)?;
    if !vm.physics_ready(
        "IAController.TestDirection",
        None,
        c.this,
        Value::Bool(false),
    )? {
        return val(Value::Bool(false));
    }
    let Some(pawn) = vm.obj_prop(c.this, "Pawn") else {
        return val(Value::Bool(false));
    };
    let loc = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
    let start = [
        loc[0],
        loc[1],
        loc[2] + vm.f32_prop(pawn, "CollisionHeight"),
    ];
    let end = [
        start[0] + dir[0] * dist,
        start[1] + dir[1] * dist,
        start[2] + dir[2] * dist,
    ];
    let hit = vm
        .physics
        .as_mut()
        .and_then(|p| p.trace(start, end, [0.0; 3]));
    let pick = hit.map_or(end, |h| h.location);
    if a.len() > 3 {
        a[3] = Value::Vector(pick);
    }
    let d = [pick[0] - loc[0], pick[1] - loc[1], pick[2] - loc[2]];
    let d2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
    val(Value::Bool(d2 >= mindist * mindist))
}

/// `IAController.PickStartPoint() -> PatrolPoint`.
///
/// Disassembly evidence (XIDPawn.dll `?execPickStartPoint@AIAController` RVA 0x3B80): with no
/// `BaseS` it returns null; otherwise it scans a level/`Game` list of points matching a route
/// field (`candidate+0x268 == BaseS+0x574`, then a fallback `candidate+0x268 == None`), keeps the
/// nearest that `Pawn.actorReachable(...)` accepts, and returns it. The list and route field names
/// are not in the decoded reflection, so this returns the nearest decoded navigation point that
/// fits the pawn (falling back to `StartSpot`); `None` when neither is available (documented
/// `Partial`).
fn pick_start_point(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(pawn) = vm.obj_prop(c.this, "Pawn") else {
        return val(Value::Object(None));
    };
    let loc = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
    let radius = vm.f32_prop(pawn, "CollisionRadius");
    let height = vm.f32_prop(pawn, "CollisionHeight");
    // The script follows `DestNavPoint.NextPatrolPoint`, so prefer an actual `PatrolPoint`.
    if let Some(id) = vm.nav_nearest_point_actor(loc, radius, height, Some("PatrolPoint")) {
        return val(Value::Object(Some(ObjRef::Instance(id))));
    }
    if let Some(id) = vm.nav_nearest_point_actor(loc, radius, height, None) {
        return val(Value::Object(Some(ObjRef::Instance(id))));
    }
    if let Some(spot) = vm.obj_prop(c.this, "StartSpot") {
        return val(Value::Object(Some(ObjRef::Instance(spot))));
    }
    val(Value::Object(None))
}

/// `LevelInfo.IncAttaque()` (native 589, static).
///
/// Decoded declaration: `native(589) final native static function IncAttaque()` (engine.u,
/// 1-byte body). `IAController.s_incattaque` calls `self.Level.IncAttaque()` on entering
/// `Attaque.BeginState`. Engine.dll VA 0x103e4750 increments MusicVars[2].Value;
/// the optional audio-device notification is not yet bridged.
fn level_info_inc_attaque(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    vm.adjust_attack_music_var(c.this, 1)?;
    val(Value::Void)
}

/// `LevelInfo.GetPlateForme() -> int`: the platform the game runs on.
///
/// Engine.dll `?execGetPlateForme@ALevelInfo` (VA 0x103df410) returns the `int` at offset 0x7c of
/// `GSys` (Core's `USystem`), the `[Core.System] PlateForm` setting: `Default.ini` ships
/// `PlateForm=0`. Scripts compare it with 1/2/3 for the console builds (e.g. `XIIIPawn` damage
/// feedback, `XIIIBaseHud`); the PC value is 0.
fn level_info_get_plate_forme(
    _vm: &mut Vm<'_>,
    _c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    val(Value::Int(PLATFORM_PC))
}

/// `[Core.System] PlateForm` of the shipped PC `Default.ini` (read by `GetPlateForme`).
const PLATFORM_PC: i32 = 0;

/// `LevelInfo.DecAttaque()` (native 588, static).
/// Engine.dll VA 0x103e4870 decrements MusicVars[2].Value without clamping.
/// The optional audio-device notification is not yet bridged.
fn level_info_dec_attaque(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    vm.adjust_attack_music_var(c.this, -1)?;
    val(Value::Void)
}

/// `IAController.DirectionDuTir() -> vector`.
///
/// Disassembly evidence (XIDPawn.dll `?execDirectionDuTir@AIAController` RVA 0x2070): with no pawn
/// it returns `vect(0,0,0)`; otherwise it builds the shooting direction from the pawn's rotation
/// and `Enemy`. The model here is the unit vector from the pawn to `Enemy.Location`, falling back
/// to the pawn's forward axis when there is no enemy (documented `Partial`: dispersion and the
/// aim offset are not reproduced).
fn direction_du_tir(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let Some(pawn) = vm.obj_prop(c.this, "Pawn") else {
        return val(Value::Vector([0.0; 3]));
    };
    if let Some(enemy) = vm.obj_prop(c.this, "Enemy") {
        let l = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let e = vm.vector_prop(enemy, "Location").unwrap_or(l);
        let d = [e[0] - l[0], e[1] - l[1], e[2] - l[2]];
        let n = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        if n > 1e-6 {
            return val(Value::Vector([d[0] / n, d[1] / n, d[2] / n]));
        }
    }
    let rot = match vm.get_property(pawn, "Rotation") {
        Some(Value::Rotator(r)) => *r,
        _ => [0; 3],
    };
    val(Value::Vector(rotator_basis(rot).0))
}

/// `IAController.LigneVisee(vector TraceEnd, vector TraceStart) -> bool`.
///
/// Disassembly evidence (XIDPawn.dll `?execLigneVisee@AIAController` RVA 0x2B10): line-checks the
/// segment between the two points and returns whether it is clear. The VM uses its world-only
/// `Actor.FastTrace` (no actor occlusion, the same convention as `Controller.LineOfSightTo`);
/// `TraceStart` omitted defaults to the pawn's eye.
fn ligne_visee(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let end = vector2(vm, a, 0)?;
    let start = if c.omitted(1) {
        match vm.obj_prop(c.this, "Pawn") {
            Some(pawn) => {
                let l = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
                [l[0], l[1], l[2] + vm.f32_prop(pawn, "CollisionHeight")]
            }
            None => return val(Value::Bool(false)),
        }
    } else {
        vector2(vm, a, 1)?
    };
    if !vm.physics_ready("IAController.LigneVisee", None, c.this, Value::Bool(false))? {
        return val(Value::Bool(false));
    }
    val(Value::Bool(vm.vm_fast_trace(start, end)?))
}

/// `IAController.PseudoSteering() -> vector`.
///
/// Disassembly evidence (XIDPawn.dll `?execPseudoSteering@AIAController` RVA 0x3230): the combat
/// steering vector used by `Attaque.UpdateTactics`, over GenAlerte.SoldierInFightList.
/// See Vm::ai_pseudo_steering for the unusual equality gate present in the retail DLL.
fn pseudo_steering(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Vector(vm.ai_pseudo_steering(c.this)?))
}

/// `IAController.LineOfFireObstacle() -> int`.
///
/// Disassembly evidence (XIDPawn.dll `?execLineOfFireObstacle@AIAController` RVA 0x40D0): returns
/// 0 for no obstacle/enemy/self/world/dead/hostile pawn, 1 for a living ally (writes Pote),
/// 2 for a non-pawn that cannot be seen through. Trace flags depend on AmmoType.bInstantHit.
fn line_of_fire_obstacle(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let Some(hit) = vm.ai_fire_obstacle(c.this)? else {
        return val(Value::Int(0));
    };
    if vm.is_a(hit, "Pawn") {
        if vm.bool_prop(hit, "bIsDead") {
            return val(Value::Int(0));
        }
        let mut args = [Value::Object(Some(ObjRef::Instance(hit)))];
        if matches!(alliance_level(vm, c, &mut args)?, NativeOutcome::Value(Value::Int(n)) if n >= 0)
        {
            vm.set_property(c.this, "Pote", 0, args[0].clone());
            return val(Value::Int(1));
        }
        return val(Value::Int(0));
    }
    val(Value::Int(if vm.bool_prop(hit, "bCanSeeThrough") {
        0
    } else {
        2
    }))
}

/// `IAController.FindBestPathTo(vector desti) -> bool`.
///
/// XIDPawn.dll VA 0x11903a90 calls FindPath(desti,None,1), assigning MoveTarget and Destination
/// only on success. The shared ReachSpec search remains Partial: native anchor, cost and endpoint
/// checks have not been ported.
fn find_best_path_to(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let goal = vector2(vm, a, 0)?;
    let first = vm.nav_find_path_to(c.this, goal)?;
    if let Some(first) = first {
        vm.set_property(
            c.this,
            "MoveTarget",
            0,
            Value::Object(Some(ObjRef::Instance(first))),
        );
        if let Some(location) = vm.vector_prop(first, "Location") {
            vm.set_property(c.this, "Destination", 0, Value::Vector(location));
        }
    }
    val(Value::Bool(first.is_some()))
}

/// `IAController.FindNewStakeOutDir()`.
///
/// XIDPawn.dll VA 0x11903ea0 updates LastSeenPos from the most aligned visible
/// navigation point at distance (100,800).
fn find_new_stake_out_dir(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    vm.ai_stake_out_dir(c.this)?;
    val(Value::Void)
}

/// `Pawn.SpineYawControl(bool IsControlled, int MaxValue, float RotationSpeed)`.
///
/// Disassembly evidence (Engine.dll `?execSpineYawControl@APawn` RVA 0xAFD00): it sets/clears bit
/// 0x80 of the pawn's native flags word at `+0x1F8` from `IsControlled`, stores `MaxValue` at
/// `+0x208` and `RotationSpeed` at `+0x210`. The headless VM keeps the same parameters per actor
/// for the renderer; no skeletal pose is computed (documented `Partial`).
fn spine_yaw_control(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let is_controlled = boolean(vm, a, 0)?;
    let max_value = int(vm, a, 1)?;
    let rotation_speed = float(vm, a, 2)?;
    vm.set_spine_control(c.this, is_controlled, max_value, rotation_speed);
    val(Value::Void)
}

/// `Actor.SetBoneDirection(name BoneName, rotator BoneTurn, vector BoneTrans, float Alpha)`.
///
/// Disassembly evidence (Engine.dll `?execSetBoneDirection@AActor` RVA 0xE2680, forwarding to
/// `?SetBoneDirection@USkeletalMeshInstance` RVA 0xED5C0): it applies a bone-controller request on
/// the actor's skeletal mesh. The headless VM records the request per actor for the renderer; no
/// skeletal transform is evaluated (documented `Partial`).
fn set_bone_direction(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let bone = name(vm, a, 0)?;
    let turn = rotator2(vm, a, 1)?;
    let trans = vector2(vm, a, 2)?;
    let alpha = float(vm, a, 3)?;
    vm.add_bone_direction(c.this, bone, turn, trans, alpha);
    val(Value::Void)
}

/// `Actor.SetBoneScalePerAxis(int Slot, float BoneScaleX, float BoneScaleY, float BoneScaleZ,
/// name BoneName)`.
///
/// Disassembly evidence (Engine.dll `?execSetBoneScalePerAxis@AActor` RVA 0xE23B0): it defaults each
/// omitted optional axis to `1.0` (`0x3F800000`) and forwards
/// `SetBoneScale(Slot, X, Y, Z, BoneName)` to `?SetBoneScale@USkeletalMeshInstance` RVA 0xED210.
/// The headless VM records the request per actor for the renderer; no skeletal transform is
/// evaluated (documented `Partial`).
fn set_bone_scale_per_axis(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let slot = int(vm, a, 0)?;
    let x = if c.omitted(1) { 1.0 } else { float(vm, a, 1)? };
    let y = if c.omitted(2) { 1.0 } else { float(vm, a, 2)? };
    let z = if c.omitted(3) { 1.0 } else { float(vm, a, 3)? };
    let bone = name(vm, a, 4)?;
    vm.add_bone_scale(c.this, slot, [x, y, z], bone);
    val(Value::Void)
}

fn class_arg(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<Option<GlobalRef>> {
    match a.get(i) {
        Some(Value::Object(Some(ObjRef::Static(g))))
            if matches!(
                vm.set().object(*g),
                Some(crate::reflect::ScriptObject::Class(_))
            ) =>
        {
            Ok(Some(*g))
        }
        Some(Value::Object(Some(ObjRef::Static(g)))) => Err(vm.err(VmErrorKind::Other(format!(
            "Spawn class argument {} is not a class",
            vm.set().path(*g)
        )))),
        Some(Value::Object(Some(ObjRef::Instance(id)))) => Err(vm.err(VmErrorKind::Other(
            format!("Spawn class argument is an instance ({id})"),
        ))),
        _ => Ok(None),
    }
}

fn instance_arg(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<Option<ObjectId>> {
    match a.get(i) {
        Some(Value::Object(Some(ObjRef::Instance(id)))) if !vm.objects[*id as usize].deleted => {
            Ok(Some(*id))
        }
        _ => Ok(None),
    }
}

fn spawn(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `execSpawn` reads the optional args with `P_GET_*_OPTX`: when an argument is present
    // (even if it is the zero value) it is used; only a truly omitted argument falls back to
    // the spawner's Location/Rotation. Presence is exact, not inferred from an all-zero value.
    let class = class_arg(vm, a, 0)?;
    let owner = instance_arg(vm, a, 1)?;
    let tag = match a.get(2) {
        Some(Value::Name(n)) if !c.omitted(2) && !n.eq_ignore_ascii_case("None") => Some(n.clone()),
        _ => None,
    };
    let location = if c.omitted(3) {
        None
    } else {
        match a.get(3) {
            Some(Value::Vector(v)) => Some(*v),
            Some(other) => return Err(type_err(vm, "vector", other)),
            None => None,
        }
    };
    let rotation = if c.omitted(4) {
        None
    } else {
        match a.get(4) {
            Some(Value::Rotator(r)) => Some(*r),
            Some(other) => return Err(type_err(vm, "rotator", other)),
            None => None,
        }
    };
    let id = vm.spawn_actor(c.this, class, owner, tag.as_deref(), location, rotation)?;
    val(match id {
        Some(id) => Value::Object(Some(ObjRef::Instance(id))),
        None => Value::Object(None),
    })
}

fn destroy(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let r = vm.destroy(c.this)?;
    val(Value::Bool(r))
}

fn actor_move(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let delta = vector2(vm, a, 0)?;
    if vm.bool_prop(c.this, "bCollideWorld")
        && !vm.physics_ready("Actor.Move", Some(266), c.this, Value::Bool(false))?
    {
        return val(Value::Bool(false));
    }
    let moved = vm.vm_move(c.this, delta)?;
    val(Value::Bool(moved))
}

fn actor_set_location(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let location = vector2(vm, a, 0)?;
    let check_world =
        vm.bool_prop(c.this, "bCollideWorld") || vm.bool_prop(c.this, "bCollideWhenPlacing");
    if check_world
        && !vm.physics_ready("Actor.SetLocation", Some(267), c.this, Value::Bool(false))?
    {
        return val(Value::Bool(false));
    }
    let ok = vm.vm_set_location(c.this, location)?;
    val(Value::Bool(ok))
}

fn actor_trace(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // Params: 0 HitLocation(out), 1 HitNormal(out), 2 TraceEnd, 3 TraceStart, 4 bTraceActors,
    // 5 Extent, 6 Material(out), 7 AdditionalTraceType, 8 DiscardedHitMask(out).
    let end = vector2(vm, a, 2)?;
    let start = if c.omitted(3) {
        vm.vector_prop(c.this, "Location").unwrap_or([0.0; 3])
    } else {
        vector2(vm, a, 3)?
    };
    let b_trace_actors = if c.omitted(4) {
        // UT99 227 `Actor.uc` documents `optional bool bTraceActors // = bCollideActors`.
        vm.bool_prop(c.this, "bCollideActors")
    } else {
        boolean(vm, a, 4)?
    };
    let extent = if c.omitted(5) {
        [0.0; 3]
    } else {
        vector2(vm, a, 5)?
    };
    if !vm.physics_ready("Actor.Trace", Some(277), c.this, Value::Object(None))? {
        return val(Value::Object(None));
    }
    // Engine.dll execTrace 0x103e8ac7: (bTraceActors ? 0xBF : 0x86) | extra.
    // AdditionalTraceType is additive, so it can admit pawns even with bTraceActors=false.
    let additional = if a.len() <= 7 || c.omitted(7) {
        0
    } else {
        int(vm, a, 7)? as u32
    };
    let flags = (if b_trace_actors { 0xbf } else { 0x86 }) | additional;
    let (hit_actor, hit_location, hit_normal) =
        vm.vm_trace_flags(c.this, start, end, flags, extent)?;
    a[0] = Value::Vector(hit_location);
    a[1] = Value::Vector(hit_normal);
    if a.len() > 6 && !c.omitted(6) {
        // Material: the VM has no material objects; upstream fills the hit surface material.
        a[6] = Value::Object(None);
    }
    if a.len() > 8 && !c.omitted(8) {
        // DiscardedHitMask: no discarded-hit filtering is modelled.
        a[8] = Value::Int(0);
    }
    val(match hit_actor {
        Some(id) => Value::Object(Some(ObjRef::Instance(id))),
        None => Value::Object(None),
    })
}

fn actor_trace_actors(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(class)) => Some(class),
        None => None,
        Some(_) => {
            return Err(vm.err(VmErrorKind::Other(
                "TraceActors BaseClass is not a class".into(),
            )));
        }
    };
    let end = vector2(vm, a, 4)?;
    let start = if c.omitted(5) {
        vm.vector_prop(c.this, "Location").unwrap_or([0.0; 3])
    } else {
        vector2(vm, a, 5)?
    };
    let extent = if c.omitted(6) {
        [0.0; 3]
    } else {
        vector2(vm, a, 6)?
    };
    if !vm.physics_ready("Actor.TraceActors", Some(309), c.this, Value::Void)? {
        return Ok(NativeOutcome::Iterate(Vec::new()));
    }
    let rows = vm.vm_trace_actors(c.this, base, start, end, extent)?;
    Ok(NativeOutcome::Iterate(
        rows.into_iter()
            .map(|(actor, loc, normal)| {
                vec![
                    Value::Object(Some(ObjRef::Instance(actor))),
                    Value::Vector(loc),
                    Value::Vector(normal),
                ]
            })
            .collect(),
    ))
}

fn actor_fast_trace(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // Params: 0 TraceEnd, 1 TraceStart, 2 AdditionalTraceType, 3 DiscardedHitMask(out).
    let end = vector2(vm, a, 0)?;
    let start = if c.omitted(1) {
        vm.vector_prop(c.this, "Location").unwrap_or([0.0; 3])
    } else {
        vector2(vm, a, 1)?
    };
    if !vm.physics_ready("Actor.FastTrace", Some(548), c.this, Value::Bool(false))? {
        return val(Value::Bool(false));
    }
    if a.len() > 3 && !c.omitted(3) {
        a[3] = Value::Int(0);
    }
    val(Value::Bool(vm.vm_fast_trace(start, end)?))
}

fn actor_set_collision(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let col = if c.omitted(0) {
        None
    } else {
        Some(boolean(vm, a, 0)?)
    };
    let block_actors = if c.omitted(1) {
        None
    } else {
        Some(boolean(vm, a, 1)?)
    };
    let block_players = if c.omitted(2) {
        None
    } else {
        Some(boolean(vm, a, 2)?)
    };
    vm.vm_set_collision(c.this, col, block_actors, block_players)?;
    val(Value::Void)
}

fn actor_set_collision_size(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let radius = float(vm, a, 0)?;
    let height = float(vm, a, 1)?;
    let ok = vm.vm_set_collision_size(c.this, radius, height)?;
    val(Value::Bool(ok))
}

fn actor_touching_actors(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => Some(g),
        None => None,
        Some(ObjRef::Instance(_)) | Some(ObjRef::External(_)) => {
            return Err(vm.err(VmErrorKind::Other(
                "TouchingActors base class is an instance".into(),
            )));
        }
    };
    let items: Vec<Value> = vm
        .touching_list(c.this)
        .into_iter()
        .filter(|id| base.is_none_or(|b| vm.objects[*id as usize].layout.chain.contains(&b)))
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(
        items.into_iter().map(|v| vec![v]).collect(),
    ))
}

fn channel(vm: &Vm<'_>, a: &[Value], i: usize, omitted: bool) -> VmResult<u8> {
    if omitted {
        return Ok(0);
    }
    let c = int(vm, a, i)?;
    u8::try_from(c).map_err(|_| {
        vm.err(VmErrorKind::TypeMismatch {
            expected: "channel 0..255",
            found: "int",
        })
    })
}

fn link_skel_anim(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let anim = object(vm, a, 0)?;
    vm.link_skel_anim(c.this, anim);
    val(Value::Void)
}

fn anim_blend_params(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let stage = int(vm, a, 0)?;
    let blend_alpha = if c.omitted(1) { 1.0 } else { float(vm, a, 1)? };
    let in_time = if c.omitted(2) { 0.0 } else { float(vm, a, 2)? };
    let out_time = if c.omitted(3) { 0.0 } else { float(vm, a, 3)? };
    let bone_name = if c.omitted(4) {
        None
    } else {
        let n = name(vm, a, 4)?;
        (!n.eq_ignore_ascii_case("None")).then_some(n)
    };
    vm.anim_blend_params(c.this, stage, blend_alpha, in_time, out_time, bone_name);
    val(Value::Void)
}

fn play_anim(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !vm.animation_ready("Actor.PlayAnim", Some(259), c.this, Value::Void)? {
        return val(Value::Void);
    }
    let seq = name(vm, a, 0)?;
    let rate = if c.omitted(1) { 1.0 } else { float(vm, a, 1)? };
    let tween = if c.omitted(2) { 0.0 } else { float(vm, a, 2)? };
    let ch = channel(vm, a, 3, c.omitted(3))?;
    vm.start_animation(c.this, &seq, rate, tween, ch, false)?;
    val(Value::Void)
}

fn loop_anim(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !vm.animation_ready("Actor.LoopAnim", Some(260), c.this, Value::Void)? {
        return val(Value::Void);
    }
    let seq = name(vm, a, 0)?;
    let rate = if c.omitted(1) { 1.0 } else { float(vm, a, 1)? };
    let tween = if c.omitted(2) { 0.0 } else { float(vm, a, 2)? };
    let ch = channel(vm, a, 3, c.omitted(3))?;
    vm.start_animation(c.this, &seq, rate, tween, ch, true)?;
    val(Value::Void)
}

fn tween_anim(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !vm.animation_ready("Actor.TweenAnim", Some(294), c.this, Value::Void)? {
        return val(Value::Void);
    }
    let seq = name(vm, a, 0)?;
    let time = if c.omitted(1) { 0.0 } else { float(vm, a, 1)? };
    let ch = channel(vm, a, 2, c.omitted(2))?;
    vm.start_animation(c.this, &seq, 0.0, time, ch, false)?;
    vm.mark_tween_only(c.this, ch);
    val(Value::Void)
}

fn is_animating(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let ch = channel(vm, a, 0, c.omitted(0))?;
    val(Value::Bool(vm.anim_channel_active(c.this, ch)))
}

fn has_anim(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    if !vm.animation_ready("Actor.HasAnim", Some(263), c.this, Value::Bool(false))? {
        return val(Value::Bool(false));
    }
    let seq = name(vm, a, 0)?;
    let r = vm.has_anim("Actor.HasAnim", c.this, &seq)?;
    val(Value::Bool(r))
}

fn finish_anim(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let ch = channel(vm, a, 0, c.omitted(0))?;
    vm.finish_anim(c.this, ch, c.in_state_code)?;
    val(Value::Void)
}

fn set_view_target(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let target = object(vm, a, 0)?;
    vm.set_property(c.this, "ViewTarget", 0, Value::Object(target));
    val(Value::Void)
}

/// Engine.dll `AActor::execMakeNoise` (0x103aff40) -> `AActor::CheckNoiseHearing` (0x1036b6d0)
/// -> `AController::CanHear` (0x1036b0f0) -> `eventHearNoise(Loudness, NoiseMaker)` (item41c,
/// decoded in full; see `Vm::vm_make_noise`). Partial only where the VM lacks the engine's data:
/// zone hearing (no zone model) and the BSP-only traces of the muffled/around-corner branches
/// (the provider's world trace stands in); each is reported once with a trace note.
fn make_noise(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let loudness = float(vm, a, 0)?;
    vm.vm_make_noise(c.this, loudness)?;
    val(Value::Void)
}

/// item14 `Actor.GetLastTraceBone`: the bone name chosen by the engine's last actor trace, read
/// by `XIIIWeapon.RealTraceFire` into `XIIIPawn.LastBoneHit` and then by
/// `XIIIPawn.GetDamageLocation` (head/spine classification). The VM records the zone in
/// `Vm::vm_trace`; see [`crate::physics::HitZones`].
fn get_last_trace_bone(vm: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Name(vm.last_trace_bone().to_owned()))
}

/// item14 `Actor.IntersectWaterPlane`: UE2 returns the `PhysicsVolume` a segment crosses at a
/// water plane, or `None`. XIII only uses it to route bullet impacts to a water volume; the VM
/// does not model water volumes, so it returns `None` and writes the segment end into the `out`
/// `Intersection` (the caller only dereferences the volume when non-null).
fn intersect_water_plane(
    _vm: &mut Vm<'_>,
    _: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    if let Some(end) = a.get(1).and_then(|v| match v {
        Value::Vector(v) => Some(*v),
        _ => None,
    }) && let Some(slot) = a.get_mut(2)
    {
        *slot = Value::Vector(end);
    }
    val(Value::Object(None))
}

/// item14 `IAController.SetEnemy`: stores `Enemy` on the controller (the XIDPawn native sets the
/// controller's current target). Returns whether the target was accepted (always true here; the
/// native's extra `BaseS` bookkeeping is not modelled).
fn ia_controller_set_enemy(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let new_value = Value::Object(object(vm, a, 0)?);
    let old = vm.get_property(c.this, "Enemy").cloned();
    vm.set_property(c.this, "Enemy", 0, new_value.clone());
    // The engine's `AAIController::SetEnemy` raises the `EnemyAcquired` event when the enemy
    // changes (XIDPawn.dll exports `?eventEnemyAcquired@AIAController`); the XIII states use it to
    // leave the neutral states (`Patrouille`/`Tenir` -> `acquisition`). Without this the AI sets
    // `Enemy` and never reacts.
    let changed = matches!(new_value, Value::Object(Some(ObjRef::Instance(_))))
        && old.as_ref() != Some(&new_value);
    if changed {
        vm.send_event(c.this, "EnemyAcquired", Vec::new())?;
    }
    val(Value::Bool(true))
}

/// item14 `Object.SubtractSubtract_Byte`: the UE2 `--` pre-decrement on a byte value
/// (`--ReloadCount` in `XIIIWeapon.LoneFire`); returns `A - 1` as a byte.
fn dec_byte(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let b = byte(vm, a, 0)?;
    val(Value::Byte(b.wrapping_sub(1)))
}

/// item14 `Object.AddAdd_Byte`: the UE2 `++` pre-increment on a byte value; returns `A + 1`.
fn inc_byte(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let b = byte(vm, a, 0)?;
    val(Value::Byte(b.wrapping_add(1)))
}

/// item14 `Object.Warn`: the UE2 warning log. The VM has no log sink; accepted and discarded
/// (recorded as a `Note`). `XIIIPawn.TakeDamage` warns when a dead pawn is hit again.
fn warn_log(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    if let Some(Value::Str(s)) = a.first() {
        vm.note(TraceKind::Note(format!("Warn: {s}")));
    }
    val(Value::Void)
}

/// item14 `Object.Subtract_PreVector`: the UE2 unary `-` on a vector, used by
/// `XIIIBulletsAmmo.ProcessTraceHit` when it aims spawned emitters.
fn neg_vector(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let v = vector2(vm, a, 0)?;
    val(Value::Vector([-v[0], -v[1], -v[2]]))
}

/// item14 `Actor.PlaySndDeathOno`: death onomatopoeia sound; accepted and discarded (the VM has
/// no HX sound mapping). Called by `BaseSoldier.Died`.
fn play_snd_death_ono(_vm: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Void)
}

/// `Actor.RefreshLighting` (389): the engine recomputes the lighting affected by this actor
/// (dynamic lights such as the muzzle flash). There is no script-visible result; the renderer
/// does not draw dynamic lights yet, so the call is recorded as a visible trace note.
fn refresh_lighting(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = vm.objects[c.this as usize].name.clone();
    vm.note(TraceKind::Note(format!(
        "RefreshLighting on {name} (dynamic lights not rendered)"
    )));
    val(Value::Void)
}

/// item14b `Weapon.PlayFiringSound`: the engine-side firing sound. The decoded
/// `Weapon` class carries `hFireSound` (normal), `hAltFireSound` (silencer/alt) and `bUseSilencer`;
/// the `bHasSilencer` argument chosen by `HasSilencer()` in `Beretta.PlayFiring` selects between
/// them (name-table evidence: `hFireSound` 12x in engine.u, `hAltFireSound` 8x; `Beretta.PlayFiring`
/// calls `PlayFiringSound(HasSilencer())`). The native emits a `PlaySound` presentation event whose
/// `actor` is the weapon's `Instigator` (the pawn) so a soldier's gunfire is positional in the
/// host; the HX wave is resolved from `hFireSound` by the host audio library. When neither
/// property holds a `Sound`, the call appends a visible trace note instead of silently succeeding.
fn play_firing_sound(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let silencer = matches!(a.first(), Some(Value::Bool(true)));
    let primary = if silencer {
        "hAltFireSound"
    } else {
        "hFireSound"
    };
    let secondary = if silencer {
        "hFireSound"
    } else {
        "hAltFireSound"
    };
    let sound = vm
        .get_property(c.this, primary)
        .and_then(|v| vm.obj_path(v))
        .or_else(|| {
            vm.get_property(c.this, secondary)
                .and_then(|v| vm.obj_path(v))
        });
    let speaker = vm.obj_prop(c.this, "Instigator").unwrap_or(c.this);
    let actor = vm.objects[speaker as usize].name.clone();
    if sound.is_none() {
        let weapon = vm.objects[c.this as usize].name.clone();
        vm.note(TraceKind::Note(format!(
            "PlayFiringSound: {weapon} has no {primary}/{secondary} Sound"
        )));
    }
    vm.emit_event(PresentationEvent::PlaySound(SoundEvent {
        actor,
        sound,
        rolloff_actor: None,
        slot: None,
        volume: None,
        radius: None,
        pitch: None,
        param5: None,
        time: vm.time,
    }));
    val(Value::Void)
}

/// item14 `Pawn.EyePosition`: the eye offset from the pawn `Location` (UE2 applies crouch/view
/// height; the VM returns `EyeHeight` along +Z, falling back to `BaseEyeHeight`). Needed by
/// `XIIIWeapon.RealTraceFire`'s trace start.
fn pawn_eye_position(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    let h = match vm.get_property(c.this, "EyeHeight") {
        Some(Value::Float(f)) => *f,
        _ => vm.f32_prop(c.this, "BaseEyeHeight"),
    };
    val(Value::Vector([0.0, 0.0, h]))
}

/// item14 `Pawn.GetViewRotation`: the rotation the pawn looks along. UE2 returns the controller's
/// rotation for a player-controlled pawn; the VM returns `Controller.Rotation` when present,
/// else the pawn's own `Rotation`.
fn pawn_get_view_rotation(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let rot = vm
        .obj_prop(c.this, "Controller")
        .and_then(|ctrl| match vm.get_property(ctrl, "Rotation") {
            Some(Value::Rotator(r)) => Some(*r),
            _ => None,
        })
        .or_else(|| match vm.get_property(c.this, "Rotation") {
            Some(Value::Rotator(r)) => Some(*r),
            _ => None,
        })
        .unwrap_or([0; 3]);
    val(Value::Rotator(rot))
}

/// item14 `Weapon.GetFireStart`: the muzzle position for the hitscan. The VM returns the
/// instigator's eye (Location + `EyePosition`); the decoded muzzle offsets are not applied.
fn weapon_get_fire_start(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let Some(pawn) = vm.obj_prop(c.this, "Instigator") else {
        return val(Value::Vector([0.0; 3]));
    };
    let loc = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
    let eye = match vm.get_property(pawn, "EyeHeight") {
        Some(Value::Float(f)) => *f,
        _ => vm.f32_prop(pawn, "BaseEyeHeight"),
    };
    val(Value::Vector([loc[0], loc[1], loc[2] + eye]))
}

/// item14 `Pawn.CalcDrawOffset`: the first-person draw offset of an inventory item. The VM
/// returns the item's `PlayerViewOffset` (the scripts carry the value; the host uses it for
/// presentation), never an error, so `Weapon.BringUp`'s `Active.BeginState` can run.
fn pawn_calc_draw_offset(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let off = object(vm, a, 0)?
        .and_then(|r| match r {
            ObjRef::Instance(i) => vm.vector_prop(i, "PlayerViewOffset"),
            _ => None,
        })
        .unwrap_or([0.0; 3]);
    val(Value::Vector(off))
}

/// `Canvas.MakeColor`: UE2 packs the four bytes into the `Color` struct (A defaults to 255 when
/// omitted). `PlayerController.ClearProgressMessages` calls it on the login/PostLogin path.
fn make_color(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let r = byte(vm, a, 0)?;
    let g = byte(vm, a, 1)?;
    let b = byte(vm, a, 2)?;
    let alpha = if c.omitted(3) { 255 } else { byte(vm, a, 3)? };
    val(Value::Struct(vec![
        ("r".into(), Value::Byte(r)),
        ("g".into(), Value::Byte(g)),
        ("b".into(), Value::Byte(b)),
        ("a".into(), Value::Byte(alpha)),
    ]))
}

/// The named byte members of a `Color` struct value.
fn color_fields(vm: &Vm<'_>, a: &[Value], i: usize) -> VmResult<Vec<(String, Value)>> {
    match a.get(i) {
        Some(Value::Struct(f)) if f.len() == 4 => Ok(f.clone()),
        Some(v) => Err(type_err(vm, "struct<Color>", v)),
        None => Err(vm.err(VmErrorKind::Other("missing Color argument".into()))),
    }
}

fn color_channel(fields: &[(String, Value)], name: &str) -> i32 {
    fields
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .and_then(|(_, v)| match v {
            Value::Byte(b) => Some(i32::from(*b)),
            _ => None,
        })
        .unwrap_or(0)
}

fn clamp_byte(v: i32) -> Value {
    Value::Byte(v.clamp(0, 255) as u8)
}

/// A `Color` rebuilt in the component order of the first operand, from `(b, g, r, a)`.
fn color_from_bgra(order: &[(String, Value)], bgra: [i32; 4]) -> Value {
    let pick = |name: &str| match name.to_ascii_lowercase().as_str() {
        "b" => clamp_byte(bgra[0]),
        "g" => clamp_byte(bgra[1]),
        "r" => clamp_byte(bgra[2]),
        "a" => clamp_byte(bgra[3]),
        _ => Value::Byte(0),
    };
    Value::Struct(order.iter().map(|(k, _)| (k.clone(), pick(k))).collect())
}

/// `Actor.Multiply_ColorFloat(Color A, float B)` (native 552): componentwise `A * B`, truncated
/// and clamped to `[0,255]` (UE2 `FColor` scalar multiply). Called by the HUD widget draw path
/// (`XIIIBaseHud`/`HudState.DrawStt`).
fn multiply_color_float(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let fields = color_fields(vm, a, 0)?;
    let f = float(vm, a, 1)?;
    let m = |name: &str| (color_channel(&fields, name) as f32 * f) as i32;
    val(color_from_bgra(&fields, [m("b"), m("g"), m("r"), m("a")]))
}

/// `Actor.Multiply_FloatColor(float A, Color B)` (native 550): the reversed operand order.
fn multiply_float_color(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let fields = color_fields(vm, a, 1)?;
    let f = float(vm, a, 0)?;
    let m = |name: &str| (color_channel(&fields, name) as f32 * f) as i32;
    val(color_from_bgra(&fields, [m("b"), m("g"), m("r"), m("a")]))
}

/// `Actor.Add_ColorColor(Color A, Color B)` (native 551): componentwise sum, clamped.
fn add_color_color(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = color_fields(vm, a, 0)?;
    let y = color_fields(vm, a, 1)?;
    let s = |name: &str| color_channel(&x, name) + color_channel(&y, name);
    val(color_from_bgra(&x, [s("b"), s("g"), s("r"), s("a")]))
}

/// `Actor.Subtract_ColorColor(Color A, Color B)` (native 549): componentwise difference, clamped.
fn subtract_color_color(
    vm: &mut Vm<'_>,
    _c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let x = color_fields(vm, a, 0)?;
    let y = color_fields(vm, a, 1)?;
    let d = |name: &str| color_channel(&x, name) - color_channel(&y, name);
    val(color_from_bgra(&x, [d("b"), d("g"), d("r"), d("a")]))
}

fn play_sound(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.emit_sound(false, c.this, a, &c.omitted);
    val(Value::Void)
}

fn play_music(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.emit_sound(true, c.this, a, &c.omitted);
    val(Value::Void)
}

/// `Actor.StopVoice()`: stop the actor's current voice/dialogue audio. The engine
/// (`?execStopVoice@AActor` RVA 0xE3F00) calls the audio subsystem's voice channel
/// (`vtable +0xA4` with `(0, 0, 4, 1)`); the headless VM emits a `StopVoice` presentation event.
fn stop_voice(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::StopVoice { actor, time });
    val(Value::Void)
}

/// `Actor.StopSound(object<Sound> Sound)`: stop one sound on the actor (Engine.dll
/// `?execStopSound@AActor`, declaration `native(265)`). Emits a `StopSound` presentation event.
fn stop_sound(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let sound = object(vm, a, 0)?.map(|r| vm.obj_label(&r));
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::StopSound { actor, sound, time });
    val(Value::Void)
}

/// `Actor.PlaySndPNJOno(object<SndOno> Sound, int CodeMesh, int Timbre)`: play an onomatopoeia
/// sound. Engine.dll `?execPlaySndPNJOno@AActor` RVA 0xE2D10 forwards `(this, Sound, CodeMesh,
/// Timbre)` to the audio subsystem (`vtable +0xD8`) and no-ops on a null `Sound`. The headless VM
/// emits a `PlaySndPNJOno` presentation event.
fn play_snd_pn_jo(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let sound = object(vm, a, 0)?.map(|r| vm.obj_label(&r));
    let code_mesh = int(vm, a, 1)?;
    let timbre = int(vm, a, 2)?;
    if sound.is_none() {
        return val(Value::Void);
    }
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::PlaySndPNJOno {
        actor,
        sound,
        code_mesh,
        timbre,
        time,
    });
    val(Value::Void)
}

/// `PlayerController.ConsoleCommand(string Command) -> string`.
///
/// The campaign scripts issue a small set of commands. Implemented as VM effects/queries; every
/// other command is logged (a visible `Note`) and returns the empty string — never silently
/// accepted. The engine command set (Engine.dll `?execConsoleCommand@APlayerController` RVA
/// 0x698F0) is much larger; only the commands reached on the campaign path are modelled here.
///
/// - `GETPING` (`PlayerReplicationInfo.Timer`): the engine returns the round-trip ping; the
///   headless VM has no network, so it returns `0`.
/// - `Get GameInfo GoreLevel` (`XIIIPlayerController.ClientSetHUD`): returns
///   `Level.Game.GoreLevel` as a decimal string (the parental-lock check).
fn console_command(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let command = string(vm, a, 0)?;
    if vm.canvas.menu.is_some() {
        return crate::canvas::menu_console(vm, &command)
            .map_err(|e| vm.err(VmErrorKind::Other(e)));
    }
    let norm = command.trim().to_ascii_lowercase();
    let reply = match norm.as_str() {
        "getping" => "0".to_owned(),
        "get gameinfo gorelevel" => {
            let gore = vm
                .obj_prop(c.this, "Level")
                .and_then(|l| vm.obj_prop(l, "Game"))
                .and_then(|g| match vm.get_property(g, "GoreLevel") {
                    Some(Value::Int(v)) => Some(*v),
                    _ => None,
                })
                .unwrap_or(0);
            gore.to_string()
        }
        _ => {
            vm.note(TraceKind::Note(format!(
                "ConsoleCommand({command:?}) is not implemented; returned an empty string"
            )));
            String::new()
        }
    };
    val(Value::Str(reply))
}

/// `PlayerController.ClientTravel(string URL, byte<ETravelType> TravelType, bool bItems) -> void`.
///
/// The engine's client-travel entry point. `XIIIGameInfo.ProcessServerTravel` (xiii.u) calls it
/// for a network client (`Player != None`) and `LevelInfo.ServerTravel` reaches it in the
/// standalone path through `Game.ProcessServerTravel`; the front-end menu reaches it too
/// (`XIIIMenu.EndOfVideo`, item16). The VM never loads a map: it records a
/// [`TravelRequest`](crate::events::TravelRequest) (the URL, the `ETravelType` byte and `bItems`)
/// for the host to consume with [`Vm::take_travel_request`], **and** the item16 structured trace
/// note (`canvas::format_travel_note`) that the `xiii-app --menu` host reads via
/// `canvas::parse_travel_note`. This is the single registration for the native.
fn client_travel(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let url = string(vm, a, 0)?;
    let mode = byte(vm, a, 1)?;
    let items = boolean(vm, a, 2)?;
    vm.note(TraceKind::Note(crate::canvas::format_travel_note(
        &url, mode, items,
    )));
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.request_travel(TravelRequest {
        actor,
        url,
        mode,
        items,
        source: TravelSource::ClientTravel,
        time,
    });
    val(Value::Void)
}

/// `PlayerController.GetDefaultURL(string Option) -> string` (native 510).
///
/// UE2 reads the option (`Skin`/`Face`/`Team`/`Name`/`Class`) from the player's stored
/// connection defaults. The headless VM has no player profile; it returns the empty string and
/// records a visible note. The standalone travel path does not reach this native
/// (`XIIIGameInfo.ProcessServerTravel` only calls it when `Level.NetMode == 2`).
fn get_default_url(vm: &mut Vm<'_>, _c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let option = string(vm, a, 0)?;
    vm.note(TraceKind::Note(format!(
        "GetDefaultURL({option:?}) has no player profile; returned an empty string"
    )));
    val(Value::Str(String::new()))
}

/// `PlayerController.CalcFirstPersonView(out Vector CameraLocation, out Rotator CameraRotation)`
/// (native 497).
///
/// The first-person camera pose on the goal/end-game path: `MapInfo.DoTravel` ->
/// `XIIIGameInfo.EndGame` -> `XIIIPlayerController.GameEnded.BeginState` -> `global.PlayerCalcView`
/// -> `engine.PlayerController.PlayerCalcView` calls it. The headless VM has no renderer, so the
/// pose is the host's first-person description: the pawn's eye position and the controller's
/// rotation (falling back to the pawn's). Without it the end-game state suspends and the level
/// cannot travel (item15).
fn calc_first_person_view(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let pawn = vm.obj_prop(c.this, "Pawn").unwrap_or(c.this);
    let loc = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
    let eye = match vm.get_property(pawn, "EyeHeight") {
        Some(Value::Float(h)) => *h,
        _ => match vm.get_property(pawn, "BaseEyeHeight") {
            Some(Value::Float(h)) => *h,
            _ => 0.0,
        },
    };
    let rot = match vm.get_property(c.this, "Rotation") {
        Some(Value::Rotator(r)) => *r,
        _ => match vm.get_property(pawn, "Rotation") {
            Some(Value::Rotator(r)) => *r,
            _ => [0; 3],
        },
    };
    if let Some(slot) = a.get_mut(0) {
        *slot = Value::Vector([loc[0], loc[1], loc[2] + eye]);
    }
    if let Some(slot) = a.get_mut(1) {
        *slot = Value::Rotator(rot);
    }
    val(Value::Void)
}

/// `PlayerController.AdjustAimForDisplay(object<Ammunition> FiredAmmunition, struct<Vector> projStart) -> Rotator`
/// (native 498).
///
/// Decoded from Engine.dll (all measured, ImageBase 0x10300000): the export
/// `?execAdjustAimForDisplay@APlayerController@@QAEXAAUFFrame@@QAX@Z` registers native 0x1f2 with
/// thunk slot VA 0x1051aac8 whose .data slot holds the implementation VA 0x1036e2e0
/// (`execCalcFirstPersonView`, native 497, resolves the same way to 0x10368340 and is already
/// implemented). Structure at 0x1036e2e0:
/// - null `FiredAmmunition` fast path -> convert `Rotation` (controller+0xd8) and call
///   `APlayerController::SmoothedAim(FRotator)` (0x1036c720) -> Result;
/// - target-scan path gated on `[controller+0x3b0] > [controller+0x3ac]` (scan cooldown) and
///   `byte [ammunition+0x270] & 0x10 == 0`; the scan (camera axes from the WeaponBob helper,
///   FCheckResult init Item=None/1.0f/-1, target loop, vector->rotator) ends in
///   `SmoothedAim(<snapped rotator>)` -> Result;
/// - every epilogue writes `SmoothedAim(...)` into the Result buffer.
///
/// `SmoothedAim` blends the engine's internal smoothed-aim cache (controller+0x5c0..+0x5fc,
/// display state the VM does not model); with no snap applied it converges to the input rotation.
/// The snap exists only for the crosshair display: the script consumer
/// (`xiii.XIIIPlayerInteraction.MyPCPostRender` 0x032D) feeds the returned rotator into the
/// crosshair ray (`FiringTargHitLoc`) and `AmmoType.WarnTarget` — the un-snapped view rotation is
/// the faithful unsnapped value there. Partial: no aim-assist snap decode (the ~0x800-byte target
/// loop is presentation-only in the headless VM).
fn adjust_aim_for_display(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let rot = match vm.get_property(c.this, "Rotation") {
        Some(Value::Rotator(r)) => *r,
        _ => [0; 3],
    };
    val(Value::Rotator(rot))
}

fn replace_texture_by_another(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let src = object(vm, a, 0)?.map(|r| vm.obj_label(&r));
    let dst = object(vm, a, 1)?.map(|r| vm.obj_label(&r));
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::ReplaceTexture {
        actor,
        source: src,
        destination: dst,
        time,
    });
    val(Value::Void)
}

fn refresh_displaying(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::RefreshDisplaying { actor, time });
    val(Value::Void)
}

fn set_injured_effect(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let new_state = boolean(vm, a, 0)?;
    let delay = if c.omitted(1) { 0.0 } else { float(vm, a, 1)? };
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::SetInjuredEffect {
        actor,
        new_state,
        delay,
        time,
    });
    val(Value::Void)
}

fn attach_projector(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::ProjectorAttach { actor, time });
    val(Value::Void)
}

fn detach_projector(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let force = if c.omitted(0) {
        false
    } else {
        boolean(vm, a, 0)?
    };
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::ProjectorDetach { actor, force, time });
    val(Value::Void)
}

fn abandon_projector(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let lifetime = if c.omitted(0) { 0.0 } else { float(vm, a, 0)? };
    let actor = vm.objects[c.this as usize].name.clone();
    let time = vm.time;
    vm.emit_event(PresentationEvent::ProjectorAbandon {
        actor,
        lifetime,
        time,
    });
    val(Value::Void)
}

fn set_owner(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let owner = object(vm, a, 0)?;
    vm.set_property(c.this, "Owner", 0, Value::Object(owner));
    val(Value::Void)
}

fn is_player_pawn(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `APawn::IsPlayerPawn` (upstream): the pawn's `Controller` is a `PlayerController`.
    // The old class-chain test `IsA('PlayerPawn')` was always false in XIII: Engine.u has no
    // `PlayerPawn` class (measured; the player pawn chain is
    // `xiiiplayerpawn <- xiiipawn <- pawn <- actor <- object`), so every authored
    // `IsPlayerPawn()` branch took the AI path. Measured consequence: `Weapon.GiveTo`'s
    // ammo-fill guard read `!IsPlayerPawn()` as true and ran `AmmoType.AddAmmo(ReloadCount)`
    // for the player, and `Weapon.GiveAmmo`'s zero-fill lost its player branch. The engine's
    // own marker is `Controller.bIsPlayer` (default false on `Controller`, true on
    // `PlayerController`, measured), which also covers controller classes that predate the
    // class-chain check.
    let player = match vm.obj_prop(c.this, "Controller") {
        Some(ctrl) => {
            vm.bool_prop(ctrl, "bIsPlayer")
                || vm.objects.get(ctrl as usize).is_some_and(|o| {
                    o.layout
                        .chain_names
                        .iter()
                        .any(|n| n.eq_ignore_ascii_case("playercontroller"))
                })
        }
        None => false,
    };
    val(Value::Bool(player))
}

fn find_inventory_type(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // `APawn::FindInventoryType(DesiredClass)`: walks `Inventory` -> `Inventory` and returns the
    // first item of **exactly** `DesiredClass`. XIII's decoded one-argument signature has no
    // `bExactClass` flag, and every decoded call site relies on exact matching (`XIIIGameInfo`
    // finds `XIIIThingsToSave`/`XIIILeftHand`, `XIIIItems.Transfer` finds a duplicate of
    // `self.Class`, `BaseSoldier` finds matching ammo). Treating it as an `IsA` subclass match was
    // wrong and made `Plage01CahuteKey` (a `XIII.Keys` subclass) swallow the truck key in
    // `XIIIItems.Transfer`'s duplicate branch, so the plage01 truck door could never be unlocked
    // (measured: search left the player with no `Keys` and `use Porte1` stayed Locked).
    let desired = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => g,
        _ => return val(Value::Object(None)),
    };
    let mut cur = prop_object(vm, c.this, "Inventory");
    let mut guard = 0;
    while let Some(id) = cur {
        guard += 1;
        if guard > 65_536 {
            break;
        }
        if vm.objects[id as usize].class == desired {
            return val(Value::Object(Some(ObjRef::Instance(id))));
        }
        cur = prop_object(vm, id, "Inventory");
    }
    val(Value::Object(None))
}

/// `Object.Cross_VectorVector` (native 220): `A x B` (UE1 `FVector` cross product). Needed by
/// `XIIIPorte.PlayerTriggerToggle.PlayerTrigger` and `Mover.EncroachingOn` to pick the swing side.
fn cross_vv(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let x = vector2(vm, a, 0)?;
    let y = vector2(vm, a, 1)?;
    val(Value::Vector([
        x[1] * y[2] - x[2] * y[1],
        x[2] * y[0] - x[0] * y[2],
        x[0] * y[1] - x[1] * y[0],
    ]))
}

/// `Actor.GetBoundingBox` (native 419): the actor's collision extent as a `Box` struct. XIII's
/// `XIIIPorte.PlayerTriggerToggle.BeginState` uses it to compute the door direction. **Partial**:
/// the VM has no mesh/pre-pivot bounds, so the box is the collision cylinder's extent centred on
/// `Location`; a door whose mesh centre is offset from its origin therefore reads a zero
/// direction (documented, not hidden).
fn get_bounding_box(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let loc = vm.vector_prop(c.this, "Location").unwrap_or([0.0; 3]);
    let r = vm.f32_prop(c.this, "CollisionRadius");
    let h = vm.f32_prop(c.this, "CollisionHeight");
    val(Value::Struct(vec![
        (
            "min".into(),
            Value::Vector([loc[0] - r, loc[1] - r, loc[2] - h]),
        ),
        (
            "max".into(),
            Value::Vector([loc[0] + r, loc[1] + r, loc[2] + h]),
        ),
        // UE2 `FBox` also carries `IsValid` (a byte; `AActor::GetBoundingBox` constructs the box
        // valid). Script reads it as `cast<byte->int>(Box.IsValid)` before using the bounds
        // (`xidcine.BreakableMover.ComputeDispersal` 0x0012, `xidmaps.Map06_HualparBase.StartSnow`
        // 0x0077); omitting it made those reads fail with "no struct member isvalid".
        ("isvalid".into(), Value::Byte(1)),
    ]))
}

fn find_inventory_kind(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `APawn::FindInventoryKind`: walks the `Inventory` -> `Inventory` chain and returns the
    // first item whose class chain contains `DesiredClassName`. Declaration measured from
    // engine.u (`engine.Pawn.FindInventoryKind [native f1000] (name, out Inventory)`); needed by
    // `XIIIPorte.Locked.Trigger`'s `FindInventoryKind('PickLockSkill')` test.
    let desired = name(vm, a, 0)?;
    let mut cur = prop_object(vm, c.this, "Inventory");
    let mut guard = 0;
    while let Some(id) = cur {
        guard += 1;
        if guard > 65_536 {
            break;
        }
        if vm.is_a(id, &desired) {
            return val(Value::Object(Some(ObjRef::Instance(id))));
        }
        cur = prop_object(vm, id, "Inventory");
    }
    val(Value::Object(None))
}

fn play_rolloff_sound(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.emit_rolloff_sound(c.this, a, &c.omitted);
    val(Value::Void)
}

fn set_base(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `AActor::SetBase`: sets `Base` (and `Floor` when supplied). No attachment transform
    // is evaluated (the VM has no rendering/movement solver).
    let base = object(vm, a, 0)?;
    vm.set_property(c.this, "Base", 0, Value::Object(base));
    if !c.omitted(1)
        && let Some(Value::Vector(v)) = a.get(1)
    {
        vm.set_property(c.this, "Floor", 0, Value::Vector(*v));
    }
    val(Value::Void)
}

fn set_relative_location(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let loc = vector2(vm, a, 0)?;
    vm.set_property(c.this, "RelativeLocation", 0, Value::Vector(loc));
    val(Value::Bool(true))
}

fn set_relative_rotation(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let rot = rotator2(vm, a, 0)?;
    vm.set_property(c.this, "RelativeRotation", 0, Value::Rotator(rot));
    val(Value::Bool(true))
}

fn set_draw_type(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let dt = match a.first() {
        Some(Value::Byte(b)) => *b,
        Some(other) => return Err(type_err(vm, "byte", other)),
        None => 0,
    };
    vm.set_property(c.this, "DrawType", 0, Value::Byte(dt));
    val(Value::Void)
}

fn set_draw_scale(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = float(vm, a, 0)?;
    vm.set_property(c.this, "DrawScale", 0, Value::Float(s));
    val(Value::Void)
}

fn set_draw_scale3d(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let s = vector2(vm, a, 0)?;
    vm.set_property(c.this, "DrawScale3D", 0, Value::Vector(s));
    val(Value::Void)
}

fn attach_to_bone(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `AActor::AttachToBone(AActor* Attachment, FName BoneName)`: the attachment is based on
    // this actor and remembers the bone. The VM records the base link and the bone name; no
    // skeletal attachment transform is evaluated.
    let Some(ObjRef::Instance(id)) = object(vm, a, 0)? else {
        return val(Value::Bool(false));
    };
    vm.set_property(id, "Base", 0, Value::Object(Some(ObjRef::Instance(c.this))));
    if !c.omitted(1)
        && let Some(Value::Name(n)) = a.get(1)
    {
        vm.set_property(id, "AttachmentBone", 0, Value::Name(n.clone()));
    }
    val(Value::Bool(true))
}

fn anim_blend_to_alpha(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let stage = int(vm, a, 0)?;
    let target = float(vm, a, 1)?;
    let time = float(vm, a, 2)?;
    vm.anim_blend_to_alpha(c.this, stage, target, time);
    val(Value::Void)
}

fn controlled_by_player(vm: &Vm<'_>, id: ObjectId) -> bool {
    vm.obj_prop(id, "Controller")
        .is_some_and(|c| vm.is_a(c, "PlayerController"))
}

fn is_locally_controlled(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _: &mut [Value],
) -> VmResult<NativeOutcome> {
    // No network/local-player model in the headless VM: a PlayerController is the only
    // possible local controller (labelled Partial).
    val(Value::Bool(controlled_by_player(vm, c.this)))
}

fn is_human_controlled(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(controlled_by_player(vm, c.this)))
}

/// `int` property value, or `0` when absent/another type.
fn int_prop(vm: &Vm<'_>, id: ObjectId, name: &str) -> i32 {
    match vm.get_property(id, name) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

fn ammunition_has_ammo(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Bool(int_prop(vm, c.this, "AmmoAmount") > 0))
}

fn weapon_has_ammo(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `AWeapon::HasAmmo`: an AmmoType exists with ammo left.
    val(Value::Bool(
        vm.obj_prop(c.this, "AmmoType")
            .is_some_and(|a| int_prop(vm, a, "AmmoAmount") > 0),
    ))
}

fn get_anim_params(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // Read the requested channel, independent of the actor's last started sequence.
    let ch = channel(vm, a, 0, c.omitted(0))?;
    let name = Value::Name(
        vm.anim_channel_sequence(c.this, ch)
            .unwrap_or("None")
            .to_owned(),
    );
    let (frame, rate) = vm.anim_channel_params(c.this, ch).unwrap_or((0.0, 0.0));
    if a.len() > 1 {
        a[1] = name;
    }
    if a.len() > 2 {
        a[2] = Value::Float(frame);
    }
    if a.len() > 3 {
        a[3] = Value::Float(rate);
    }
    val(Value::Void)
}

fn adjust_counter(vm: &mut Vm<'_>, c: &NativeCtx, name: &str, delta: i32) -> NativeOutcome {
    vm.adjust_music_var(c.this, name, delta);
    NativeOutcome::Value(Value::Void)
}

fn inc_attente(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    Ok(adjust_counter(vm, c, "NbAttente", 1))
}

fn dec_attente(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    Ok(adjust_counter(vm, c, "NbAttente", -1))
}

fn inc_alerte(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    Ok(adjust_counter(vm, c, "NbAlerte", 1))
}

fn dec_alerte(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    Ok(adjust_counter(vm, c, "NbAlerte", -1))
}

/// `LevelInfo.GetLocalURL`: the local URL the runtime loaded the map with (`<Map>?<options>`).
/// The string is runtime configuration (`Vm::set_local_url`); the engine reads it from
/// `ULevel::URL` (UE2 `ALevelInfo::GetLocalURL`). `XIIIPlayerController.SetInitialState` uses
/// `Left(GetLocalURL(), 7) ~= "mapmenu"` to detect the menu map.
fn get_local_url(vm: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Str(vm.local_url().to_owned()))
}

/// `LevelInfo.GetAddressURL`: the `Host:Port` address form of the loaded URL, runtime-configured
/// (`Vm::set_address_url`). Engine.dll `?execGetAddressURL@ALevelInfo` formats the URL host and
/// port with the literal `%s:%i`; for the GOG single-player install `[URL] Host=` is empty and
/// `Port=7777`, so the runtime supplies `:7777`.
fn get_address_url(vm: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Str(vm.address_url().to_owned()))
}

fn noop(vm: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let _ = vm;
    val(Value::Void)
}

/// Object id stored in a property, if any (a deleted actor counts as absent).
fn prop_object(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(ObjRef::Instance(i)))) if !vm.objects[*i as usize].deleted => {
            Some(*i)
        }
        _ => None,
    }
}

/// Raw object-pointer read for the engine's native list maintenance. It does not hide destroyed
/// actors: the engine's list natives compare and copy raw `AActor*` values and `execContext` has
/// no `bDeleteMe` test, so a just-destroyed node keeps its stale link (item25).
fn prop_object_raw(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(ObjRef::Instance(i)))) => Some(*i),
        _ => None,
    }
}

/// `Level.PawnList`/`Level.ControllerList` insert-at-head and unlink, shared by the pawn and
/// controller list natives (UE2 `APawn::AddPawnToList`/`RemovePawnFromList` and the controller
/// equivalents).
///
/// The engine works on raw `AActor*` links, and `execContext` has no `bDeleteMe` test, so a plain
/// property read already sees a destroyed node exactly as the engine does (see
/// [`Vm::bypass_context_none`] and [`Vm::destroy`]): the item-25 change that runs `Destroyed`
/// before `bDeleteMe` makes the old `raw_prop_object` workaround unnecessary. These helpers use
/// the ordinary property read (which is never gated on `deleted`), and the script-level reads in
/// `XIIIGameInfo.EndGame`/`Destroyed` go through the VM's context path.
///
/// Engine.dll `AController::execAddController` (0x10367a80..0x10367ab5): `NextController (0x214)
/// = Level (0x7c)->ControllerList (0x450); Level->ControllerList = this`. The pawn variant is
/// the same shape on `PawnList (0x454)`/`NextPawn (0x4cc)`.
fn list_insert_head(
    vm: &mut Vm<'_>,
    obj: ObjectId,
    level: ObjectId,
    list: &str,
    next: &str,
) -> VmResult<()> {
    let head = prop_object_raw(vm, level, list);
    vm.set_property(obj, next, 0, Value::Object(head.map(ObjRef::Instance)));
    vm.set_property(level, list, 0, Value::Object(Some(ObjRef::Instance(obj))));
    Ok(())
}

/// Engine.dll `AController::execRemoveController` (0x10367ac0..0x10367b21) and
/// `APawn::execRemovePawnFromList` (0x103b02e0..0x103b0341): if `Level->List == this` the head
/// becomes `this.Next`; otherwise walk the raw links to the predecessor and set
/// `pred.Next = this.Next`. All comparisons are raw pointers (a destroyed node is still linked
/// and still matched), and `this.Next` is never cleared, so a script loop that removes the
/// current node and then advances with `P = P.NextController` (XIIIGameInfo.EndGame +0x02A8..
/// +0x0336, where `GotoState('GameEnded')` destroys AI controllers) still reaches the rest.
///
/// The engine dereferences `this.Level` (0x7c) without a null check; with no `Level` the VM has
/// no list to edit, so it records a visible note and leaves every link unchanged.
fn list_remove(vm: &mut Vm<'_>, obj: ObjectId, list: &str, next: &str) -> VmResult<()> {
    let Some(level) = prop_object_raw(vm, obj, "Level") else {
        let actor = vm.objects[obj as usize].name.clone();
        vm.note(TraceKind::Note(format!(
            "{list} unlink of {actor}: Level is None (the engine dereferences Level unconditionally); links left unchanged"
        )));
        return Ok(());
    };
    let successor = prop_object_raw(vm, obj, next);
    if prop_object_raw(vm, level, list) == Some(obj) {
        vm.set_property(
            level,
            list,
            0,
            Value::Object(successor.map(ObjRef::Instance)),
        );
        return Ok(());
    }
    let mut cur = prop_object_raw(vm, level, list);
    let mut guard = 0;
    while let Some(c) = cur {
        guard += 1;
        if guard > 65_536 {
            break;
        }
        let after = prop_object_raw(vm, c, next);
        if after == Some(obj) {
            vm.set_property(c, next, 0, Value::Object(successor.map(ObjRef::Instance)));
            break;
        }
        cur = after;
    }
    Ok(())
}

/// Level for the list natives, or a visible note when it is None (the engine dereferences
/// `Level` (0x7c) unconditionally in all four natives).
fn list_level(vm: &mut Vm<'_>, obj: ObjectId, list: &str) -> Option<ObjectId> {
    let level = prop_object_raw(vm, obj, "Level");
    if level.is_none() {
        let actor = vm.objects[obj as usize].name.clone();
        vm.note(TraceKind::Note(format!(
            "{list} insert of {actor}: Level is None (the engine dereferences Level unconditionally); links left unchanged"
        )));
    }
    level
}

fn add_pawn_to_list(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    if let Some(level) = list_level(vm, c.this, "PawnList") {
        list_insert_head(vm, c.this, level, "PawnList", "NextPawn")?;
    }
    val(Value::Void)
}

fn remove_pawn_from_list(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _: &mut [Value],
) -> VmResult<NativeOutcome> {
    list_remove(vm, c.this, "PawnList", "NextPawn")?;
    val(Value::Void)
}

fn add_controller(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    if let Some(level) = list_level(vm, c.this, "ControllerList") {
        list_insert_head(vm, c.this, level, "ControllerList", "NextController")?;
    }
    val(Value::Void)
}

fn remove_controller(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    list_remove(vm, c.this, "ControllerList", "NextController")?;
    val(Value::Void)
}

fn dynamic_actors(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => Some(g),
        None => None,
        Some(ObjRef::Instance(_)) | Some(ObjRef::External(_)) => {
            return Err(vm.err(VmErrorKind::Other(
                "DynamicActors base class is an instance".into(),
            )));
        }
    };
    let tag = match a.get(2) {
        Some(Value::Name(n)) if !n.eq_ignore_ascii_case("None") => Some(n.clone()),
        _ => None,
    };
    let items: Vec<Value> = vm
        .dynamic_actors(base, tag.as_deref())
        .into_iter()
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(
        items.into_iter().map(|v| vec![v]).collect(),
    ))
}

const UE2_OP: &str = "UE2 operator semantics; declaration and index decoded from core.u; Core.dll exports the operator thunks";

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

/// `Object.GetAxes` (native 229): fill the rotator's orthonormal basis into the out params
/// X (forward), Y (right), Z (up), the same `FRotationMatrix` basis as `vector >> rotator`.
/// Decoded call site `engine.Pawn.TossWeapon` 0x0014.
fn get_axes(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let r = rotator2(vm, a, 0)?;
    let (x, y, z) = rotator_basis(r);
    if a.len() >= 4 {
        a[1] = Value::Vector(x);
        a[2] = Value::Vector(y);
        a[3] = Value::Vector(z);
    }
    val(Value::Void)
}

/// `Actor.StopAnimating` (native 417): stop the actor's animation (all channels).
fn stop_animating(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.stop_animating(c.this);
    val(Value::Void)
}

/// Records a `LevelInfo` snow-particle native call as a visible trace note. XIII drives its
/// snow through the `RndCubeSpr` particle system (`xidmaps.Map06_HualparBase.StartSnow`); this
/// runtime has no particle renderer, so the request is recorded rather than silently accepted.
fn snow_note(vm: &mut Vm<'_>, name: &str, a: &[Value]) {
    let args: Vec<String> = a.iter().map(|v| vm.value_text(v)).collect();
    vm.note(crate::vm::TraceKind::Log(format!(
        "LevelInfo.{name}({}): recorded; no particle subsystem",
        args.join(", ")
    )));
}

/// `ParticleEmitter.SetMaxParticles` (name-based native, index 0): a visible trace note. The
/// runtime has no particle renderer, so the request is recorded rather than silently accepted
/// or left as a missing native. Call site `xidcine.BreakableMover.InitializeEmitters` 0x0348
/// (reached once the emitter subobject exists).
fn set_max_particles(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let n = int(vm, a, 0)?;
    vm.note(crate::vm::TraceKind::Note(format!(
        "ParticleEmitter.SetMaxParticles({n}): recorded; no particle subsystem"
    )));
    val(Value::Void)
}

/// `ParticleEmitter.SpawnParticle(int Amount)`: keep the request attached to the VM-owned
/// subobject; the presentation host consumes it once and injects that many particles into its
/// simulator. Called by `xidcine.Shells.TriggerParticle` on the m60/kalash fire path.
fn particle_spawn(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let amount = int(vm, a, 0)?;
    if amount > 0 {
        vm.spawn_particles(c.this, amount as usize);
    }
    val(Value::Void)
}

/// item18: `Actor.KillAllSounds()` (native 0, name-based). No audio device; accepted and
/// discarded like [`actor_stop_all_sounds`](crate::canvas). `XIII.XIIIPlayerController
/// .PlayingVideo.BeginState` calls it before `VideoPlayer.Play`, so without it the level-end
/// controller suspends and `PlayingVideo.PlayerTick` (the `ServerTravel`) never runs.
fn kill_all_sounds(_vm: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Void)
}

/// item18: `Object.Normalize(Rotator) -> Rotator` (native 198). UE2 `Normalize` wraps each
/// rotator component into `0..65535`. Reached by the HUD weapon draw
/// (`XIIIWeapon.RenderOverlays`) while `--play` renders the first-person m60, which otherwise
/// aborts `PostRender` every frame.
fn normalize_rot(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let r = rotator2(vm, a, 0)?;
    val(Value::Rotator([
        r[0].rem_euclid(65536),
        r[1].rem_euclid(65536),
        r[2].rem_euclid(65536),
    ]))
}

/// item18 natives: small VM gaps found while driving the Plage01 route (the m60/kalash/m16 fire
/// path and the level-end `PlayingVideo` state). Kept in one labelled block so parallel registry
/// edits stay out of the way.
fn item18_defs() -> Vec<NativeDef> {
    vec![
        def(
            "Object.Percent_FloatFloat",
            "native(173) final native operator float %(float A, float B)",
            "core.u Object.Percent_FloatFloat decoded; UE2 appFmod (C fmod); xiii.m60.RumbleFX 0x004E (ReloadCount % 1) on the fire path",
            percent_ff,
        ),
        def(
            "ParticleEmitter.SpawnParticle",
            "native(0) native function SpawnParticle(int Amount)",
            "engine.u ParticleEmitter.SpawnParticle; xidcine.Shells.TriggerParticle 0x006C on the m60/kalash fire path",
            particle_spawn,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "no audio device: the call is accepted and discarded (the VM has no mixer)",
            ),
            ..def(
                "Actor.KillAllSounds",
                "native(0) final native static function KillAllSounds()",
                "engine.u Actor.KillAllSounds; xiii.XIIIPlayerController.PlayingVideo.BeginState 0x0000 before VideoPlayer.Play",
                kill_all_sounds,
            )
        },
        def(
            "Engine.Actor.TraceActors",
            "native(309) final iterator function TraceActors(class<Actor> BaseClass, out Actor Actor, out vector HitLoc, out vector HitNorm, vector End, optional vector Start, optional vector Extent)",
            "Actor.TraceActors UE2 iterator: trace actor cylinders in segment order and return per-hit Actor/HitLoc/HitNorm; requires world trace provider",
            actor_trace_actors,
        ),
        def(
            "Object.Normalize",
            "native(198) final native static function Rotator Normalize(Rotator Rot)",
            "core.u Object.Normalize decoded; xiii.XIIIWeapon.RenderOverlays (m60 first-person draw) 0x0391",
            normalize_rot,
        ),
    ]
}

fn init_rnd_cube_spr(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    snow_note(vm, "InitRndCubeSpr", a);
    val(Value::Void)
}

fn set_rnd_cube_spr_size(
    vm: &mut Vm<'_>,
    _: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    snow_note(vm, "SetRndCubeSprSize", a);
    val(Value::Void)
}

fn set_rnd_cube_spr_speed(
    vm: &mut Vm<'_>,
    _: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    snow_note(vm, "SetRndCubeSprSpeed", a);
    val(Value::Void)
}

fn add_rnd_cube_spr_exclude(
    vm: &mut Vm<'_>,
    _: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    snow_note(vm, "AddRndCubeSprExclude", a);
    val(Value::Void)
}

fn set_rnd_cube_spr_state(
    vm: &mut Vm<'_>,
    _: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    snow_note(vm, "SetRndCubeSprState", a);
    val(Value::Void)
}

fn change_rnd_cube_spr_prop(
    vm: &mut Vm<'_>,
    _: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    snow_note(vm, "ChangeRndCubeSprProp", a);
    // No particle system to change: report that the change was not applied (never a silent
    // success).
    val(Value::Bool(false))
}

// ---- item14c: bullet-trail natives on the weapon-attachment fire path. `XIIIWeaponAttachment
// .Timer2` spawns the weapon's `TraceClass` and calls `Trail.Init` then `Trail.AddSection` for
// each traced point. Before item14c the unimplemented `Trail.Init` suspended `BerettaAttach`, and
// the VM then deferred every call to the suspended attachment (including the
// `WeaponAttachment(ThirdPersonActor).ThirdPersonEffects()` call from `IncrementFlashCount`), so
// the game's own muzzle-flash chain never ran. The trail itself is presentation only; this
// runtime has no trail renderer, so `Init` records the request in the trace and the sections are
// accepted and dropped. The attachment therefore stays active and its game-side effects run.
fn trail_init(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let owner = vm
        .objects
        .get(c.this as usize)
        .map(|o| o.name.clone())
        .unwrap_or_else(|| "?".to_owned());
    vm.note(TraceKind::Note(format!(
        "Trail.Init on {owner}: recorded; no trail renderer (attachment kept active)"
    )));
    val(Value::Void)
}

/// `Trail.AddSection` (native 603): append a traced point to the trail. No trail renderer exists,
/// so the point is dropped (the native still returns normally). Not recorded per call: a fired
/// burst emits several sections and the per-shot `Trail.Init` note above already makes the gap
/// visible.
fn trail_add_section(_: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Void)
}

/// `Trail.Reset` (name-based native, index 0): clear the trail's sections. No trail renderer
/// exists, so there is nothing to clear; accepted and dropped so the attachment stays active.
fn trail_reset(_: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    val(Value::Void)
}

/// Headless counterpart of `UInteraction::Initialize`: native viewport/input registration is
/// unavailable, while the script-owned AddInteraction sequence still owns array membership,
/// Master, MyPC and Level. Keep this operation explicit in traces instead of silently accepting
/// an unknown native.
fn interaction_initialize(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _: &mut [Value],
) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(format!(
        "{}: headless Interaction.Initialize (no viewport input device)",
        vm.objects[c.this as usize].name
    )));
    val(Value::Void)
}

/// Force-feedback devices are not exposed by the headless VM; retain the controller state and
/// make the unavailable hardware side effect visible in the trace.
fn force_feedback_enable(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    a: &mut [Value],
) -> VmResult<NativeOutcome> {
    let enabled = boolean(vm, a, 0)?;
    vm.note(TraceKind::Note(format!(
        "{}: headless ForceFeedbackController.EnableForceFeedback({enabled}) (device unavailable)",
        vm.objects[c.this as usize].name
    )));
    val(Value::Void)
}

fn force_feedback_is_enabled(
    vm: &mut Vm<'_>,
    c: &NativeCtx,
    _: &mut [Value],
) -> VmResult<NativeOutcome> {
    vm.note(TraceKind::Note(format!(
        "{}: headless ForceFeedbackController.IsForceFeedbackEnable=false (device unavailable)",
        vm.objects[c.this as usize].name
    )));
    val(Value::Bool(false))
}

fn builtin_defs() -> Vec<NativeDef> {
    let mut v = vec![
        NativeDef {
            status: NativeStatus::Partial("headless runtime has no force-feedback device"),
            ..def(
                "ForceFeedbackController.EnableForceFeedback",
                "native(0) function EnableForceFeedback(bool bEnable)",
                "Engine.ForceFeedbackController native declaration; device output unavailable in headless runtime",
                force_feedback_enable,
            )
        },
        NativeDef {
            status: NativeStatus::Partial("headless runtime has no force-feedback device"),
            ..def(
                "ForceFeedbackController.IsForceFeedbackEnable",
                "native(0) function bool IsForceFeedbackEnable()",
                "Engine.ForceFeedbackController native declaration; no device reports disabled",
                force_feedback_is_enabled,
            )
        },
        NativeDef {
            status: NativeStatus::Partial("headless viewport/input registration unavailable"),
            ..def(
                "Interaction.Initialize",
                "native(0) function Interaction.Initialize()",
                "Engine.Interaction.Initialize declaration; headless runtime supplies Player/Console/InteractionMaster but no UViewport input device",
                interaction_initialize,
            )
        },
        def(
            "Object.Not_PreBool",
            "native(129) preoperator bool !(bool A)",
            UE2_OP,
            not_b,
        ),
        NativeDef {
            short_circuit: Some(false),
            ..def(
                "Object.AndAnd_BoolBool",
                "native(130) operator(30) bool &&(bool A, skip bool B)",
                "UE2 short-circuit: B is a Skip token, not evaluated when A is false",
                and_bb,
            )
        },
        NativeDef {
            short_circuit: Some(true),
            ..def(
                "Object.OrOr_BoolBool",
                "native(132) operator(32) bool ||(bool A, skip bool B)",
                "UE2 short-circuit: B is a Skip token, not evaluated when A is true",
                or_bb,
            )
        },
        def(
            "Object.EqualEqual_BoolBool",
            "native(242) bool ==(bool, bool)",
            UE2_OP,
            eq_bb,
        ),
        def(
            "Object.NotEqual_BoolBool",
            "native(243) bool !=(bool, bool)",
            UE2_OP,
            ne_bb,
        ),
        def(
            "Object.Subtract_PreInt",
            "native(143) preoperator int -(int)",
            UE2_OP,
            neg_i,
        ),
        def(
            "Object.Multiply_IntInt",
            "native(144) int *(int, int)",
            UE2_OP,
            mul_ii,
        ),
        def(
            "Object.Divide_IntInt",
            "native(145) int /(int, int)",
            "UE2; division by zero is an error here",
            div_ii,
        ),
        def(
            "Object.Add_IntInt",
            "native(146) int +(int, int)",
            UE2_OP,
            add_ii,
        ),
        def(
            "Object.Subtract_IntInt",
            "native(147) int -(int, int)",
            UE2_OP,
            sub_ii,
        ),
        def(
            "Object.Less_IntInt",
            "native(150) bool <(int, int)",
            UE2_OP,
            lt_ii,
        ),
        def(
            "Object.Greater_IntInt",
            "native(151) bool >(int, int)",
            UE2_OP,
            gt_ii,
        ),
        def(
            "Object.LessEqual_IntInt",
            "native(152) bool <=(int, int)",
            UE2_OP,
            le_ii,
        ),
        def(
            "Object.GreaterEqual_IntInt",
            "native(153) bool >=(int, int)",
            UE2_OP,
            ge_ii,
        ),
        def(
            "Object.EqualEqual_IntInt",
            "native(154) bool ==(int, int)",
            UE2_OP,
            eq_ii,
        ),
        def(
            "Object.NotEqual_IntInt",
            "native(155) bool !=(int, int)",
            UE2_OP,
            ne_ii,
        ),
        def(
            "Object.And_IntInt",
            "native(156) final operator int &(int, int)",
            "UE2 bitwise AND; Core.dll operator thunk",
            and_ii,
        ),
        def(
            "Object.AddAdd_PreInt",
            "native(163) preoperator int ++(out int A)",
            UE2_OP,
            preinc_i,
        ),
        def(
            "Object.SubtractSubtract_PreInt",
            "native(164) preoperator int --(out int A)",
            UE2_OP,
            predec_i,
        ),
        def(
            "Object.AddAdd_Int",
            "native(165) postoperator int ++(out int A)",
            "UE2 postfix ++: returns the old value",
            postinc_i,
        ),
        def(
            "Object.SubtractSubtract_Int",
            "native(166) postoperator int --(out int A)",
            "UE2 postfix --: returns the old value",
            postdec_i,
        ),
        def(
            "Object.Subtract_PreFloat",
            "native(169) preoperator float -(float)",
            UE2_OP,
            neg_f,
        ),
        def(
            "Object.Multiply_FloatFloat",
            "native(171) float *(float, float)",
            UE2_OP,
            mul_ff,
        ),
        def(
            "Object.Divide_FloatFloat",
            "native(172) float /(float, float)",
            "UE2; division by zero is an error here",
            div_ff,
        ),
        def(
            "Object.Add_FloatFloat",
            "native(174) float +(float, float)",
            UE2_OP,
            add_ff,
        ),
        def(
            "Object.Subtract_FloatFloat",
            "native(175) float -(float, float)",
            UE2_OP,
            sub_ff,
        ),
        def(
            "Object.Less_FloatFloat",
            "native(176) bool <(float, float)",
            UE2_OP,
            lt_ff,
        ),
        def(
            "Object.Greater_FloatFloat",
            "native(177) bool >(float, float)",
            UE2_OP,
            gt_ff,
        ),
        def(
            "Object.LessEqual_FloatFloat",
            "native(178) bool <=(float, float)",
            UE2_OP,
            le_ff,
        ),
        def(
            "Object.GreaterEqual_FloatFloat",
            "native(179) bool >=(float, float)",
            UE2_OP,
            ge_ff,
        ),
        def(
            "Object.EqualEqual_FloatFloat",
            "native(180) bool ==(float, float)",
            UE2_OP,
            eq_ff,
        ),
        def(
            "Object.NotEqual_FloatFloat",
            "native(181) bool !=(float, float)",
            UE2_OP,
            ne_ff,
        ),
        def(
            "Object.Concat_StrStr",
            "native(112) string $(string, string)",
            UE2_OP,
            concat_ss,
        ),
        def(
            "Object.Left",
            "native(128) final static function string Left(coerce string S, int i)",
            "UE2 FString::Left: first i chars, i clamped to [0,Len]; Core.dll ?execLeft@UObject",
            left_ss,
        ),
        def(
            "Object.Right",
            "native(234) final static function string Right(coerce string S, int i)",
            "UE2 FString::Right: last i chars, i clamped to [0,Len]; Core.dll ?execRight@UObject",
            right_ss,
        ),
        def(
            "Object.Mid",
            "native(127) final static function string Mid(coerce string S, int i, optional int j)",
            "UE2 FString::Mid: start i and end i+j clamped unsigned to [0,Len], j omitted = rest; Core.dll ?execMid@UObject",
            mid_ssi,
        ),
        def(
            "Object.Len",
            "native(125) final static function int Len(coerce string S)",
            "UE2: character count; Core.dll ?execLen@UObject",
            len_s,
        ),
        def(
            "Object.InStr",
            "native(126) final static function int InStr(coerce string S, coerce string t)",
            "UE2 execInStr uses appStrstr (case-sensitive), -1 when absent; Core.dll ?execInStr@UObject",
            instr_ss,
        ),
        def(
            "Object.Caps",
            "native(235) final static function string Caps(coerce string S)",
            "UE2: uppercase; Core.dll ?execCaps@UObject",
            caps_s,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "resolves names already loaded in the script set; does not read packages from disk (no filesystem in the VM)",
            ),
            ..def(
                "Object.DynamicLoadObject",
                "native(205) final static function object DynamicLoadObject(string ObjectName, class<Object> ObjectClass, optional bool MayFail)",
                "engine.u call site `DynamicLoadObject(X, class'Core.Class')`; UE2 resolves a package object by name; the VM has no filesystem so only loaded objects resolve",
                dynamic_load_object,
            )
        },
        def(
            "Object.ClassIsChildOf",
            "native(258) final static function bool ClassIsChildOf(class<Object> TestClass, class<Object> ParentClass)",
            "UE2: class-chain containment; Core.dll ?execClassIsChildOf@UObject",
            class_is_child_of,
        ),
        def(
            "Object.Localize",
            "native(199) final native static function string Localize(string SectionName, string KeyName, string PackageName)",
            "Core.dll ?execLocalize@UObject RVA 0x1DD40 -> ?Localize RVA 0x281A0; miss placeholder \"<?%s?%s.%s.%s?>\" at VA 0x10179574",
            localize_native,
        ),
        def(
            "Object.ComplementEqual_StrStr",
            "native(124) final operator bool ~=(string, string)",
            "UnrealScript `~=` is case-insensitive equality (appStricmp==0), same result as string ==; Core.dll operator thunk",
            complement_equal_ss,
        ),
        def(
            "Object.FMax",
            "native(245) final static function float FMax(float A, float B)",
            "UE2: larger of two floats; Core.dll ?execFMax@UObject",
            fmax_ff,
        ),
        def(
            "Object.FMin",
            "native(244) final static function float FMin(float A, float B)",
            "UE2: smaller of two floats; Core.dll ?execFMin@UObject",
            fmin_ff,
        ),
        def(
            "Object.FClamp",
            "native(246) final static function float FClamp(float V, float A, float B)",
            "UE2: bounds V into [min(A,B), max(A,B)]; Core.dll ?execFClamp@UObject",
            fclamp_fff,
        ),
        def(
            "Object.Min",
            "native(249) final static function int Min(int A, int B)",
            "UE2: smaller of two ints; Core.dll ?execMin@UObject",
            min_ii,
        ),
        def(
            "Object.Max",
            "native(250) final static function int Max(int A, int B)",
            "UE2: larger of two ints; Core.dll ?execMax@UObject",
            max_ii,
        ),
        def(
            "Object.Lerp",
            "native(247) final static function float Lerp(float Alpha, float A, float B)",
            "UE2: A + (B-A)*Alpha; Core.dll ?execLerp@UObject",
            lerp_fff,
        ),
        def(
            "Object.Abs",
            "native(186) final static function float Abs(float A)",
            "UE2: absolute value; Core.dll ?execAbs@UObject",
            abs_f,
        ),
        def(
            "Object.Sqrt",
            "native(193) final static function float Sqrt(float A)",
            "UE2: square root; Core.dll ?execSqrt@UObject",
            sqrt_f,
        ),
        def(
            "Object.Exp",
            "native(191) final static function float Exp(float A)",
            "UE2: e^A; Core.dll ?execExp@UObject",
            exp_f,
        ),
        def(
            "Object.Loge",
            "native(192) final static function float Loge(float A)",
            "UE2: natural logarithm; Core.dll ?execLoge@UObject",
            loge_f,
        ),
        def(
            "Object.Sin",
            "native(187) final static function float Sin(float A)",
            "UE2: sine (radians); Core.dll ?execSin@UObject",
            sin_f,
        ),
        def(
            "Object.Cos",
            "native(188) final static function float Cos(float A)",
            "UE2: cosine (radians); Core.dll ?execCos@UObject",
            cos_f,
        ),
        def(
            "Object.Tan",
            "native(189) final static function float Tan(float A)",
            "UE2: tangent (radians); Core.dll ?execTan@UObject",
            tan_f,
        ),
        def(
            "Object.Atan",
            "native(190) final static function float Atan(float A)",
            "UE2: arctangent; Core.dll ?execAtan@UObject",
            atan_f,
        ),
        def(
            "Object.AddEqual_IntInt",
            "native(161) final operator out int +=(out int A, int B)",
            UE2_OP,
            add_eq_ii,
        ),
        def(
            "Object.SubtractEqual_IntInt",
            "native(162) final operator out int -=(out int A, int B)",
            UE2_OP,
            sub_eq_ii,
        ),
        def(
            "Object.MultiplyEqual_IntFloat",
            "native(159) final operator out int *=(out int A, float B)",
            UE2_OP,
            mul_eq_if,
        ),
        def(
            "Object.DivideEqual_IntFloat",
            "native(160) final operator out int /=(out int A, float B)",
            "UE2; division by zero is an error here",
            div_eq_if,
        ),
        def(
            "Object.AddEqual_FloatFloat",
            "native(184) final operator out float +=(out float A, float B)",
            UE2_OP,
            add_eq_ff,
        ),
        def(
            "Object.SubtractEqual_FloatFloat",
            "native(185) final operator out float -=(out float A, float B)",
            UE2_OP,
            sub_eq_ff,
        ),
        def(
            "Object.MultiplyEqual_FloatFloat",
            "native(182) final operator out float *=(out float A, float B)",
            UE2_OP,
            mul_eq_ff,
        ),
        def(
            "Object.DivideEqual_FloatFloat",
            "native(183) final operator out float /=(out float A, float B)",
            "UE2; division by zero is an error here",
            div_eq_ff,
        ),
        def(
            "Object.AddEqual_VectorVector",
            "native(223) final operator out vector +=(out vector A, vector B)",
            UE2_OP,
            add_eq_vv,
        ),
        def(
            "Object.SubtractEqual_VectorVector",
            "native(224) final operator out vector -=(out vector A, vector B)",
            UE2_OP,
            sub_eq_vv,
        ),
        def(
            "Object.MultiplyEqual_VectorFloat",
            "native(221) final operator out vector *=(out vector A, float B)",
            UE2_OP,
            mul_eq_vf,
        ),
        def(
            "Object.DivideEqual_VectorFloat",
            "native(222) final operator out vector /=(out vector A, float B)",
            "UE2; division by zero is an error here",
            div_eq_vf,
        ),
        def(
            "Object.MultiplyEqual_VectorVector",
            "native(297) final operator out vector *=(out vector A, vector B)",
            "UE2: component-wise product; Core.dll operator thunk",
            mul_eq_vv,
        ),
        def(
            "Object.AddEqual_RotatorRotator",
            "native(318) final operator out rotator +=(out rotator A, rotator B)",
            UE2_OP,
            add_eq_rr,
        ),
        def(
            "Object.SubtractEqual_RotatorRotator",
            "native(319) final operator out rotator -=(out rotator A, rotator B)",
            UE2_OP,
            sub_eq_rr,
        ),
        def(
            "Object.MultiplyEqual_RotatorFloat",
            "native(290) final operator out rotator *=(out rotator A, float B)",
            UE2_OP,
            mul_eq_rf,
        ),
        def(
            "Object.DivideEqual_RotatorFloat",
            "native(291) final operator out rotator /=(out rotator A, float B)",
            "UE2; division by zero is an error here",
            div_eq_rf,
        ),
        def(
            "Object.Clamp",
            "native(251) final static function int Clamp(int V, int A, int B)",
            "UE2: bounds V into [min(A,B), max(A,B)]; Core.dll ?execClamp@UObject",
            clamp_iii,
        ),
        def(
            "Object.Multiply_FloatVector",
            "native(213) final operator vector *(float, vector)",
            "UE2: component-wise scale; Core.dll operator thunk",
            multiply_fv,
        ),
        def(
            "Object.FRand",
            "native(195) final static function float FRand()",
            "Core.dll execFRand 0x1011a950 -> appFrand 0x1010ffd0: MSVCR70 rand * float 0x38000100 (inclusive 0..1); CRT rand 0x7c02836d uses 32-bit LCG 214013*s+2531011, bits 16..30",
            f_rand,
        ),
        def(
            "Object.Rand",
            "native(167) final static function int Rand(int Max)",
            "Core.dll execRand 0x101197f0: Max<=0 returns 0 without consuming RNG; otherwise same CRT rand stream modulo Max",
            rand_i,
        ),
        def(
            "Engine.Actor.SetRotation",
            "native(299) final function bool SetRotation(rotator NewRotation)",
            "UE2 AActor::execSetRotation: sets Rotation (physics ignored headless); Engine.dll ?execSetRotation@AActor",
            set_rotation,
        ),
        def(
            "Object.Add_VectorVector",
            "native(215) final operator vector +(vector, vector)",
            UE2_OP,
            add_vv,
        ),
        def(
            "Object.Subtract_VectorVector",
            "native(216) final operator vector -(vector, vector)",
            UE2_OP,
            sub_vv,
        ),
        def(
            "Object.Multiply_VectorVector",
            "native(296) final operator vector *(vector, vector)",
            UE2_OP,
            mul_vv,
        ),
        def(
            "Object.Multiply_VectorFloat",
            "native(212) final operator vector *(vector, float)",
            UE2_OP,
            mul_vf,
        ),
        def(
            "Object.Divide_VectorFloat",
            "native(214) final operator vector /(vector, float)",
            UE2_OP,
            div_vf,
        ),
        def(
            "Object.EqualEqual_VectorVector",
            "native(217) final operator bool ==(vector, vector)",
            UE2_OP,
            eq_vv,
        ),
        def(
            "Object.Dot_VectorVector",
            "native(219) final operator float dot(vector, vector)",
            UE2_OP,
            dot_vv,
        ),
        def(
            "Object.VSize",
            "native(225) final static function float VSize(vector A)",
            "UE2: vector magnitude; Core.dll ?execVSize@UObject",
            vsize_v,
        ),
        def(
            "Object.Normal",
            "native(226) final static function vector Normal(vector A)",
            "UE2: unit vector (zero for a zero vector); Core.dll ?execNormal@UObject",
            normal_v,
        ),
        def(
            "Object.Add_RotatorRotator",
            "native(316) final operator rotator +(rotator, rotator)",
            UE2_OP,
            add_rr,
        ),
        def(
            "Object.Subtract_RotatorRotator",
            "native(317) final operator rotator -(rotator, rotator)",
            UE2_OP,
            sub_rr,
        ),
        def(
            "Object.GreaterGreater_VectorRotator",
            "native(276) final operator vector >>(vector A, rotator B)",
            "engine.xidcine.HelicoDeco.PostBeginPlay uses `Offset >> Rotation` for a local-to-world part offset; UE1 FRotationMatrix transform",
            greater_greater_vr,
        ),
        def(
            "Object.LessLess_VectorRotator",
            "native(275) final operator vector <<(vector A, rotator B)",
            "xidcine.HelicoDeco.PostBeginPlay uses `(Location-Linked.Location) << Rotation` for a world-to-local offset; UE1 inverse FRotationMatrix",
            less_less_vr,
        ),
        def(
            "Engine.Actor.SetPhysics",
            "native(3970) final function SetPhysics(byte<EPhysics> newPhysics)",
            "engine.u Actor.SetPhysics decoded; stores the EPhysics byte property (no native physics solver)",
            set_physics,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "iterates the live map actors in object order; skips deleted actors and class-default objects. bStatic is *not* skipped, matching execAllActors (only DynamicActors skips static)",
            ),
            ..def(
                "Engine.Actor.AllActors",
                "native(304) final iterator function AllActors(class<Actor> BaseClass, out Actor Actor, optional name MatchTag)",
                "UE2 AActor::execAllActors iterates every map actor of BaseClass including bStatic ones (only DynamicActors skips static); Engine.dll ?execAllActors@AActor",
                all_actors,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "iterates live actors within the radius of the caller's Location (or the optional Loc); excludes class-default objects, includes self; no visibility/line-of-sight test",
            ),
            ..def(
                "Engine.Actor.RadiusActors",
                "native(310) final iterator function RadiusActors(class<Actor> BaseClass, out Actor Actor, float Radius, optional vector Loc)",
                "engine.u Actor.RadiusActors decoded (BaseClass, Actor, Radius, Loc); UE1 AActor::execRadiusActors distance test; Engine.dll ?execRadiusActors@AActor",
                radius_actors,
            )
        },
        def(
            "Object.At_StrStr",
            "native(168) string @(string, string)",
            "UE2: concatenation with one space",
            at_ss,
        ),
        def(
            "Object.EqualEqual_StrStr",
            "native(122) bool ==(string, string)",
            "UE2: case-insensitive compare",
            eq_ss,
        ),
        def(
            "Object.NotEqual_StrStr",
            "native(123) bool !=(string, string)",
            "UE2: case-insensitive compare",
            ne_ss,
        ),
        def(
            "Object.EqualEqual_ObjectObject",
            "native(114) bool ==(object, object)",
            UE2_OP,
            eq_oo,
        ),
        def(
            "Object.NotEqual_ObjectObject",
            "native(119) bool !=(object, object)",
            UE2_OP,
            ne_oo,
        ),
        def(
            "Object.EqualEqual_NameName",
            "native(254) bool ==(name, name)",
            "UE2: name identity (case-insensitive)",
            eq_nn,
        ),
        def(
            "Object.NotEqual_NameName",
            "native(255) bool !=(name, name)",
            "UE2: name identity (case-insensitive)",
            ne_nn,
        ),
        def(
            "Object.GotoState",
            "native(113) final function GotoState(optional name NewState, optional name Label)",
            "UE2 UObject::GotoState: EndState on the old state, label (default Begin) located in the new state's label table, BeginState; Core.dll ?execGotoState@UObject",
            goto_state,
        ),
        def(
            "Object.IsA",
            "native(303) final function bool IsA(name ClassName)",
            "UE2: class chain name test; Core.dll ?execIsA@UObject",
            is_a,
        ),
        def(
            "Object.IsInState",
            "native(281) final static function bool IsInState(name TestState)",
            "UE2: true when the current state or a super state has the name (None matches no state); Core.dll ?execIsInState@UObject",
            is_in_state,
        ),
        def(
            "Object.GetStateName",
            "native(284) final static function name GetStateName()",
            "UE2: name of the current state (None when stateless); Core.dll ?execGetStateName@UObject",
            get_state_name,
        ),
        def(
            "Object.Disable",
            "native(118) final function Disable(name ProbeFunc)",
            "UE2: probe mask bit cleared; engine events for that probe are dropped; Core.dll ?execDisable@UObject",
            disable,
        ),
        def(
            "Object.Enable",
            "native(117) final function Enable(name ProbeFunc)",
            "UE2: probe re-enabled; Core.dll ?execEnable@UObject",
            enable,
        ),
        def(
            "Engine.Actor.Sleep",
            "native(256) final latent function Sleep(float Seconds)",
            "UE2 AActor::execSleep + execPollSleep (finishes when remaining < 0.5*dt); Engine.dll ?execSleep / ?execPollSleep@AActor",
            sleep,
        ),
        def(
            "Engine.Actor.SetTimer",
            "native(280) final function SetTimer(float NewTimerRate, bool bLoop)",
            "UE2 AActor::execSetTimer: Timer event after the rate, repeating if bLoop; Engine.dll ?execSetTimer@AActor",
            set_timer,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "no collision/encroachment check at the spawn location; abstract-class refusal uses an unverified class-flag bit",
            ),
            ..def(
                "Engine.Actor.Spawn",
                "native(278) final static function Actor Spawn(class<Actor> SpawnClass, optional object<Actor> SpawnOwner, optional name SpawnTag, optional vector SpawnLocation, optional rotator SpawnRotation)",
                "engine.u Actor.Spawn decoded params; UE2 ULevel::SpawnActor semantics (Owner/Tag defaults, Location/Rotation from spawner); Engine.dll ?execSpawn@AActor",
                spawn,
            )
        },
        def(
            "Engine.Actor.Destroy",
            "native(279) final function bool Destroy()",
            "engine.u Actor.Destroy decoded; UE2 AActor::execDestroy (Destroyed event, bDeleteMe, references become None); Engine.dll ?execDestroy@AActor",
            destroy,
        ),
        def(
            "Engine.Actor.SetOwner",
            "native(272) final static function SetOwner(object<Actor> NewOwner)",
            "engine.u Actor.SetOwner decoded (NewOwner, void); UE2 AActor::execSetOwner sets Owner; Engine.dll ?execSetOwner@AActor",
            set_owner,
        ),
        def(
            "Engine.Pawn.IsPlayerPawn",
            "native(0) final function bool IsPlayerPawn()",
            "engine.u Pawn.IsPlayerPawn decoded (bool); UE2 upstream: the Controller is a PlayerController (Controller.bIsPlayer, measured default true only on PlayerController)",
            is_player_pawn,
        ),
        def(
            "Engine.Pawn.FindInventoryType",
            "native(0) final function Inventory FindInventoryType(class<Object> DesiredClass)",
            "engine.u Pawn.FindInventoryType decoded (DesiredClass, Inventory); UE2 walks the Inventory->Inventory chain and returns the first deriving item",
            find_inventory_type,
        ),
        def(
            "Engine.Pawn.IsLocallyControlled",
            "native(0) simulated function bool IsLocallyControlled()",
            "engine.u Pawn.IsLocallyControlled decoded (bool); no local-player/network model, true when the Controller is a PlayerController (Partial)",
            is_locally_controlled,
        ),
        def(
            "Engine.Pawn.IsHumanControlled",
            "native(0) simulated function bool IsHumanControlled()",
            "engine.u Pawn.IsHumanControlled decoded (bool); true when the Controller is a PlayerController (Partial)",
            is_human_controlled,
        ),
        def(
            "Engine.Ammunition.HasAmmo",
            "native(0) function bool HasAmmo()",
            "engine.u Ammunition.HasAmmo decoded (bool); UE2: AmmoAmount > 0",
            ammunition_has_ammo,
        ),
        def(
            "Engine.Weapon.HasAmmo",
            "native(0) function bool HasAmmo()",
            "engine.u Weapon.HasAmmo decoded (bool); UE2: AmmoType != None && AmmoType.AmmoAmount > 0",
            weapon_has_ammo,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "requested channel sequence and normalized frame/rate supported; velocity-derived negative rates and exact interrupted tween/global cache semantics remain unimplemented",
            ),
            ..def(
                "Engine.Actor.GetAnimParams",
                "native(396) final static function GetAnimParams(int Channel, out name OutSeqName, out float OutAnimFrame, out float OutAnimRate)",
                "engine.u Actor.GetAnimParams decoded; fills the requested channel sequence, normalized frame (negative during tween) and normalized rate; channel-zero Actor mirrors stay independent of higher stages",
                get_anim_params,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "updates the `MusicVars` counter named NbAttente (hypothesis: the counter name is inferred from the LevelInfo defaults; the DLL setter is not decoded)",
            ),
            ..def(
                "Engine.LevelInfo.IncAttente",
                "native(593) final native static function IncAttente()",
                "engine.u LevelInfo.IncAttente decoded (void); increments the MusicVars counter NbAttente",
                inc_attente,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "updates the `MusicVars` counter named NbAttente (hypothesis: the counter name is inferred from the LevelInfo defaults; the DLL setter is not decoded)",
            ),
            ..def(
                "Engine.LevelInfo.DecAttente",
                "native(592) final native static function DecAttente()",
                "engine.u LevelInfo.DecAttente decoded (void); decrements the MusicVars counter NbAttente",
                dec_attente,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "updates the `MusicVars` counter named NbAlerte (hypothesis: the counter name is inferred from the LevelInfo defaults; the DLL setter is not decoded)",
            ),
            ..def(
                "Engine.LevelInfo.IncAlerte",
                "native(591) final native static function IncAlerte()",
                "engine.u LevelInfo.IncAlerte decoded (void); increments the MusicVars counter NbAlerte",
                inc_alerte,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "updates the `MusicVars` counter named NbAlerte (hypothesis: the counter name is inferred from the LevelInfo defaults; the DLL setter is not decoded)",
            ),
            ..def(
                "Engine.LevelInfo.DecAlerte",
                "native(590) final native static function DecAlerte()",
                "engine.u LevelInfo.DecAlerte decoded (void); decrements the MusicVars counter NbAlerte",
                dec_alerte,
            )
        },
        def(
            "Engine.LevelInfo.GetLocalURL",
            "native(0) simulated native function string GetLocalURL()",
            "engine.u LevelInfo.GetLocalURL decoded (return string); returns the runtime-configured local URL `<Map>?<options>` (Vm::set_local_url; UE2 ALevelInfo::GetLocalURL reads ULevel::URL)",
            get_local_url,
        ),
        def(
            "Engine.LevelInfo.GetAddressURL",
            "native(0) simulated native function string GetAddressURL()",
            "engine.u LevelInfo.GetAddressURL decoded (return string); returns the runtime-configured `Host:Port` (Vm::set_address_url); Engine.dll ?execGetAddressURL@ALevelInfo formats it with the literal `%s:%i`",
            get_address_url,
        ),
        def(
            "Engine.Actor.PlayRolloffSound",
            "native(350) final static function PlayRolloffSound(object<Sound> Sound, object<Actor> RollOffActor, optional int Param1, optional int Param2, optional int Param3, optional int Param4, optional int Param5)",
            "engine.u Actor.PlayRolloffSound decoded (Sound, RollOffActor + five optional ints, void); emits PresentationEvent::PlayRolloffSound",
            play_rolloff_sound,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "decoded CheckNoiseHearing/CanHear (noise slots, ControllerList walk, IsProbing, tag/player filter, HearingThreshold^2*Loudness*Alertness range, eye line trace); zone hearing is not modelled and the muffled/around-corner BSP-only traces use the world trace (trace note once per VM)",
            ),
            ..def(
                "Engine.Actor.MakeNoise",
                "native(512) final native static function MakeNoise(float Loudness)",
                "engine.u Actor.MakeNoise decoded (float Loudness; native 512); Engine.dll execMakeNoise 0x103aff40, CheckNoiseHearing 0x1036b6d0, CanHear 0x1036b0f0 (item41c)",
                make_noise,
            )
        },
        def(
            "Engine.Actor.SetBase",
            "native(298) final static function SetBase(object<Actor> NewBase, optional vector NewFloor)",
            "engine.u Actor.SetBase decoded (NewBase, optional NewFloor, void); sets Base and Floor; no attachment transform (headless)",
            set_base,
        ),
        def(
            "Engine.Canvas.MakeColor",
            "native(274) final static function Color MakeColor(byte R, byte G, byte B, optional byte A)",
            "engine.u Canvas.MakeColor decoded (three/four bytes -> Color struct; A defaults to 255); UE2 FColor constructor; called by PlayerController.ClearProgressMessages",
            make_color,
        ),
        def(
            "Engine.Actor.SetRelativeLocation",
            "native(420) final static function bool SetRelativeLocation(vector NewLocation)",
            "engine.u Actor.SetRelativeLocation decoded (NewLocation, bool); sets RelativeLocation; returns true (headless)",
            set_relative_location,
        ),
        def(
            "Engine.Actor.SetRelativeRotation",
            "native(421) final static function bool SetRelativeRotation(rotator NewRotation)",
            "engine.u Actor.SetRelativeRotation decoded (NewRotation, bool); sets RelativeRotation; returns true (headless)",
            set_relative_rotation,
        ),
        def(
            "Engine.Actor.SetDrawType",
            "native(422) final static function SetDrawType(byte<EDrawType> NewDrawType)",
            "engine.u Actor.SetDrawType decoded (byte, void); stores the DrawType property (no renderer)",
            set_draw_type,
        ),
        def(
            "Engine.Actor.SetDrawScale",
            "native(424) final static function SetDrawScale(float NewScale)",
            "engine.u Actor.SetDrawScale decoded (float, void); stores the DrawScale property",
            set_draw_scale,
        ),
        def(
            "Engine.Actor.SetDrawScale3D",
            "native(423) final static function SetDrawScale3D(vector NewScale3D)",
            "engine.u Actor.SetDrawScale3D decoded (vector, void); stores the DrawScale3D property",
            set_draw_scale3d,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "renderer follows evaluated bones and relative transforms; mesh attachment aliases and VM collision/location propagation remain unimplemented",
            ),
            ..def(
                "Engine.Actor.AttachToBone",
                "native(404) final static function bool AttachToBone(object<Actor> Attachment, name BoneName)",
                "engine.u Actor.AttachToBone decoded (Attachment, BoneName, bool); bases attachment on self; play renderer evaluates bone coordinates with RelativeLocation/RelativeRotation; attach aliases and VM collision following remain unimplemented",
                attach_to_bone,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "advances alpha in seconds and preserves subtree; zero/negative intervals and invalid stages still need original-engine validation",
            ),
            ..def(
                "Engine.Actor.AnimBlendToAlpha",
                "native(411) final static function AnimBlendToAlpha(int Stage, float TargetAlpha, float TimeInterval)",
                "engine.u Actor.AnimBlendToAlpha decoded (Stage, TargetAlpha, TimeInterval, void); UE2 blends a channel's alpha over time",
                anim_blend_to_alpha,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "world move via physics provider; cylinder blocking dispatches Bump to the blocking actor, then refreshes Touch/UnTouch; encroachment events and exact native ordering remain unverified; player/projectile bBlockPlayers pairing inferred from class names",
            ),
            ..def(
                "Engine.Actor.Move",
                "native(266) final function bool Move(vector Delta)",
                "engine.u Actor.Move decoded (Delta, bool); UE2 ULevel::MoveActor / SurrealEngine UActor::Move=TryMove(delta).Fraction==1.0 (true when the full delta was applied); Engine.dll ?execMove@AActor",
                actor_move,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "world placement via provider point_free (only when bCollideWorld||bCollideWhenPlacing) and a blocking-actor cylinder encroachment refusal; no FindSpot search; touch updates unconditional (upstream gates them on Level.bBegunPlay)",
            ),
            ..def(
                "Engine.Actor.SetLocation",
                "native(267) final function bool SetLocation(vector NewLocation)",
                "engine.u Actor.SetLocation decoded (NewLocation, bool); UE1/UE2 AActor::SetLocation via CheckLocation plus touch updates (SurrealEngine UActor::SetLocation); Engine.dll ?execSetLocation@AActor",
                actor_set_location,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "AdditionalTraceType is ORed into actor category flags; actor hits use ray-vs-grown-cylinder; Material out-param is None and DiscardedHitMask 0 (no material/hit-mask model); special world filtering bits remain unmodelled; mover geometry comes from the physics provider",
            ),
            ..def(
                "Engine.Actor.Trace",
                "native(277) final function Actor Trace(out vector HitLocation, out vector HitNormal, vector TraceEnd, optional vector TraceStart, optional bool bTraceActors, optional vector Extent, optional out object<Material> Material, optional int AdditionalTraceType, optional out int DiscardedHitMask)",
                "engine.u Actor.Trace decoded params; UE1 227 // = Location / = bCollideActors / extent defaults and LevelInfo-for-world-hit (SurrealEngine UActor::Trace / CollisionSystem::TraceFirstHit); Engine.dll ?execTrace@AActor",
                actor_trace,
            )
        },
        def(
            "Engine.Actor.FastTrace",
            "native(548) final function bool FastTrace(vector TraceEnd, optional vector TraceStart, optional int AdditionalTraceType, optional out int DiscardedHitMask)",
            "engine.u Actor.FastTrace decoded; UE1 227 'returns true if did not hit world geometry' (actors ignored); SurrealEngine UActor::FastTrace = !TraceAnyHit(.., traceActors=false, traceWorld=true); Engine.dll ?execFastTrace@AActor",
            actor_fast_trace,
        ),
        def(
            "Engine.Actor.SetCollision",
            "native(262) final function SetCollision(optional bool NewColActors, optional bool NewBlockActors, optional bool NewBlockPlayers)",
            "engine.u Actor.SetCollision decoded (all optional); omitted flags keep their current value and touching is recomputed (SurrealEngine NActor::SetCollision); Engine.dll ?execSetCollision@AActor",
            actor_set_collision,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "always returns true; the UT469 optional bCheckEncroachment flag is absent from XIII's decoded declaration, so an internal encroachment check is not modelled",
            ),
            ..def(
                "Engine.Actor.SetCollisionSize",
                "native(283) final function bool SetCollisionSize(float NewRadius, float NewHeight)",
                "engine.u Actor.SetCollisionSize decoded (NewRadius, NewHeight, bool); updates the cylinder and recomputes touching; Engine.dll ?execSetCollisionSize@AActor",
                actor_set_collision_size,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "iterates the VM-maintained Touching array; XIII's Touching is a dynamic array (decoded ArrayProperty with a template element), unlike UE1/UT's fixed 4-slot engine array",
            ),
            ..def(
                "Engine.Actor.TouchingActors",
                "native(307) final iterator function TouchingActors(class<Actor> BaseClass, out Actor Actor)",
                "engine.u Actor.TouchingActors decoded; UE1 227 'returns all actors touching the current actor'; Engine.dll ?execTouchingActors@AActor",
                actor_touching_actors,
            )
        },
        def(
            "Engine.Pawn.AddPawnToList",
            "native(0) final native function AddPawnToList()",
            "engine.u Pawn.AddPawnToList (1-byte native stub, name-bound index 0); UE2 inserts self at the head of Level.PawnList via Pawn.NextPawn",
            add_pawn_to_list,
        ),
        def(
            "Engine.Pawn.RemovePawnFromList",
            "native(0) final native function RemovePawnFromList()",
            "engine.u Pawn.RemovePawnFromList (1-byte native stub, name-bound index 0); UE2 unlinks self from Level.PawnList",
            remove_pawn_from_list,
        ),
        def(
            "Engine.Controller.AddController",
            "native(529) final native function AddController()",
            "engine.u Controller.AddController; UE2 inserts self at the head of Level.ControllerList via Controller.NextController",
            add_controller,
        ),
        def(
            "Engine.Controller.RemoveController",
            "native(530) final native function RemoveController()",
            "engine.u Controller.RemoveController; UE2 unlinks self from Level.ControllerList",
            remove_controller,
        ),
        NativeDef {
            status: NativeStatus::Partial("no-op: network channel notifications are not modelled"),
            ..def(
                "Engine.Actor.EnableChannelNotify",
                "native(415) final function EnableChannelNotify(int Channel, int Switch)",
                "engine.u Actor.EnableChannelNotify decoded; UE2 registers a channel notification (networking)",
                noop,
            )
        },
    ];
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "iterates a snapshot in object (map export) order; skips actors whose bStatic is true",
        ),
        ..def(
            "Engine.Actor.DynamicActors",
            "native(313) final iterator function DynamicActors(class<Actor> BaseClass, out Actor Actor, optional name MatchTag)",
            "UE2 AActor::execDynamicActors (non-static actors of BaseClass, Tag == MatchTag); Engine.dll ?execDynamicActors@AActor",
            dynamic_actors,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial("records the message in the trace; optional tag ignored"),
        ..def(
            "Object.Log",
            "native(231) final static function Log(coerce string S, optional name Tag)",
            "UE2: writes to the log; Core.dll ?execLog@UObject",
            log,
        )
    });
    v.push(def(
        "Engine.Actor.LinkSkelAnim",
        "native(413) final function LinkSkelAnim(object<MeshAnimation> Anim)",
        "engine.u Actor.LinkSkelAnim decoded; adds the MeshAnimation to the actor's animation sources (the VM keeps the render Mesh separate and remembers the path for sequence lookups)",
        link_skel_anim,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "shared CPU evaluator blends stages in ascending order over named subtrees with normalized InTime; OutTime use not found in PC GetFrame; special cached/global pose paths unimplemented",
        ),
        ..def(
            "Engine.Actor.AnimBlendParams",
            "native(412) final function AnimBlendParams(int Stage, float BlendAlpha, float InTime, float OutTime, name BoneName)",
            "engine.u Actor.AnimBlendParams decoded (Stage, BlendAlpha, InTime, OutTime, BoneName; no bGlobalPose unlike UT2003/2004); UE2 stores the values on the skeletal-mesh animation channel (MeshAnimChannel), BoneName selects a bone subtree",
            anim_blend_params,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "authored rate multiplier, last-frame completion, frozen-source tween and CPU channel sampling; velocity-dependent negative rates, automatic TweenTime and callback reentrancy remain unimplemented",
        ),
        ..def(
            "Engine.Actor.PlayAnim",
            "native(259) final function PlayAnim(name Sequence, float Rate, float TweenTime, int Channel)",
            "engine.u Actor.PlayAnim decoded (Sequence, Rate, TweenTime, Channel); UE1 AActor::PlayAnim (non-looping); animation data via Vm::set_animation_data",
            play_anim,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "authored rate multiplier, last-frame completion, frozen-source tween and CPU channel sampling; velocity-dependent negative rates, automatic TweenTime and callback reentrancy remain unimplemented",
        ),
        ..def(
            "Engine.Actor.LoopAnim",
            "native(260) final function LoopAnim(name Sequence, float Rate, float TweenTime, int Channel)",
            "engine.u Actor.LoopAnim decoded (Sequence, Rate, TweenTime, Channel); Engine.dll UpdateAnimation (loop AnimEnd at final frame; wrap at N)",
            loop_anim,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "frozen-source pose tween to frame zero then hold, with AnimEnd at tween completion; original cached-pose/global-pose optimizations and automatic negative TweenTime unimplemented",
        ),
        ..def(
            "Engine.Actor.TweenAnim",
            "native(294) final function TweenAnim(name Sequence, float Time, int Channel)",
            "engine.u Actor.TweenAnim decoded (Sequence, Time, Channel); UE1 AActor::TweenAnim tween-in",
            tween_anim,
        )
    });
    v.push(def(
        "Engine.Actor.IsAnimating",
        "native(282) final function bool IsAnimating(int Channel)",
        "engine.u Actor.IsAnimating decoded (Channel, bool); true while the channel has an active sequence",
        is_animating,
    ));
    v.push(def(
        "Engine.Actor.HasAnim",
        "native(263) final function bool HasAnim(name Sequence)",
        "engine.u Actor.HasAnim decoded (Sequence, bool); asks the AnimationData provider for the linked mesh",
        has_anim,
    ));
    v.push(def(
        "Engine.Actor.FinishAnim",
        "native(261) final latent function FinishAnim(int Channel)",
        "engine.u Actor.FinishAnim decoded (Channel); latent: suspends state code until the channel reaches AnimEnd (immediate when not animating)",
        finish_anim,
    ));
    v.push(def(
        "Engine.PlayerController.SetViewTarget",
        "native(513) final function SetViewTarget(object<Actor> NewViewTarget)",
        "engine.u PlayerController.SetViewTarget decoded (NewViewTarget); sets ViewTarget (no camera/rendering)",
        set_view_target,
    ));
    // Presentation natives: the VM is headless, so these enqueue a typed PresentationEvent and
    // return the decoded declaration's value (void). Consumed with Vm::drain_events.
    v.push(def(
        "Engine.Actor.PlaySound",
        "native(264) final static function PlaySound(object<Sound> Sound, optional int Param1, optional int Param2, optional int Param3, optional int Param4, optional int Param5)",
        "engine.u Actor.PlaySound decoded (Sound + five optional ints, void); emits PresentationEvent::PlaySound",
        play_sound,
    ));
    v.push(def(
        "Engine.Actor.PlayMusic",
        "native(358) final static function PlayMusic(object<Sound> Sound, optional int Param1, optional int Param2, optional int Param3, optional int Param4, optional int Param5)",
        "engine.u Actor.PlayMusic decoded (Sound + five optional ints, void); emits PresentationEvent::PlayMusic",
        play_music,
    ));
    v.push(def(
        "Engine.Actor.ReplaceATextureByAnOther",
        "native(0) final static function ReplaceATextureByAnOther(object<Texture> SrcTexture, object<Texture> DestTexture)",
        "engine.u Actor.ReplaceATextureByAnOther decoded (two Texture objects, void); emits PresentationEvent::ReplaceTexture",
        replace_texture_by_another,
    ));
    v.push(def(
        "Engine.Actor.RefreshDisplaying",
        "native(0) final function RefreshDisplaying()",
        "engine.u Actor.RefreshDisplaying decoded (void); emits PresentationEvent::RefreshDisplaying",
        refresh_displaying,
    ));
    v.push(def(
        "Engine.LevelInfo.SetInjuredEffect",
        "native(0) simulated function SetInjuredEffect(bool NewState, float Delay)",
        "engine.u LevelInfo.SetInjuredEffect decoded (bool, float, void); emits PresentationEvent::SetInjuredEffect",
        set_injured_effect,
    ));
    v.push(def(
        "Engine.Projector.AttachProjector",
        "native(0) final function AttachProjector()",
        "engine.u Projector.AttachProjector decoded (void, no args); emits PresentationEvent::ProjectorAttach",
        attach_projector,
    ));
    v.push(def(
        "Engine.Projector.DetachProjector",
        "native(0) final function DetachProjector(optional bool Force)",
        "engine.u Projector.DetachProjector decoded (optional bool, void); emits PresentationEvent::ProjectorDetach",
        detach_projector,
    ));
    v.push(def(
        "Engine.Projector.AbandonProjector",
        "native(0) final function AbandonProjector(optional float Lifetime)",
        "engine.u Projector.AbandonProjector decoded (optional float, void); emits PresentationEvent::ProjectorAbandon",
        abandon_projector,
    ));
    // Pathing (Part item3f). The decoded declarations and indices come from the GOG engine.u;
    // semantics follow UE2 `AController` (upstream) and are marked `Partial` where the headless
    // VM cannot model the engine exactly. The navigation graph comes from `Vm::set_navigation`.
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "uniform direction from the VM's deterministic PRNG; the engine RNG sequence is not reproduced",
        ),
        ..def(
            "Object.VRand",
            "native(252) final static function vector VRand()",
            "core.u Object.VRand decoded (return Vector); UE2: random unit vector; Core.dll ?execVRand@UObject",
            vrand,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "dispatches the `Timer2` event; UE2 keeps the three timers independent, the VM keeps \
             one active slot per actor (a later SetTimer* replaces it)",
        ),
        ..def(
            "Engine.Actor.SetTimer2",
            "native(363) final function SetTimer2(float NewTimerRate, bool bLoop)",
            "engine.u Actor.SetTimer2 decoded (float, bool; identical declaration to SetTimer, but \
             fires the Timer2 event); Engine.dll ?execSetTimer2@AActor",
            set_timer2,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "dispatches the `Timer3` event; UE2 keeps the three timers independent, the VM keeps \
             one active slot per actor (a later SetTimer* replaces it)",
        ),
        ..def(
            "Controller.SetTimer3",
            "native(0) final function SetTimer3(float NewTimerRate, bool bLoop)",
            "engine.u Controller.SetTimer3 decoded (float, bool; fires the Timer3 event); \
             IAController.Attaque uses it for the enemy-position refresh",
            set_timer3,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "shortest path over decoded ReachSpecs filtered by the pawn's collision size and reach flags; the engine's path weighting is not reproduced",
        ),
        ..def(
            "Engine.Controller.FindPathToward",
            "native(517) final function Actor FindPathToward(actor anActor, bool bClearPaths)",
            "engine.u Controller.FindPathToward decoded; UE2 AController::FindPathToward builds RouteCache and returns the first path node (navig provider via Vm::set_navigation); Engine.dll ?execFindPathToward@AController",
            find_path_toward,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "shortest path over decoded ReachSpecs filtered by the pawn's collision size and reach flags; the engine's path weighting is not reproduced",
        ),
        ..def(
            "Engine.Controller.FindPathTo",
            "native(518) final function Actor FindPathTo(vector aPoint, bool bClearPaths)",
            "engine.u Controller.FindPathTo decoded; UE2 AController::FindPathTo; Engine.dll ?execFindPathTo@AController",
            find_path_to,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "picks a point with the VM's deterministic PRNG; the engine uses its own RNG",
        ),
        ..def(
            "Engine.Controller.FindRandomDest",
            "native(525) final function NavigationPoint FindRandomDest(bool bClearPaths)",
            "engine.u Controller.FindRandomDest decoded; UE2 AController::FindRandomDest; Engine.dll ?execFindRandomDest@AController",
            find_random_dest,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "clear pawn trace plus a decoded-graph path; the engine's point-reachability test is not reproduced exactly",
        ),
        ..def(
            "Engine.Controller.pointReachable",
            "native(521) final function bool pointReachable(vector aPoint)",
            "engine.u Controller.pointReachable decoded; UE2 AController::pointReachable; Engine.dll ?execpointReachable@AController",
            point_reachable,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "pointReachable on the actor's Location; ignores the actor's own collision volume",
        ),
        ..def(
            "Engine.Controller.actorReachable",
            "native(520) final function bool actorReachable(actor anActor)",
            "engine.u Controller.actorReachable decoded; UE2 AController::actorReachable; Engine.dll ?execactorReachable@AController",
            actor_reachable,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "decoded multi-line trace (base Location first, then Enemy eye point without distance \
             limits or 0.8*CollisionHeight top with 8000^2/2000^2 limits) from the view target; \
             actor occlusion not modelled, so blocked-by-the-target cannot count as visible",
        ),
        ..def(
            "Engine.Controller.LineOfSightTo",
            "native(514) final function bool LineOfSightTo(actor Other, return bool ReturnValue)",
            "engine.u Controller.LineOfSightTo decoded; Engine.dll ?LineOfSightTo@AController VA \
             0x1036ac70 and ?execLineOfSightTo@AController VA 0x1036d860 (item27m)",
            line_of_sight_to,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "decoded fractional Speed/DesiredSpeed, Acceleration, walking velocity and UpdateTactics polling; ReachedDestination margins, bAdjusting, non-walking modes and full physWalking volume/ledge/braking branches remain incomplete",
        ),
        ..def(
            "Engine.Controller.MoveTo",
            "native(500) final latent function MoveTo(vector NewDestination, optional actor ViewFocus, optional float Speed)",
            "Engine.dll execMoveTo VA 0x1036a6e0, poll 0x1036a8e0 -> APawn::moveToward 0x103b3950; physWalking 0x103bdac0 -> calcVelocity 0x103ba250; Speed default 1 is fractional",
            move_to,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "decoded fractional Speed, Focus/NextMoveTarget, moving-target/UpdateTactics polling and walking acceleration; navigation preparation/ReachSpec blending, bAdjusting, non-walking modes and full ReachedDestination/braking rules remain incomplete",
        ),
        ..def(
            "Engine.Controller.MoveToward",
            "native(502) final latent function MoveToward(actor NewTarget, optional actor ViewFocus, optional float Speed, optional actor NextTarget)",
            "Engine.dll execMoveToward VA 0x1036d8c0, poll 0x1036a9c0 updates Destination from MoveTarget.Location -> moveToward 0x103b3950",
            move_toward,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Implemented,
        ..def(
            "Engine.Controller.FinishRotation",
            "native(508) final latent function FinishRotation()",
            "engine.u Controller.FinishRotation decoded (void, latent); UE2 AController::FinishRotation waits for the pawn to face FocalPoint; Engine.dll ?execFinishRotation@AController",
            finish_rotation,
        )
    });
    // ---- item8b movers/doors: kept in their own block so a parallel AI-native edit merges
    // without touching these entries. -----------------------------------------------------------
    v.push(def(
        "Engine.Actor.FinishInterpolation",
        "native(301) final latent function FinishInterpolation()",
        "engine.u Actor.FinishInterpolation decoded (void, latent); every engine.Mover open/close \
         state calls it after DoOpen/DoClose and resumes when the brush reaches its key; the \
         per-tick PHYS_MovingBrush advance is Vm::advance_interpolation; \
         Engine.dll ?execFinishInterpolation@AActor",
        finish_interpolation,
    ));
    v.push(def(
        "Engine.Pawn.FindInventoryKind",
        "native(0) final function Inventory FindInventoryKind(name DesiredClassName)",
        "engine.u Pawn.FindInventoryKind decoded (name, out Inventory); UE2 walks the Inventory \
         chain and returns the first item whose class chain contains the name. Needed by \
         XIIIPorte.Locked.Trigger's FindInventoryKind('PickLockSkill') gate",
        find_inventory_kind,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "the VM has no mesh/pre-pivot bounds; the returned Box is the collision cylinder \
             extent centred on Location, so an offset mesh centre reads as zero",
        ),
        ..def(
            "Engine.Actor.GetBoundingBox",
            "native(419) final function Box GetBoundingBox()",
            "engine.u Actor.GetBoundingBox decoded (Box, return); XIIIPorte.PlayerTriggerToggle.\
             BeginState computes DoorDirection from it",
            get_bounding_box,
        )
    });
    v.push(def(
        "Object.Cross_VectorVector",
        "native(220) final operator vector Cross(vector A, vector B)",
        "core.u Object.Cross_VectorVector decoded; UE1 FVector cross product (A x B)",
        cross_vv,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "uses the RadiusActors distance filter, not a swept collision-cylinder test; the \
             mover timer re-checks FastTrace/vision on each result",
        ),
        ..def(
            "Engine.Actor.CollidingActors",
            "native(321) final iterator function CollidingActors(class<Actor> BaseClass, out Actor Actor, float Radius, optional vector Loc)",
            "engine.u Actor.CollidingActors decoded; UE2 returns actors whose collision cylinder \
             overlaps the caller's within Radius. Needed by XIIIMover.Timer (the door warning \
             scan) so a mover is not suspended on its own timer",
            colliding_actors,
        )
    });
    // XIII AI natives (xidpawn.u, implemented in XIDPawn.dll). Semantics from the export
    // disassembly; each entry cites its RVA and the report with the evidence.
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "the engine's anonymous Level early-out bool at Level+0x380 bit 0x100 is not named in the decoded reflection and is not modelled; the InitialAlliances table lookup is reproduced",
        ),
        ..def(
            "IAController.AllianceLevel",
            "native(0) function int AllianceLevel(Pawn Newenemy)",
            "XIDPawn.dll ?execAllianceLevel@AIAController RVA 0x19E0 -> ?AllianceLevel@AIAController RVA 0x1920 (returns -1 for self.XIII / null BaseS, else BaseS.InitialAlliances[i].AllianceLevel); see local/reports/item3g-xiii-ai-natives-re.md",
            alliance_level,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "bone-control + animation reset only; the engine's anonymous controller flag words/pointers (+0x2C/+0x212/+0x514/+0x48..+0x54) are not named in the decoded reflection and are not modelled",
        ),
        ..def(
            "IAController.HalteAuFeu",
            "native(0) function HalteAuFeu()",
            "XIDPawn.dll ?execHalteAuFeu@AIAController RVA 0x1F00 (no pawn = return; clears controller flag words, drops four pointers, clears Pawn+0x1F8 bit 0x10000, resets the skeletal-mesh bone controllers); see local/reports/item3g-xiii-ai-natives-re.md",
            halte_au_feu,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "single forward world line trace of length walldist from the top of the pawn; the engine's multi-trace projection and push-back vector are not reproduced",
        ),
        ..def(
            "IAController.NearWall",
            "native(0) function bool NearWall(float walldist)",
            "XIDPawn.dll ?execNearWall@AIAController RVA 0x2E00 (probes the world ahead of the pawn and returns whether geometry is close); see local/reports/item3g-xiii-ai-natives-re.md",
            near_wall,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "one line trace from the pawn toward Dir*dist; pick = hit location or the clear end point, and the result is |pick - Pawn.Location| >= mindist; the engine's second adjusted trace is not reproduced",
        ),
        ..def(
            "IAController.TestDirection",
            "native(0) function bool TestDirection(float mindist, float dist, Vector Dir, out Vector pick)",
            "XIDPawn.dll ?execTestDirection@AIAController RVA 0x36D0 (line-checks Dir*dist, writes pick, tests the mindist clearance); see local/reports/item3g-xiii-ai-natives-re.md",
            test_direction,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "nearest decoded navigation point that fits the pawn (fallback Controller.StartSpot); the engine's patrol-list/route-field search is not reproduced",
        ),
        ..def(
            "IAController.PickStartPoint",
            "native(0) function PatrolPoint PickStartPoint()",
            "XIDPawn.dll ?execPickStartPoint@AIAController RVA 0x3B80 (scans a level/Game point list for the nearest reachable point on the soldier's route); see local/reports/item3g-xiii-ai-natives-re.md",
            pick_start_point,
        )
    });
    // item14b: the combat-state AI natives. `incattaque` is the first native the attacked
    // soldier's `Attaque.BeginState` calls; the rest are used by the attack/steering states.
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "MusicVars[2].Value increments with wrapping; optional audio-device SetMusicVar and attack-mode transition (vtable +0xc4/+0xcc) not bridged",
        ),
        ..def(
            "Engine.LevelInfo.IncAttaque",
            "native(589) final native static function IncAttaque()",
            "Engine.dll execIncAttaque VA 0x103e4750 increments LevelInfo+0x3dc MusicVars index 2 +0x0c Value; IAController.s_incattaque calls it on Attaque.BeginState",
            level_info_inc_attaque,
        )
    });
    v.push(def(
        "Engine.LevelInfo.GetPlateForme",
        "native(0) native function int GetPlateForme()",
        "Engine.dll ?execGetPlateForme@ALevelInfo VA 0x103df410: returns GSys+0x7c ([Core.System] PlateForm, 0 in the shipped Default.ini)",
        level_info_get_plate_forme,
    ));
    // item19: complete the BaseSoldier.Died alert-level-2 path paired with item14b's IncAttaque.
    // Both operations update the authoritative MusicVars record, not a separate AI counter.
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "MusicVars[2].Value decrements without clamping; optional audio-device notification and delayed music-mode transition not bridged",
        ),
        ..def(
            "Engine.LevelInfo.DecAttaque",
            "native(588) final native static function DecAttaque()",
            "Engine.dll execDecAttaque VA 0x103e4870 decrements MusicVars[2].Value; BaseSoldier.Died calls it when IAController.NiveauALerte == 2",
            level_info_dec_attaque,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "unit vector from the pawn to Enemy.Location (pawn forward fallback); dispersion/aim \
             offset not reproduced",
        ),
        ..def(
            "IAController.DirectionDuTir",
            "native(0) function vector DirectionDuTir()",
            "XIDPawn.dll ?execDirectionDuTir@AIAController RVA 0x2070; IAController.NotifyFiring \
             stores it in DirectionTir",
            direction_du_tir,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "world-only line check between the points (same convention as \
             Controller.LineOfSightTo); actor occlusion not modelled",
        ),
        ..def(
            "IAController.LigneVisee",
            "native(0) function bool LigneVisee(vector TraceEnd, vector TraceStart)",
            "XIDPawn.dll ?execLigneVisee@AIAController RVA 0x2B10; IAController.Attaque.EnemyNotVisible \
             uses it to choose TacticalMove vs temporise",
            ligne_visee,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial("retail fight-list/equality gate and trace correction; Core.appRSqrt precision/zero behavior and engine TRACE_AllBlocking filtering not reproduced exactly; malformed null lists fail explicitly"),
        ..def(
            "IAController.PseudoSteering",
            "native(0) function vector PseudoSteering()",
            "XIDPawn.dll execPseudoSteering VA 0x11903230: GenAlerte+0x20c SoldierInFightList, inverse-distance separation; equality-only gate 0x119033a6, correction scale 50000 and output thresholds 50/4000 UU",
            pseudo_steering,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial("decoded 0/1/2 classification and Pote assignment; shared world provider cannot filter shoot-through triangle geometry, actor traces use cylinders, AllianceLevel's unnamed Level flag remains Partial"),
        ..def(
            "IAController.LineOfFireObstacle",
            "native(0) function int LineOfFireObstacle()",
            "XIDPawn.dll execLineOfFireObstacle VA 0x119040d0: WeaponStartTrace/WeaponEndTrace, AmmoType.bInstantHit selects 0x4083/0x8083; nonnegative AllianceLevel writes Pote and returns 1, nonpawn bCanSeeThrough controls 0/2",
            line_of_fire_obstacle,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
             "decoded wrapper sets MoveTarget/Destination only on success; shared ReachSpec search still differs from Engine.FindPath/findPathToward anchor selection, cost/capability checks and endpoint reachability",
        ),
        ..def(
            "IAController.FindBestPathTo",
            "native(0) function bool FindBestPathTo(vector desti)",
            "XIDPawn.dll ?execFindBestPathTo@AIAController RVA 0x3A90; IAController.Tenir.BackToFormation \
             and Attaque.Trigger use it",
            find_best_path_to,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial("decoded linked-list selection, distance bounds and LastSeenPos height; shared LineOfSightTo is world-only and Core.appRSqrt precision is not bit-identical"),
        ..def(
            "IAController.FindNewStakeOutDir",
            "native(0) function FindNewStakeOutDir()",
            "XIDPawn.dll execFindNewStakeOutDir VA 0x11903ea0: NavigationPointList/NextNavigationPoint, (100,800) UU, greatest strictly improving direction dot with LineOfSightTo(node,0); LastSeenPos(+0x24c)=node.Location+(0,0,CollisionHeight/2)",
            find_new_stake_out_dir,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "decoded SeePawn (VA 0x1036dc40): LOS-only for Enemy, else SightRadius/Visibility \
             range gate and the decoded |delta| > PeripheralVision check before LineOfSightTo; \
             actor occlusion not modelled",
        ),
        ..def(
            "Engine.Controller.CanSee",
            "native(533) final function bool CanSee(Pawn Other)",
            "engine.u Controller.CanSee decoded (533, object Other, return bool); Engine.dll \
             ?execCanSee@AController VA 0x1036f070 -> ?SeePawn@AController VA 0x1036dc40 (item27m)",
            controller_can_see,
        )
    });
    v.push(def(
        "Engine.Pawn.PressingFire",
        "native(0) final simulated native function bool PressingFire()",
        "engine.u Pawn.PressingFire decoded (native 0, return bool); Engine.dll \
         ?execPressingFire@APawn reads Controller.bFire (not IAController.bTire); the \
         XIIIWeapon state fire chain calls it",
        pawn_pressing_fire,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "parameters retained; spine-bone selection, speed/max clamp and world-space yaw conversion remain undecoded; renderer reports active requests",
        ),
        ..def(
            "Engine.Pawn.SpineYawControl",
            "native(0) function SpineYawControl(bool IsControlled, int MaxValue, float RotationSpeed)",
            "Engine.dll ?execSpineYawControl@APawn RVA 0xAFD00 (sets Pawn+0x1F8 bit 0x80 and stores MaxValue+0x208 / RotationSpeed+0x210); see local/reports/item3g-xiii-ai-natives-re.md",
            spine_yaw_control,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "CPU evaluator applies per-axis local scale by slot; slot disabling/invalid bones and shear after rotated nonuniform scale need parity validation",
        ),
        ..def(
            "Engine.Actor.SetBoneScalePerAxis",
            "native(400) final static function SetBoneScalePerAxis(int Slot, float BoneScaleX, float BoneScaleY, float BoneScaleZ, name BoneName)",
            "Engine.dll ?execSetBoneScalePerAxis@AActor RVA 0xE23B0 (omitted axes default to 1.0; forwards SetBoneScale to ?SetBoneScale@USkeletalMeshInstance RVA 0xED210); xidcine.Cine2.CineInit.Timer calls it for 'X Blink'",
            set_bone_scale_per_axis,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "request retained per bone; post-hierarchy inverse mesh/world-space rotation and translation alignment at GetFrame 0x103f324d remain undecoded; renderer reports active requests",
        ),
        ..def(
            "Engine.Actor.SetBoneDirection",
            "native(399) final static function SetBoneDirection(name BoneName, rotator BoneTurn, vector BoneTrans, float Alpha)",
            "Engine.dll ?execSetBoneDirection@AActor RVA 0xE2680 -> ?SetBoneDirection@USkeletalMeshInstance RVA 0xED5C0 (applies a bone-controller request); see local/reports/item3g-xiii-ai-natives-re.md",
            set_bone_direction,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "the request is recorded per actor for the renderer; no skeletal bone control is evaluated",
        ),
        ..def(
            "IAController.FindBestPathToward",
            "native(0) function bool FindBestPathToward(Actor Desired, optional float xyMargin, optional float heightMargin)",
            "XIDPawn.dll ?execFindBestPathToward@AIAController RVA 0x1AB0 (defaults 70.0/160.0; runs the A* to Desired.Location and writes the first node); decoded-ReachSpec path via Vm::nav_find_path_to",
            find_best_path_toward,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "the engine's exact trace endpoints are inferred from the XIDCine.dll disassembly",
        ),
        ..def(
            "PositionInfo.AutoPosition",
            "native(0) function AutoPosition()",
            "XIDCine.dll ?execAutoPosition@APositionInfo RVA 0x1990 (traces from Location.Z-1000 up to Location, sets Location to the hit, then adds fAltitude); physical trace via the physics provider",
            auto_position,
        )
    });
    v.push(def(
        "Engine.Actor.PlayerCanSeeMe",
        "native(532) final static function bool PlayerCanSeeMe()",
        "Engine.dll ?execPlayerCanSeeMe@AActor RVA 0xB3850 (standalone uses LastRenderTime; otherwise controller TestCanSeeMe) and ?TestCanSeeMe@AActor RVA 0xB0A60; headless: clear eye->actor trace from a player pawn",
        player_can_see_me,
    ));
    v.push(def(
        "Engine.Actor.StopVoice",
        "native(344) final static function StopVoice()",
        "Engine.dll ?execStopVoice@AActor RVA 0xE3F00 (audio subsystem voice stop); emits PresentationEvent::StopVoice",
        stop_voice,
    ));
    v.push(def(
        "Engine.Actor.StopSound",
        "native(265) final static function StopSound(object<Sound> Sound)",
        "Engine.dll ?execStopSound@AActor (?execStopActorSounds RVA 0xE3FA0 region); emits PresentationEvent::StopSound",
        stop_sound,
    ));
    v.push(def(
        "Engine.Actor.PlaySndPNJOno",
        "native(347) final static function PlaySndPNJOno(object<SndOno> Sound, int CodeMesh, int Timbre)",
        "Engine.dll ?execPlaySndPNJOno@AActor RVA 0xE2D10 (audio subsystem at vtable +0xD8, null Sound no-ops); emits PresentationEvent::PlaySndPNJOno",
        play_snd_pn_jo,
    ));
    v.push(def(
        "Engine.PlayerController.ConsoleCommand",
        "native(0) static function string ConsoleCommand(string Command)",
        "engine.u PlayerController.ConsoleCommand decoded; Engine.dll ?execConsoleCommand@APlayerController RVA 0x698F0; implements the campaign commands GETPING and Get GameInfo GoreLevel, logs the rest",
        console_command,
    ));
    // ---- item14 combat natives: firing/trace/damage entry points. Kept in their own block so
    // parallel edits merge cleanly. ------------------------------------------------
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "dynamic lights are not rendered yet: the refresh is recorded as a VM trace Note",
        ),
        ..def(
            "Engine.Actor.RefreshLighting",
            "native(389) final function RefreshLighting()",
            "engine.u Actor.RefreshLighting decoded (void, no params); xiii.MuzzleLight.PostBeginPlay              calls it when the Beretta's muzzle flash spawns (XIIIWeapon.Fire -> PlayFiring ->              IncrementFlashCount -> ThirdPersonEffects -> MuzzleAttach); Engine.dll              ?execRefreshLighting@AActor (relights the actor's light). No script-visible result",
            refresh_lighting,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "no log sink: the message is recorded as a VM trace Note and discarded",
        ),
        ..def(
            "Object.Warn",
            "native(232) final static function Warn(coerce string S)",
            "core.u Object.Warn decoded (string, void); XIIIPawn.TakeDamage warns when a dead pawn \
             is hit again",
            warn_log,
        )
    });
    v.push(def(
        "Object.Subtract_PreVector",
        "native(211) final preoperator vector -(vector A)",
        "core.u Object.Subtract_PreVector decoded (unary vector negation); \
         XIIIBulletsAmmo.ProcessTraceHit uses -HitNormal for spawned emitters",
        neg_vector,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "accepted and discarded: the VM has no HX sound mapping for the death onomatopoeia",
        ),
        ..def(
            "Engine.Actor.PlaySndDeathOno",
            "native(346) final native static function PlaySndDeathOno(object<DeathOno> Sound, int CodeMesh, int Timbre)",
            "engine.u Actor.PlaySndDeathOno decoded (Sound, CodeMesh, Timbre); BaseSoldier.Died \
             calls it after Super.Died; Engine.dll ?execPlaySndDeathOno@AActor",
            play_snd_death_ono,
        )
    });
    v.push(def(
        "Object.SubtractSubtract_Byte",
        "native(140) final native operator static function byte --(byte A)",
        "core.u Object.SubtractSubtract_Byte decoded (byte A, return byte); XIIIWeapon.LoneFire \
         decrements ReloadCount through it",
        dec_byte,
    ));
    v.push(def(
        "Object.AddAdd_Byte",
        "native(139) final native operator static function byte ++(byte A)",
        "core.u Object.AddAdd_Byte decoded (byte A, return byte); the byte pre-increment \
         counterpart used by the weapon/ammo scripts",
        inc_byte,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "emits PlaySound from the weapon's hFireSound/hAltFireSound selected by bHasSilencer; \
             the host resolves the HX wave and plays it. The decoded XIII weapons set neither \
             property in their class defaults, so the event may carry no sound (recorded as a \
             trace note, never a silent success)",
        ),
        ..def(
            "Engine.Weapon.PlayFiringSound",
            "native(0) native function PlayFiringSound(bool bHasSilencer)",
            "engine.u Weapon.PlayFiringSound decoded (bool bHasSilencer, native); \
             Beretta.PlayFiring calls PlayFiringSound(HasSilencer()) on every shot; the decoded \
             Weapon defaults are hFireSound/hAltFireSound/bUseSilencer; Engine.dll \
             ?execPlayFiringSound@AWeapon",
            play_firing_sound,
        )
    });
    v.push(def(
        "Engine.Actor.IntersectWaterPlane",
        "native(0) final native static function Actor IntersectWaterPlane(Vector Start, Vector End, out Vector Intersection)",
        "engine.u Actor.IntersectWaterPlane decoded (Start, End, out Intersection, return Actor); \
         XIIIWeapon.RealTraceFire calls it on every shot to route a water impact to a \
         PhysicsVolume. The VM has no water volumes, so it returns None (the caller dereferences \
         the volume only when non-null); Engine.dll ?execIntersectWaterPlane@AActor",
        intersect_water_plane,
    ));
    v.push(def(
        "Engine.Actor.GetLastTraceBone",
        "native(364) final native static function name GetLastTraceBone()",
        "engine.u Actor.GetLastTraceBone decoded (return name); XIIIWeapon.RealTraceFire stores it \
         into XIIIPawn.LastBoneHit and XIIIPawn.GetDamageLocation reads 'X Head'/'X Spine1' from \
         it; Vm::vm_trace records the actor hit zone (the default model is the collision \
         cylinder; see xiii_script::physics::HitZones); Engine.dll ?execGetLastTraceBone@AActor",
        get_last_trace_bone,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "stores Enemy and raises EnemyAcquired when it changes; the XIDPawn native's BaseS/\
             GenAlerte bookkeeping and sight-counter side effects are not modelled",
        ),
        ..def(
            "IAController.SetEnemy",
            "native(0) function bool SetEnemy(Pawn Newenemy)",
            "xidpawn.u IAController.SetEnemy decoded (Pawn, return bool); IAController.SeePlayer/\
             SeeEnemy set the current target through it; XIDPawn.dll ?execSetEnemy@AIAController \
             calls ?eventEnemyAcquired@AIAController, which the neutral states use to enter \
             acquisition",
            ia_controller_set_enemy,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "returns EyeHeight/BaseEyeHeight along +Z; crouch and view-height interpolation are \
             not modelled",
        ),
        ..def(
            "Engine.Pawn.EyePosition",
            "native(0) native function Vector EyePosition()",
            "engine.u Pawn.EyePosition decoded (return Vector, native); XIIIPawn overrides it; \
             XIIIWeapon.RealTraceFire adds it to Instigator.Location for the trace start; \
             Engine.dll ?execEyePosition@APawn",
            pawn_eye_position,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "returns the controller's Rotation when set, else the pawn's Rotation; the native's \
             cloud/rotation blending is not modelled",
        ),
        ..def(
            "Engine.Pawn.GetViewRotation",
            "native(0) simulated native function Rotator GetViewRotation()",
            "engine.u Pawn.GetViewRotation decoded (return Rotator, native); \
             XIIIWeapon.RealTraceFire passes it to Object.GetAxes; Engine.dll \
             ?execGetViewRotation@APawn",
            pawn_get_view_rotation,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "returns the instigator's eye (Location + EyePosition); the decoded muzzle offset is \
             not applied",
        ),
        ..def(
            "Engine.Weapon.GetFireStart",
            "native(0) native function Vector GetFireStart(Vector X, Vector Y, Vector Z)",
            "engine.u Weapon.GetFireStart decoded (X,Y,Z, return Vector, native); \
             XIIIWeapon.RealTraceFire uses it as StartTrace for WHand != 0/4; Engine.dll \
             ?execGetFireStart@AWeapon",
            weapon_get_fire_start,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "returns Inv.PlayerViewOffset; the engine combines it with the mesh eye offset and \
             view bob",
        ),
        ..def(
            "Engine.Pawn.CalcDrawOffset",
            "native(0) simulated native function Vector CalcDrawOffset(object<Inventory> Inv)",
            "engine.u Pawn.CalcDrawOffset decoded (Inventory, return Vector, native); \
             XIIIWeapon.Active.BeginState calls it to place the first-person weapon; Engine.dll \
             ?execCalcDrawOffset@APawn",
            pawn_calc_draw_offset,
        )
    });
    // `Color` operators (native 549-552). The HUD widget draw path (`HudState.DrawStt` ->
    // `XIIIBaseHud.DrawHUD`) multiplies/tints colors; before these the HUD `PostRender`
    // suspended on `Actor.Multiply_ColorFloat`. UE2 `FColor` scalar/vector arithmetic
    // (truncate then clamp to 0..255).
    v.push(def(
        "Engine.Actor.Multiply_ColorFloat",
        "native(552) final native operator static function Color Multiply_ColorFloat(struct<Color> A, float B)",
        "engine.u Actor.Multiply_ColorFloat decoded; UE2 FColor::operator*(float), componentwise, clamped",
        multiply_color_float,
    ));
    v.push(def(
        "Engine.Actor.Multiply_FloatColor",
        "native(550) final native operator static function Color Multiply_FloatColor(float A, struct<Color> B)",
        "engine.u Actor.Multiply_FloatColor decoded; UE2 FColor::operator*(float) operand order, clamped",
        multiply_float_color,
    ));
    v.push(def(
        "Engine.Actor.Add_ColorColor",
        "native(551) final native operator static function Color Add_ColorColor(struct<Color> A, struct<Color> B)",
        "engine.u Actor.Add_ColorColor decoded; UE2 FColor::operator+(FColor), componentwise, clamped",
        add_color_color,
    ));
    v.push(def(
        "Engine.Actor.Subtract_ColorColor",
        "native(549) final native operator static function Color Subtract_ColorColor(struct<Color> A, struct<Color> B)",
        "engine.u Actor.Subtract_ColorColor decoded; UE2 FColor::operator-(FColor), componentwise, clamped",
        subtract_color_color,
    ));
    // Campaign-suspension fixes (item3n): natives reached by the campaign survey after the VM
    // fixes. Kept in one block so parallel registry edits stay out of the way.
    v.push(def(
        "Object.GetAxes",
        "native(229) final native static function GetAxes(rotator A, out vector X, out vector Y, out vector Z)",
        "core.u Object.GetAxes decoded; UE2 FRotationMatrix basis (X forward, Y right, Z up); engine.Pawn.TossWeapon 0x0014",
        get_axes,
    ));
    v.push(def(
        "Engine.Actor.StopAnimating",
        "native(417) final function StopAnimating()",
        "engine.u Actor.StopAnimating decoded; engine.Inventory.DropFrom 0x003A stops the dropped item's animation",
        stop_animating,
    ));
    let snow_natives: [(&'static str, &'static str, NativeFn); 6] = [
        (
            "LevelInfo.InitRndCubeSpr",
            "native(0) simulated function InitRndCubeSpr(object<Texture> Texture, int MaxNbrSpr, float PropSprUsed, float Distance)",
            init_rnd_cube_spr,
        ),
        (
            "LevelInfo.SetRndCubeSprSize",
            "native(0) simulated function SetRndCubeSprSize(float NewSpriteSize, float NewSpriteSizeMax, bool IsMask)",
            set_rnd_cube_spr_size,
        ),
        (
            "LevelInfo.SetRndCubeSprSpeed",
            "native(0) simulated function SetRndCubeSprSpeed(vector Speed, float RandomSpeed, float RandomAcc)",
            set_rnd_cube_spr_speed,
        ),
        (
            "LevelInfo.AddRndCubeSprExclude",
            "native(0) simulated function AddRndCubeSprExclude(vector Min, vector Max)",
            add_rnd_cube_spr_exclude,
        ),
        (
            "LevelInfo.SetRndCubeSprState",
            "native(0) simulated function SetRndCubeSprState(bool Activate)",
            set_rnd_cube_spr_state,
        ),
        (
            "LevelInfo.ChangeRndCubeSprProp",
            "native(0) simulated function bool ChangeRndCubeSprProp(float Proportion, float FadeSpeed, float NbrSprFadePerLoop)",
            change_rnd_cube_spr_prop,
        ),
    ];
    for (path, sig, f) in snow_natives {
        v.push(NativeDef {
            status: NativeStatus::Partial(
                "no particle subsystem: the call is recorded in the trace and never silently accepted",
            ),
            ..def(
                path,
                sig,
                "engine.u LevelInfo.RndCubeSpr* decoded; xidmaps.Map06_HualparBase.StartSnow calls them",
                f,
            )
        });
    }
    // Level-travel natives (item15). Kept in one block so parallel registry edits stay out of the
    // way. `ClientTravel` is the engine entry point reached by `XIIIGameInfo.ProcessServerTravel`;
    // it never loads a map, only records a host `TravelRequest`.
    v.push(def(
        "PlayerController.ClientTravel",
        "native(0) net reliable native event static function ClientTravel(string URL, byte<ETravelType> TravelType, bool bItems)",
        "engine.u PlayerController.ClientTravel decoded; xiii.u XIIIGameInfo.ProcessServerTravel 0x008F; records a TravelRequest for the host (the VM loads no map)",
        client_travel,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "no player profile in the headless VM; returns \"\" with a visible note (the standalone travel path does not reach it)",
        ),
        ..def(
            "PlayerController.GetDefaultURL",
            "native(510) final native static function string GetDefaultURL(string Option)",
            "engine.u PlayerController.GetDefaultURL decoded (native 510); xiii.u XIIIGameInfo.ProcessServerTravel builds the URL options from it",
            get_default_url,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "no renderer: the first-person pose is the pawn eye position and the controller rotation (enough for the end-game camera hand-off; not a projection match)",
        ),
        ..def(
            "PlayerController.CalcFirstPersonView",
            "native(497) final native static function CalcFirstPersonView(out struct<Vector> CameraLocation, out struct<Rotator> CameraRotation)",
            "engine.u PlayerController.CalcFirstPersonView decoded (native 497); reachable from XIIIGameInfo.EndGame -> GameEnded.BeginState -> global.PlayerCalcView on the level-complete path",
            calc_first_person_view,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "no renderer: returns the un-snapped SmoothedAim(Rotation) (the aim-assist target-scan \
             is crosshair display only); the returned rotator is the controller view rotation",
        ),
        ..def(
            "PlayerController.AdjustAimForDisplay",
            "native(498) final native static function Rotator AdjustAimForDisplay(object<Ammunition> FiredAmmunition, struct<Vector> projStart)",
            "Engine.dll execAdjustAimForDisplay 0x1036e2e0 decoded (native 498, registration thunk 0x1051aac8); every epilogue returns SmoothedAim(0x1036c720) of the rotation, snap path gated on a scan cooldown + ammunition+0x270 bit 0x10; consumer XIIIPlayerInteraction.MyPCPostRender 0x032D (crosshair ray + WarnTarget)",
            adjust_aim_for_display,
        )
    });
    // item3p: the missing rotator operators, reached by the campaign survey. `Multiply_RotatorFloat`
    // (287, `xidcine.HelicoDeco.HelicoTick` 0x01F4) and `EqualEqual_RotatorRotator` (142,
    // `xiii.MitraillTop.GoToWaitingPos.Tick` 0x0035) were the two highest-count unimplemented
    // natives; the rest of the declared rotator operator family (288, 289, 203) is added so the
    // set is complete, `MultiplyMultiply_FloatFloat` (170) is the next operator the campaign
    // then reaches in `HelicoTick`, and `ParticleEmitter.SetMaxParticles` is a visible Partial
    // (no particle renderer) reached once the BreakableMover emitter subobject exists. Kept in
    // one block so parallel registry edits stay out of the way.
    v.push(def(
        "Object.Multiply_RotatorFloat",
        "native(287) final operator rotator *(rotator A, float B)",
        "core.u Object.Multiply_RotatorFloat decoded; componentwise rotator scale; xidcine.HelicoDeco.HelicoTick 0x01F4",
        mul_rf,
    ));
    v.push(def(
        "Object.Multiply_FloatRotator",
        "native(288) final operator rotator *(float A, rotator B)",
        "core.u Object.Multiply_FloatRotator decoded; reversed operand order of native 287",
        mul_fr,
    ));
    v.push(def(
        "Object.Divide_RotatorFloat",
        "native(289) final operator rotator /(rotator A, float B)",
        "core.u Object.Divide_RotatorFloat decoded; componentwise rotator division; a zero divisor errors",
        div_rf,
    ));
    v.push(def(
        "Object.EqualEqual_RotatorRotator",
        "native(142) final operator bool ==(rotator A, rotator B)",
        "core.u Object.EqualEqual_RotatorRotator decoded; exact componentwise equality; xiii.MitraillTop.GoToWaitingPos.Tick 0x0035",
        eq_rr,
    ));
    v.push(def(
        "Object.NotEqual_RotatorRotator",
        "native(203) final operator bool !=(rotator A, rotator B)",
        "core.u Object.NotEqual_RotatorRotator decoded; negation of native 142",
        ne_rr,
    ));
    v.push(def(
        "Object.MultiplyMultiply_FloatFloat",
        "native(170) final operator float **(float A, float B)",
        "core.u Object.MultiplyMultiply_FloatFloat decoded; UE2 appPow (C pow); xidcine.HelicoDeco.HelicoTick 0x01FF",
        pow_ff,
    ));
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "no particle subsystem: the call is recorded in the trace and never silently accepted",
        ),
        ..def(
            "Engine.ParticleEmitter.SetMaxParticles",
            "native(0) final function SetMaxParticles(int NewMaxParticles)",
            "engine.u ParticleEmitter.SetMaxParticles decoded; xidcine.BreakableMover.InitializeEmitters 0x0348 (reached once the emitter subobject exists)",
            set_max_particles,
        )
    });
    // item14c: the weapon-attachment bullet-trail natives. `XIIIWeaponAttachment.Timer2` spawns
    // the weapon's `TraceClass` and calls these; before this block the missing `Trail.Init`
    // suspended `BerettaAttach` and the game's own muzzle-flash chain was silently deferred. Kept
    // in its own block so parallel registry edits stay out of the way.
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "no trail renderer: the request is recorded in the trace and the section list is not \
             built; the attachment actor stays active",
        ),
        ..def(
            "Engine.Trail.Init",
            "native(601) final native static function Init()",
            "engine.u Trail.Init decoded (void, no params); xiii.XIIIWeaponAttachment.Timer2 0x0088 \
             calls it after spawning the weapon's TraceClass; Engine.dll ?execTrailInit",
            trail_init,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "no trail renderer: the traced point is accepted and dropped",
        ),
        ..def(
            "Engine.Trail.AddSection",
            "native(603) final native static function AddSection(struct<Vector> Position)",
            "engine.u Trail.AddSection decoded (struct<Vector>, void); \
             xiii.XIIIWeaponAttachment.Timer2 0x01F8/0x022B/0x0292/0x02BE add each traced point",
            trail_add_section,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "no trail renderer: there is nothing to clear; accepted and dropped",
        ),
        ..def(
            "Engine.Trail.Reset",
            "native(0) function Reset()",
            "engine.u Trail.Reset decoded (void, no params); called by Trail.Tick while the \
             weapon attachment's traced trail lives",
            trail_reset,
        )
    });
    // Canvas draw-recording natives (`crates/xiii-script/src/canvas.rs`). Kept in one block so a
    // parallel edit to the registry stays out of the way.
    v.extend(crate::canvas::canvas_defs());
    // item16 front-end menu natives (VideoPlayer, ClientTravel, menu sounds). Kept in the same
    // `canvas.rs` block so a parallel edit to the registry stays out of the way.
    v.extend(crate::canvas::menu_defs());
    // item16b GUI-frame natives (`GUIController.GetStyle`/`InitStateFrame`). New block so a
    // parallel edit to the registry stays out of the way.
    v.extend(crate::canvas::item16b_defs());
    // item16c: menu configuration, property text and host audio/video settings.
    v.extend(crate::canvas::item16c_defs());
    // Cinematic/dialogue natives (`crates/xiii-script/src/cinematics.rs`). Kept in one block so a
    // parallel edit to the registry stays out of the way.
    v.extend(crate::cinematics::cinematic_defs());
    // Cartoon-panel natives (`crates/xiii-script/src/cartoon.rs`). Kept in one block so a
    // parallel edit to the registry stays out of the way.
    v.extend(crate::cartoon::cartoon_defs());
    // item18: small VM gaps found on the Plage01 route (float `%`, particle spawn Partial).
    v.extend(item18_defs());
    // item19: CineController2 cutscene movement and explicit bullet-trail presentation Partials.
    // New block so a parallel registry edit stays out of the way.
    v.extend(crate::cinematics::item19_defs());
    // Intro/checkpoint residual natives (`crates/xiii-script/src/residuals.rs`). Kept in one
    // block so a parallel edit to the registry stays out of the way.
    v.extend(crate::residuals::residual_defs());
    // item20: decoded GUI save-slot APIs. Host directory integration is required to enable them.
    v.extend(crate::item20::save_defs());
    // Paths are matched without the package ("Class.Function"): strip it.
    for d in &mut v {
        if let Some(rest) = d.path.strip_prefix("Engine.") {
            d.path = rest;
        }
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Item15/item16 merge guard: `Registry` is a `BTreeMap`, so a second registration of the same
    /// `Class.Function` silently replaces the first. This checks the raw `builtin_defs()` list so a
    /// duplicate (e.g. two `PlayerController.ClientTravel`) fails the build instead of silently
    /// dropping one implementation.
    #[test]
    fn no_native_is_registered_twice() {
        let mut seen: std::collections::BTreeMap<String, &'static str> =
            std::collections::BTreeMap::new();
        for d in builtin_defs() {
            let key = d.path.to_ascii_lowercase();
            if let Some(prev) = seen.insert(key, d.path) {
                panic!("native {prev} is registered twice");
            }
        }
    }
}
