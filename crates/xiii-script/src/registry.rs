//! Native-function registry for the interpreter.
//!
//! Natives are keyed by `Class.Function` (lowercase) of their script declaration; the native
//! index of a call token is first resolved to its declaring function by the VM (duplicate
//! indices are separated by argument count), so the registry never guesses from an index.
//! Every entry records its signature, the source of its semantics and its status. A native
//! that is declared but not registered fails with `VmErrorKind::UnimplementedNative`.

use std::collections::BTreeMap;

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
    // Paths are matched without the package ("Class.Function"): strip it.
    for d in &mut v {
        if let Some(rest) = d.path.strip_prefix("Engine.") {
            d.path = rest;
        }
    }
    v
}
