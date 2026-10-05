//! Native-function registry for the interpreter.
//!
//! Natives are keyed by `Class.Function` (lowercase) of their script declaration; the native
//! index of a call token is first resolved to its declaring function by the VM (duplicate
//! indices are separated by argument count), so the registry never guesses from an index.
//! Every entry records its signature, the source of its semantics and its status. A native
//! that is declared but not registered fails with `VmErrorKind::UnimplementedNative`.

use std::collections::BTreeMap;

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

fn dynamic_load_object(vm: &mut Vm<'_>, _: &NativeCtx, a: &mut [Value]) -> VmResult<NativeOutcome> {
    let name = match a.first() {
        Some(Value::Str(s)) | Some(Value::Name(s)) => s.clone(),
        _ => return val(Value::Object(None)),
    };
    // UE2 `execDynamicLoadObject`: resolve the name, then require the loaded object's class to
    // be `ObjectClass` (a subclass), else the load fails (NULL). This matters because e.g.
    // `DynamicLoadObject(MeshName, class'Engine.Mesh')` must not return a non-Mesh object.
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
                // The VM has no runtime class hierarchy for arbitrary natives; compare the
                // object's decoded class name against the requested one (leaf name). This
                // rejects a Texture when `class'Engine.Mesh'` was requested.
                let actual = vm.class_path_of(g);
                let ok = actual.as_deref().is_some_and(|a| {
                    let leaf = |p: &str| p.rsplit('.').next().unwrap_or(p).to_ascii_lowercase();
                    leaf(a) == leaf(req)
                });
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
        "engine.u Actor.LinkSkelAnim decoded; sets the actor MeshAnimation reference (the VM remembers its path for sequence lookups)",
        link_skel_anim,
    ));
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
    // Paths are matched without the package ("Class.Function"): strip it.
    for d in &mut v {
        if let Some(rest) = d.path.strip_prefix("Engine.") {
            d.path = rest;
        }
    }
    v
}
