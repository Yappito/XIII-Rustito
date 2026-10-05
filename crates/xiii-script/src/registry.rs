//! Native-function registry for the interpreter.
//!
//! Natives are keyed by `Class.Function` (lowercase) of their script declaration; the native
//! index of a call token is first resolved to its declaring function by the VM (duplicate
//! indices are separated by argument count), so the registry never guesses from an index.
//! Every entry records its signature, the source of its semantics and its status. A native
//! that is declared but not registered fails with `VmErrorKind::UnimplementedNative`.

use std::collections::BTreeMap;

use crate::events::PresentationEvent;
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
    /// Iterator items (the first `out` parameter receives each one).
    Iterate(Vec<Value>),
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
        Some(Value::Object(Some(ObjRef::Static(g)))) => vm.class_path_of(*g),
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
        None => val(Value::Object(None)),
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
fn rotator_basis(r: [i32; 3]) -> ([f32; 3], [f32; 3], [f32; 3]) {
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

fn all_actors(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => Some(g),
        None => None,
        Some(ObjRef::Instance(_)) => {
            return Err(vm.err(VmErrorKind::Other(
                "AllActors base class is an instance".into(),
            )));
        }
    };
    let tag = match a.get(2) {
        Some(Value::Name(n)) if !n.eq_ignore_ascii_case("None") => Some(n.clone()),
        _ => None,
    };
    let items = vm
        .all_actors(base, tag.as_deref())
        .into_iter()
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(items))
}

/// `Actor.CollidingActors` (native 321): actors of `BaseClass` near the caller. **Partial**: the
/// VM uses the same distance filter as `RadiusActors` (its collision cylinders are not swept);
/// `XIIIMover.Timer` re-checks `FastTrace`/vision on each result, so this is sufficient for the
/// door-warning timer and keeps a mover from being suspended on its own timer.
fn colliding_actors(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => Some(g),
        None => None,
        Some(ObjRef::Instance(_)) => {
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
    let items = vm
        .radius_actors(base, radius, loc)
        .into_iter()
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(items))
}

fn radius_actors(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let base = match object(vm, a, 0)? {
        Some(ObjRef::Static(g)) => Some(g),
        None => None,
        Some(ObjRef::Instance(_)) => {
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
    let items = vm
        .radius_actors(base, radius, loc)
        .into_iter()
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(items))
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
    let state = name(vm, a, 0)?;
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
    set_timer(vm, c, a)
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

fn move_to(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let destination = vector2(vm, a, 0)?;
    let speed = if c.omitted(2) { 0.0 } else { float(vm, a, 2)? };
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
    let speed = if c.omitted(2) { 0.0 } else { float(vm, a, 2)? };
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

/// `Controller.FinishRotation`: snap the pawn's yaw to face the controller's `FocalPoint`.
///
/// The engine suspends state code while it interpolates the rotation at `RotationRate.Yaw`. The
/// headless VM has no per-tick rotation, so the snap is applied immediately (no latent). The
/// decoded declaration is `final latent function FinishRotation()`.
fn finish_rotation(vm: &mut Vm<'_>, c: &NativeCtx, _a: &mut [Value]) -> VmResult<NativeOutcome> {
    if let (Some(pawn), Some(focal)) = (
        vm.obj_prop(c.this, "Pawn"),
        vm.vector_prop(c.this, "FocalPoint"),
    ) {
        let loc = vm.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        // UE2 rotator units: 65536 per full turn; yaw is the Z component.
        let dx = focal[0] - loc[0];
        let dy = focal[1] - loc[1];
        let yaw = (dy.atan2(dx) * (65536.0 / std::f32::consts::TAU)) as i32;
        let rot = match vm.get_property(pawn, "Rotation") {
            Some(Value::Rotator(r)) => [r[0], r[1], yaw],
            _ => [0, 0, yaw],
        };
        vm.set_property(pawn, "Rotation", 0, Value::Rotator(rot));
        vm.note(crate::vm::TraceKind::Log(format!(
            "FinishRotation: {} -> yaw {}",
            vm.objects[c.this as usize].name, yaw
        )));
    }
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
    let (hit_actor, hit_location, hit_normal) =
        vm.vm_trace(c.this, start, end, b_trace_actors, extent)?;
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
        Some(ObjRef::Instance(_)) => {
            return Err(vm.err(VmErrorKind::Other(
                "TouchingActors base class is an instance".into(),
            )));
        }
    };
    let items = vm
        .touching_list(c.this)
        .into_iter()
        .filter(|id| base.is_none_or(|b| vm.objects[*id as usize].layout.chain.contains(&b)))
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(items))
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
    let rate = if c.omitted(1) { 0.0 } else { float(vm, a, 1)? };
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
    let rate = if c.omitted(1) { 0.0 } else { float(vm, a, 1)? };
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

/// `Actor.MakeNoise`: UE2 notifies nearby AI (`Pawn.HearNoise`) of a noise at the actor's
/// location. The VM has no AI hearing/perception model, so the call is accepted and discarded
/// (registered `Partial` with that reason; `GameInfo.PlayTeleportEffect` calls it on the
/// player-login path).
fn make_noise(vm: &mut Vm<'_>, _: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    let _ = vm;
    val(Value::Void)
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

fn play_sound(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.emit_sound(false, c.this, a, &c.omitted);
    val(Value::Void)
}

fn play_music(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    vm.emit_sound(true, c.this, a, &c.omitted);
    val(Value::Void)
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
    // UE2 `APawn::IsPlayerPawn`: true for a player pawn. XIII has no `bIsPlayerPawn` field
    // (measured), so this is the class-chain test (the same approximation as `is_player_or_projectile`).
    val(Value::Bool(vm.is_a(c.this, "PlayerPawn")))
}

fn find_inventory_type(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    // UE2 `APawn::FindInventoryType`: walks `Inventory` -> `Inventory` (the item chain) and
    // returns the first item whose class derives from `DesiredClass`. XIII's decoded signature
    // has no `bExactClass` flag.
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
        if vm.is_child_of_class(vm.objects[id as usize].class, desired) {
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
        vm.set_property(id, "AttachBone", 0, Value::Name(n.clone()));
    }
    val(Value::Bool(true))
}

fn anim_blend_to_alpha(vm: &mut Vm<'_>, c: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let stage = int(vm, a, 0)?;
    let target = float(vm, a, 1)?;
    let time = float(vm, a, 2)?;
    vm.anim_blend_params(c.this, stage, target, time, 0.0, None);
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
    // `Actor.GetAnimParams(int Channel, out name OutSeqName, out float OutAnimFrame,
    // out float OutAnimRate)`: the VM's channel state holds the frame/rate; the sequence name
    // is the actor's `AnimSequence` property (the channel itself does not store the name).
    let ch = channel(vm, a, 0, c.omitted(0))?;
    let name = vm
        .get_property(c.this, "AnimSequence")
        .cloned()
        .unwrap_or_else(|| Value::Name("None".to_owned()));
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

/// `Level.PawnList`/`Level.ControllerList` insert-at-head and unlink, shared by the pawn and
/// controller list natives (UE2 `APawn::AddPawnToList`/`RemovePawnFromList` and the controller
/// equivalents).
fn list_insert_head(
    vm: &mut Vm<'_>,
    obj: ObjectId,
    level: ObjectId,
    list: &str,
    next: &str,
) -> VmResult<()> {
    let head = prop_object(vm, level, list);
    vm.set_property(obj, next, 0, Value::Object(head.map(ObjRef::Instance)));
    vm.set_property(level, list, 0, Value::Object(Some(ObjRef::Instance(obj))));
    Ok(())
}

fn list_remove(vm: &mut Vm<'_>, obj: ObjectId, list: &str, next: &str) -> VmResult<()> {
    let Some(level) = prop_object(vm, obj, "Level") else {
        vm.set_property(obj, next, 0, Value::Object(None));
        return Ok(());
    };
    if prop_object(vm, level, list) == Some(obj) {
        let successor = prop_object(vm, obj, next);
        vm.set_property(
            level,
            list,
            0,
            Value::Object(successor.map(ObjRef::Instance)),
        );
    } else {
        let mut cur = prop_object(vm, level, list);
        let mut guard = 0;
        while let Some(c) = cur {
            guard += 1;
            if guard > 65_536 {
                break;
            }
            if prop_object(vm, c, next) == Some(obj) {
                let successor = prop_object(vm, obj, next);
                vm.set_property(c, next, 0, Value::Object(successor.map(ObjRef::Instance)));
                break;
            }
            cur = prop_object(vm, c, next);
        }
    }
    vm.set_property(obj, next, 0, Value::Object(None));
    Ok(())
}

fn add_pawn_to_list(vm: &mut Vm<'_>, c: &NativeCtx, _: &mut [Value]) -> VmResult<NativeOutcome> {
    if let Some(level) = prop_object(vm, c.this, "Level") {
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
    if let Some(level) = prop_object(vm, c.this, "Level") {
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
        Some(ObjRef::Instance(_)) => {
            return Err(vm.err(VmErrorKind::Other(
                "DynamicActors base class is an instance".into(),
            )));
        }
    };
    let tag = match a.get(2) {
        Some(Value::Name(n)) if !n.eq_ignore_ascii_case("None") => Some(n.clone()),
        _ => None,
    };
    let items = vm
        .dynamic_actors(base, tag.as_deref())
        .into_iter()
        .map(|i| Value::Object(Some(ObjRef::Instance(i))))
        .collect();
    Ok(NativeOutcome::Iterate(items))
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

fn builtin_defs() -> Vec<NativeDef> {
    let mut v = vec![
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
        NativeDef {
            status: NativeStatus::Partial(
                "deterministic seeded PRNG (splitmix64); the engine RNG sequence is not reproduced",
            ),
            ..def(
                "Object.FRand",
                "native(195) final static function float FRand()",
                "UE2: pseudo-random float in [0,1); Core.dll ?execFRand@UObject",
                f_rand,
            )
        },
        NativeDef {
            status: NativeStatus::Partial(
                "deterministic seeded PRNG (splitmix64); the engine RNG sequence is not reproduced",
            ),
            ..def(
                "Object.Rand",
                "native(167) final static function int Rand(int Max)",
                "UE2: pseudo-random int in [0,Max); Core.dll ?execRand@UObject",
                rand_i,
            )
        },
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
            "engine.u Pawn.IsPlayerPawn decoded (bool); class-chain test (XIII has no bIsPlayerPawn field; measured)",
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
                "the channel state stores frame/rate but not the sequence name; OutSeqName is the actor's AnimSequence property",
            ),
            ..def(
                "Engine.Actor.GetAnimParams",
                "native(396) final static function GetAnimParams(int Channel, out name OutSeqName, out float OutAnimFrame, out float OutAnimRate)",
                "engine.u Actor.GetAnimParams decoded; fills the channel's sequence name (the AnimSequence property) and the VM's frame/rate",
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
                "no AI hearing/perception model: the call is accepted and discarded, AI `HearNoise` is not invoked",
            ),
            ..def(
                "Engine.Actor.MakeNoise",
                "native(512) final native static function MakeNoise(float Loudness)",
                "engine.u Actor.MakeNoise decoded (float Loudness; native 512); UE2 notifies nearby AI; Engine.dll ?execMakeNoise@AActor",
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
        def(
            "Engine.Actor.AttachToBone",
            "native(404) final static function bool AttachToBone(object<Actor> Attachment, name BoneName)",
            "engine.u Actor.AttachToBone decoded (Attachment, BoneName, bool); bases the attachment on self and records the bone; no skeletal transform (headless)",
            attach_to_bone,
        ),
        NativeDef {
            status: NativeStatus::Partial(
                "stores the target blend alpha/time like the other animation channel parameters; no skeletal blending is evaluated",
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
                "world move via the physics provider (move_box, no sliding); actor blocking is a cylinder-sweep stop; no Bump/EncroachingOn events; player/projectile bBlockPlayers pairing is inferred from class names (XIII has no bIsPlayerPawn field)",
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
                "actor hits use ray-vs-grown-cylinder, owned actors are skipped; Material out-param is None and DiscardedHitMask 0 (no material/hit-mask model); AdditionalTraceType and mover brush geometry are not modelled; world hits return the map LevelInfo (upstream)",
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
            "stores the per-channel blend parameters (BlendAlpha/InTime/OutTime and the optional BoneName); no skeletal blending is evaluated and the BoneName bone filter is not applied",
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
            "sequence length/notify times come from the AnimationData provider; no skeletal evaluation; playback is frames/second and tween holds frame 0",
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
            "sequence length/notify times come from the AnimationData provider; no skeletal evaluation; playback is frames/second and tween holds frame 0",
        ),
        ..def(
            "Engine.Actor.LoopAnim",
            "native(260) final function LoopAnim(name Sequence, float Rate, float TweenTime, int Channel)",
            "engine.u Actor.LoopAnim decoded (Sequence, Rate, TweenTime, Channel); UE1 AActor::LoopAnim (loops, never fires AnimEnd)",
            loop_anim,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "is PlayAnim with the sequence held at frame 0 until Time elapses; exact XIII tween blending is not modelled and needs the decoded mesh",
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
            "same timer semantics as SetTimer; the second timer's role is not established without DLL disassembly",
        ),
        ..def(
            "Engine.Actor.SetTimer2",
            "native(363) final function SetTimer2(float NewTimerRate, bool bLoop)",
            "engine.u Actor.SetTimer2 decoded (float, bool; identical declaration to SetTimer); Engine.dll ?execSetTimer2@AActor",
            set_timer2,
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
            "world line trace between the pawn eyes; actor occlusion is not modelled",
        ),
        ..def(
            "Engine.Controller.LineOfSightTo",
            "native(514) final function bool LineOfSightTo(actor Other, return bool ReturnValue)",
            "engine.u Controller.LineOfSightTo decoded; UE2 AController::LineOfSightTo traces eye-to-eye; Engine.dll ?execLineOfSightTo@AController",
            line_of_sight_to,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "latent horizontal movement at GroundSpeed (or the Speed argument) using the physics move_box; no acceleration, path following or footstep/floor logic; ends on arrival or after a travel-time budget",
        ),
        ..def(
            "Engine.Controller.MoveTo",
            "native(500) final latent function MoveTo(vector NewDestination, optional actor ViewFocus, optional float Speed)",
            "engine.u Controller.MoveTo decoded; UE2 AController::MoveTo (latent) sets Destination and moves the pawn; Engine.dll ?execMoveTo@AController",
            move_to,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "latent horizontal movement toward the target's Location; same movement model as MoveTo",
        ),
        ..def(
            "Engine.Controller.MoveToward",
            "native(502) final latent function MoveToward(actor NewTarget, optional actor ViewFocus, optional float Speed, optional actor NextTarget)",
            "engine.u Controller.MoveToward decoded; UE2 AController::MoveToward (latent); Engine.dll ?execMoveToward@AController",
            move_toward,
        )
    });
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "snaps the pawn yaw to FocalPoint immediately; the engine's per-tick rotation interpolation at RotationRate.Yaw is not modelled",
        ),
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
    v.push(NativeDef {
        status: NativeStatus::Partial(
            "parameters are stored per actor for the renderer; no skeletal bone control is evaluated",
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
            "the request is recorded per actor for the renderer; no skeletal transform is evaluated",
        ),
        ..def(
            "Engine.Actor.SetBoneDirection",
            "native(399) final static function SetBoneDirection(name BoneName, rotator BoneTurn, vector BoneTrans, float Alpha)",
            "Engine.dll ?execSetBoneDirection@AActor RVA 0xE2680 -> ?SetBoneDirection@USkeletalMeshInstance RVA 0xED5C0 (applies a bone-controller request); see local/reports/item3g-xiii-ai-natives-re.md",
            set_bone_direction,
        )
    });
    // Paths are matched without the package ("Class.Function"): strip it.
    for d in &mut v {
        if let Some(rest) = d.path.strip_prefix("Engine.") {
            d.path = rest;
        }
    }
    v
}
