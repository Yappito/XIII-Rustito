//! Synthetic interpreter tests: a generated version-100 package with an `Object` class
//! (native operators, script functions) and an `Actor` subclass with two states.

use xiii_package::Limits;

use crate::bytecode::ScriptLimits;
use crate::linker::{GlobalRef, ScriptPackage, ScriptSet};
use crate::reflect::function_flags as ff;
use crate::reflect::property_flags as pf;
use crate::tests::{Exp, build_package, compact};
use crate::value::Value;
use crate::vm::{TraceKind, Vm, VmErrorKind, VmLimits};

struct B {
    names: Vec<String>,
    exports: Vec<Exp>,
}

const IMP_CORE: i32 = -1;
const IMP_FUNCTION: i32 = -2;
const IMP_STATE: i32 = -3;
const IMP_INTPROP: i32 = -4;
const IMP_FLOATPROP: i32 = -5;
const IMP_NAMEPROP: i32 = -6;

impl B {
    fn new() -> Self {
        let mut b = Self {
            names: Vec::new(),
            exports: Vec::new(),
        };
        for n in [
            "None",
            "Core",
            "Class",
            "Package",
            "Function",
            "State",
            "IntProperty",
            "FloatProperty",
            "NameProperty",
            "System",
            "Begin",
        ] {
            b.name(n);
        }
        b
    }

    fn name(&mut self, s: &str) -> i32 {
        match self.names.iter().position(|n| n == s) {
            Some(i) => i as i32,
            None => {
                self.names.push(s.to_owned());
                self.names.len() as i32 - 1
            }
        }
    }

    /// Reserves an export; returns its raw reference (index + 1).
    fn reserve(&mut self, class: i32, outer: i32, name: &str) -> i32 {
        let name = self.name(name);
        self.exports.push(Exp {
            class,
            outer,
            name,
            flags: 0,
            payload: Vec::new(),
        });
        self.exports.len() as i32
    }

    fn set(&mut self, r: i32, payload: Vec<u8>) {
        self.exports[(r - 1) as usize].payload = payload;
    }

    fn prop(&mut self, r: i32, next: i32, flags: u32) {
        let mut p = compact(0); // empty tagged block ("None" is name 0)
        p.extend(compact(0));
        p.extend(compact(next));
        p.extend(1i16.to_le_bytes());
        p.extend(flags.to_le_bytes());
        p.extend(compact(0));
        self.set(r, p);
    }

    fn header(
        &self,
        sup: i32,
        next: i32,
        children: i32,
        friendly: i32,
        script: &[u8],
        mem: u32,
    ) -> Vec<u8> {
        let mut p = Vec::new();
        p.extend(compact(sup));
        p.extend(compact(next));
        p.extend(compact(0));
        p.extend(compact(children));
        p.extend(compact(friendly));
        p.extend(1i32.to_le_bytes());
        p.extend(0i32.to_le_bytes());
        p.extend((mem as i32).to_le_bytes());
        p.extend(script);
        p
    }

    #[allow(clippy::too_many_arguments)]
    fn func(
        &mut self,
        r: i32,
        next: i32,
        children: i32,
        script: &[u8],
        mem: u32,
        native: u16,
        flags: u32,
    ) {
        let friendly = self.exports[(r - 1) as usize].name;
        let mut p = compact(0);
        p.extend(self.header(0, next, children, friendly, script, mem));
        p.extend(native.to_le_bytes());
        p.push(0);
        p.extend(&flags.to_le_bytes()[..3]);
        self.set(r, p);
    }

    fn state(&mut self, r: i32, next: i32, script: &[u8], mem: u32, labels_at: u16) {
        let friendly = self.exports[(r - 1) as usize].name;
        let mut p = compact(0);
        p.extend(self.header(0, next, 0, friendly, script, mem));
        p.extend(u64::MAX.to_le_bytes());
        p.extend(u64::MAX.to_le_bytes());
        p.extend(labels_at.to_le_bytes());
        p.extend(0u16.to_le_bytes());
        self.set(r, p);
    }

    fn class(&mut self, r: i32, sup: i32, children: i32) {
        let friendly = self.exports[(r - 1) as usize].name;
        let system = self.name("System");
        let mut p = self.header(sup, 0, children, friendly, &[], 0);
        p.extend(0u64.to_le_bytes());
        p.extend(u64::MAX.to_le_bytes());
        p.extend(0xFFFFu16.to_le_bytes());
        p.extend(0u16.to_le_bytes());
        p.extend(0u16.to_le_bytes()); // class flags
        p.extend([0u8; 16]);
        p.extend(compact(0)); // dependencies
        p.extend(compact(0)); // package imports
        p.extend(compact(0)); // within
        p.extend(compact(system));
        p.extend(compact(0)); // hide categories
        p.extend(compact(0)); // defaults: None
        self.set(r, p);
    }

    fn build(mut self) -> Vec<u8> {
        let core = self.name("Core");
        let package = self.name("Package");
        let class = self.name("Class");
        let imports = vec![
            (core, package, 0, core),
            (core, class, -1, self.name("Function")),
            (core, class, -1, self.name("State")),
            (core, class, -1, self.name("IntProperty")),
            (core, class, -1, self.name("FloatProperty")),
            (core, class, -1, self.name("NameProperty")),
        ];
        let names: Vec<&str> = self.names.iter().map(String::as_str).collect();
        build_package(&names, &imports, &self.exports)
    }
}

/// Builds the test package. Native indices: 146 `+`, 150 `<` and a duplicate 150 `Unimpl`
/// (0 parameters), 165 `++`, 113 `GotoState`, 256 `Sleep` (latent).
fn fixture() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    // Object functions.
    let add = b.reserve(IMP_FUNCTION, object, "Add_IntInt");
    let less = b.reserve(IMP_FUNCTION, object, "Less_IntInt");
    let inc = b.reserve(IMP_FUNCTION, object, "AddAdd_Int");
    let unimpl = b.reserve(IMP_FUNCTION, object, "Unimpl");
    let goto = b.reserve(IMP_FUNCTION, object, "GotoState");
    let sum = b.reserve(IMP_FUNCTION, object, "Sum");
    let spin = b.reserve(IMP_FUNCTION, object, "Spin");
    let calls_dup = b.reserve(IMP_FUNCTION, object, "CallsDup");
    let calls_unknown = b.reserve(IMP_FUNCTION, object, "CallsUnknownIndex");
    let calls_new = b.reserve(IMP_FUNCTION, object, "CallsNew");
    // Parameters.
    let binop = |b: &mut B, f: i32| {
        let a = b.reserve(IMP_INTPROP, f, "A");
        let bb = b.reserve(IMP_INTPROP, f, "B");
        let r = b.reserve(IMP_INTPROP, f, "ReturnValue");
        b.prop(a, bb, pf::PARM);
        b.prop(bb, r, pf::PARM);
        b.prop(r, 0, pf::PARM | pf::RETURN_PARM);
        a
    };
    let add_a = binop(&mut b, add);
    let less_a = binop(&mut b, less);
    let inc_a = b.reserve(IMP_INTPROP, inc, "A");
    let inc_r = b.reserve(IMP_INTPROP, inc, "ReturnValue");
    b.prop(inc_a, inc_r, pf::PARM | pf::OUT_PARM);
    b.prop(inc_r, 0, pf::PARM | pf::RETURN_PARM);
    let goto_s = b.reserve(IMP_NAMEPROP, goto, "NewState");
    let goto_l = b.reserve(IMP_NAMEPROP, goto, "Label");
    b.prop(goto_s, goto_l, pf::PARM);
    b.prop(goto_l, 0, pf::PARM | pf::OPTIONAL_PARM);
    let n = b.reserve(IMP_INTPROP, sum, "N");
    let i = b.reserve(IMP_INTPROP, sum, "I");
    let s = b.reserve(IMP_INTPROP, sum, "S");
    let ret = b.reserve(IMP_INTPROP, sum, "ReturnValue");
    b.prop(n, i, pf::PARM);
    b.prop(i, s, 0);
    b.prop(s, ret, 0);
    b.prop(ret, 0, pf::PARM | pf::RETURN_PARM);
    let native_op = ff::FINAL | ff::NATIVE | ff::OPERATOR | ff::STATIC;
    b.func(add, less, add_a, &[], 0, 146, native_op);
    b.func(less, inc, less_a, &[], 0, 150, native_op);
    b.func(inc, unimpl, inc_a, &[], 0, 165, native_op);
    b.func(unimpl, goto, 0, &[], 0, 150, ff::FINAL | ff::NATIVE);
    b.func(goto, sum, goto_s, &[], 0, 113, ff::FINAL | ff::NATIVE);
    let (ri, rs, rn) = (i as u8, s as u8, n as u8);
    #[rustfmt::skip]
    let sum_code = [
        0x0F, 0x00, ri, 0x25,                               // 0000 I = 0
        0x0F, 0x00, rs, 0x25,                               // 0007 S = 0
        0x07, 0x39, 0x00, 0x96, 0x00, ri, 0x00, rn, 0x16,   // 000E if !(I < N) goto 0039
        0x0F, 0x00, rs, 0x92, 0x00, rs, 0x00, ri, 0x16,     // 001D S = S + I
        0xA5, 0x00, ri, 0x16,                               // 002F I++
        0x06, 0x0E, 0x00,                                   // 0036 goto 000E
        0x04, 0x00, rs,                                     // 0039 return S
    ];
    b.func(sum, spin, n, &sum_code, 0x3F, 0, ff::DEFINED);
    b.func(spin, calls_dup, 0, &[0x06, 0x00, 0x00], 3, 0, ff::DEFINED);
    b.func(
        calls_dup,
        calls_unknown,
        0,
        &[0x96, 0x16, 0x04, 0x0B],
        4,
        0,
        ff::DEFINED,
    );
    b.func(
        calls_unknown,
        calls_new,
        0,
        &[0xC8, 0x16, 0x04, 0x0B],
        4,
        0,
        ff::DEFINED,
    );
    b.func(
        calls_new,
        0,
        0,
        &[0x11, 0x2A, 0x2A, 0x25, 0x2A, 0x04, 0x0B],
        7,
        0,
        ff::DEFINED,
    );
    // Actor: Counter, Sleep, states Waiting and Done.
    let counter = b.reserve(IMP_INTPROP, actor, "Counter");
    let sleep = b.reserve(IMP_FUNCTION, actor, "Sleep");
    let waiting = b.reserve(IMP_STATE, actor, "Waiting");
    let done = b.reserve(IMP_STATE, actor, "Done");
    let secs = b.reserve(IMP_FLOATPROP, sleep, "Seconds");
    b.prop(counter, sleep, 0);
    b.prop(secs, 0, pf::PARM);
    b.func(
        sleep,
        waiting,
        secs,
        &[],
        0,
        256,
        ff::FINAL | ff::NATIVE | ff::LATENT,
    );
    let begin = b.name("Begin") as u8;
    let done_name = b.name("Done") as u8;
    let rc = counter as u8;
    let mut waiting_code = vec![0x0F, 0x01, rc, 0x26]; // 0000 Counter = 1
    waiting_code.extend([0x61, 0x00, 0x1E]); // 0007 Sleep(0.5)
    waiting_code.extend(0.5f32.to_le_bytes());
    waiting_code.push(0x16);
    waiting_code.extend([0x0F, 0x01, rc, 0x2C, 2]); // 000F Counter = 2
    waiting_code.extend([0x71, 0x21, done_name, 0x16]); // 0017 GotoState('Done')
    waiting_code.push(0x08); // 001E stop
    waiting_code.extend([0x0C, begin, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // 001F labels
    b.state(waiting, done, &waiting_code, 0x30, 0x1F);
    let mut done_code = vec![0x0F, 0x01, rc, 0x2C, 3, 0x08];
    done_code.extend([0x0C, begin, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    b.state(done, 0, &done_code, 0x1A, 0x09);
    b.class(object, 0, add);
    b.class(actor, object, counter);
    let _ = IMP_CORE;
    b.build()
}

fn set_of(data: Vec<u8>) -> ScriptSet {
    let p = ScriptPackage::load("Test", data, &ScriptLimits::default(), &Limits::default())
        .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

fn g(set: &ScriptSet, path: &str) -> GlobalRef {
    GlobalRef {
        package: 0,
        export: set.packages[0].export_by_path(path).expect(path),
    }
}

#[test]
fn arithmetic_loop_and_out_parameter() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let obj = vm.spawn(g(&set, "Object"), "O").unwrap();
    vm.set_active(obj, true);
    let r = vm
        .call_function(g(&set, "Object.Sum"), obj, vec![Value::Int(10)])
        .unwrap();
    assert_eq!(r, Value::Int(45));
    // Duplicate index 150 resolved by argument count: two args -> Less_IntInt.
    assert_eq!(vm.natives_used["Object.Less_IntInt"].1, 11);
    assert_eq!(vm.natives_used["Object.AddAdd_Int"].1, 10);
}

#[test]
fn state_labels_latent_sleep_and_goto_state() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(g(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    vm.goto_state(a, "Waiting", None).unwrap();
    assert_eq!(vm.state_name(a).as_deref(), Some("Waiting"));
    vm.tick(0.25).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(1)));
    vm.tick(0.25).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(1)));
    vm.tick(0.25).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(3)));
    assert_eq!(vm.state_name(a).as_deref(), Some("Done"));
    let kinds: Vec<&TraceKind> = vm.trace.iter().map(|e| &e.kind).collect();
    let resume = vm
        .trace
        .iter()
        .find(|e| matches!(e.kind, TraceKind::LatentResume { .. }))
        .expect("resume");
    assert_eq!(resume.tick, 3);
    assert!((resume.time - 0.75).abs() < 1e-6);
    assert!(
        matches!(resume.kind, TraceKind::LatentResume { started, .. } if (started - 0.25).abs() < 1e-6)
    );
    assert!(
        kinds
            .iter()
            .any(|k| matches!(k, TraceKind::StateChange { to: Some(t), .. } if t == "Done"))
    );
    assert!(matches!(kinds.last(), Some(TraceKind::StateStop { .. })));
    // State code is done: further ticks change nothing.
    vm.tick(0.25).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(3)));
}

#[test]
fn latent_native_outside_state_code_fails() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(g(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    let e = vm
        .call_function(g(&set, "Actor.Sleep"), a, vec![Value::Float(1.0)])
        .unwrap_err();
    assert!(
        matches!(e.kind, VmErrorKind::LatentOutsideState { .. }),
        "{e}"
    );
}

#[test]
fn unimplemented_and_unregistered_natives_fail_with_stack() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let obj = vm.spawn(g(&set, "Object"), "O").unwrap();
    vm.set_active(obj, true);
    // Index 150 with no arguments resolves to the 0-parameter duplicate, which has no
    // implementation: explicit failure naming it, with the calling script on the stack.
    let e = vm
        .call_function(g(&set, "Object.CallsDup"), obj, vec![])
        .unwrap_err();
    assert_eq!(
        e.kind,
        VmErrorKind::UnimplementedNative {
            path: "Object.Unimpl".into(),
            index: Some(150)
        }
    );
    assert_eq!(e.stack.last().unwrap().function, "Test.Object.CallsDup");
    assert_eq!(e.stack.last().unwrap().offset, 0);
    assert!(e.to_string().contains("at Test.Object.CallsDup"));
    let e = vm
        .call_function(g(&set, "Object.CallsUnknownIndex"), obj, vec![])
        .unwrap_err();
    assert_eq!(e.kind, VmErrorKind::UnregisteredNative { index: 200 });
    let e = vm
        .call_function(g(&set, "Object.CallsNew"), obj, vec![])
        .unwrap_err();
    assert!(
        matches!(e.kind, VmErrorKind::UnsupportedToken { opcode: 0x11, .. }),
        "{e}"
    );
}

#[test]
fn step_budget_stops_runaway_loops() {
    let set = set_of(fixture());
    let mut vm = Vm::new(
        &set,
        VmLimits {
            max_steps: 1000,
            ..VmLimits::default()
        },
    );
    let obj = vm.spawn(g(&set, "Object"), "O").unwrap();
    vm.set_active(obj, true);
    let e = vm
        .call_function(g(&set, "Object.Spin"), obj, vec![])
        .unwrap_err();
    assert_eq!(e.kind, VmErrorKind::BudgetExceeded { limit: 1000 });
    assert_eq!(e.stack.last().unwrap().function, "Test.Object.Spin");
}

#[test]
fn inactive_objects_do_not_run_state_code() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(g(&set, "Actor"), "Sleeper").unwrap();
    // Not active (outside the executed scope): its state code never runs in tick().
    vm.goto_state(a, "Waiting", None).unwrap();
    vm.tick(0.25).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(0)));
}

#[test]
fn registry_entries_are_documented() {
    let r = crate::registry::Registry::builtin();
    let defs: Vec<_> = r.defs().collect();
    assert_eq!(defs.len(), 47);
    for d in defs {
        assert!(
            !d.signature.is_empty() && !d.evidence.is_empty(),
            "{}",
            d.path
        );
        assert!(
            d.path.contains('.') && !d.path.starts_with("Engine."),
            "{}",
            d.path
        );
    }
}
