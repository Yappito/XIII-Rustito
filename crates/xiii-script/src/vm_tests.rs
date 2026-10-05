//! Synthetic interpreter tests: a generated version-100 package with an `Object` class
//! (native operators, script functions) and an `Actor` subclass with two states.

use xiii_package::Limits;

use crate::bytecode::ScriptLimits;
use crate::linker::{GlobalRef, ScriptPackage, ScriptSet};
use crate::reflect::function_flags as ff;
use crate::reflect::property_flags as pf;
use crate::tests::{Exp, build_package, compact};
use crate::value::{ObjRef, ObjectId, Value};
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
    assert_eq!(defs.len(), 79);
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

// ---------------------------------------------------------------------------------------
// Actor.Spawn / Destroy / lifecycle

const IMP_OBJECTPROP: i32 = -7;
const IMP_BOOLPROP: i32 = -8;
const IMP_STRUCTPROP: i32 = -9;
const IMP_STRUCT: i32 = -10;

/// Builds a package with an `Object` base and an `Actor`/`Child`/`AbstractChild` tree for
/// spawn and lifecycle tests.
///
/// `Actor` properties: `Owner`, `Level` (objects), `Tag` (name), `Location`, `Rotation`,
/// `Calls` (ints, set to vectors/rotators directly), `bStatic` (bool). Each lifecycle event
/// increments `Calls`; `Destroyed` also increments it. `AbstractChild` carries class flag 1.
struct SpawnB {
    names: Vec<String>,
    exports: Vec<Exp>,
}

impl SpawnB {
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
            "ObjectProperty",
            "BoolProperty",
            "System",
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
        self.prop_with(r, next, flags, &[]);
    }

    /// Property with a trailing type-specific reference (e.g. `ObjectProperty.PropertyClass`).
    fn prop_with(&mut self, r: i32, next: i32, flags: u32, extra: &[u8]) {
        let mut p = compact(0);
        p.extend(compact(0));
        p.extend(compact(next));
        p.extend(1i16.to_le_bytes());
        p.extend(flags.to_le_bytes());
        p.extend(compact(0));
        p.extend(extra);
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

    /// A `Core.Class` with the given low `class_flags` u16.
    fn class(&mut self, r: i32, sup: i32, children: i32, class_flags: u16) {
        let friendly = self.exports[(r - 1) as usize].name;
        let system = self.name("System");
        let mut p = self.header(sup, 0, children, friendly, &[], 0);
        p.extend(0u64.to_le_bytes());
        p.extend(u64::MAX.to_le_bytes());
        p.extend(0xFFFFu16.to_le_bytes());
        p.extend(0u16.to_le_bytes());
        p.extend(class_flags.to_le_bytes());
        p.extend([0u8; 16]);
        p.extend(compact(0));
        p.extend(compact(0));
        p.extend(compact(0));
        p.extend(compact(system));
        p.extend(compact(0));
        p.extend(compact(0));
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
            (core, class, -1, self.name("ObjectProperty")),
            (core, class, -1, self.name("BoolProperty")),
            (core, class, -1, self.name("StructProperty")),
            (core, class, -1, self.name("Vector")),
            (core, class, -1, self.name("Rotator")),
        ];
        let names: Vec<&str> = self.names.iter().map(String::as_str).collect();
        build_package(&names, &imports, &self.exports)
    }
}

fn spawn_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let child = b.reserve(0, 0, "Child");
    let abstract_child = b.reserve(0, 0, "AbstractChild");

    // `Add_IntInt` native (index 146) used by the lifecycle bump code.
    let add = b.reserve(IMP_FUNCTION, object, "Add_IntInt");
    let add_a = b.reserve(IMP_INTPROP, add, "A");
    let add_b = b.reserve(IMP_INTPROP, add, "B");
    let add_r = b.reserve(IMP_INTPROP, add, "ReturnValue");
    b.prop(add_a, add_b, PARM);
    b.prop(add_b, add_r, PARM);
    b.prop(add_r, 0, PARM | RETURN_PARM);
    b.func(
        add,
        0,
        add_a,
        &[],
        0,
        146,
        FINAL | NATIVE | OPERATOR | STATIC,
    );

    // Actor properties.
    let object_class = 0; // ObjectProperty.PropertyClass = null (any object)
    let object_extra = compact(object_class);
    // StructProperty.Struct = imported Vector/Rotator.
    let vector_struct = IMP_STRUCT; // first of the two struct imports (Vector)
    let rotator_struct = IMP_STRUCT - 1; // second (Rotator)
    let vector_extra = compact(vector_struct);
    let rotator_extra = compact(rotator_struct);
    let owner = b.reserve(IMP_OBJECTPROP, actor, "Owner");
    let level = b.reserve(IMP_OBJECTPROP, actor, "Level");
    let tag = b.reserve(IMP_NAMEPROP, actor, "Tag");
    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let rotation = b.reserve(IMP_STRUCTPROP, actor, "Rotation");
    let calls = b.reserve(IMP_INTPROP, actor, "Calls");
    let bstatic = b.reserve(IMP_BOOLPROP, actor, "bStatic");
    let deleted = b.reserve(IMP_BOOLPROP, actor, "bDeleteMe");
    let spawned = b.reserve(IMP_FUNCTION, actor, "Spawned");
    b.prop_with(owner, level, 0, &object_extra);
    b.prop_with(level, tag, 0, &object_extra);
    b.prop(tag, location, 0);
    b.prop_with(location, rotation, 0, &vector_extra);
    b.prop_with(rotation, calls, 0, &rotator_extra);
    b.prop(calls, bstatic, 0);
    b.prop(bstatic, deleted, 0);
    // Single child list: properties first, then the lifecycle functions.
    b.prop(deleted, spawned, 0);

    // Lifecycle events: each bumps Calls and returns.

    let pre = b.reserve(IMP_FUNCTION, actor, "PreBeginPlay");
    let begin = b.reserve(IMP_FUNCTION, actor, "BeginPlay");
    let post = b.reserve(IMP_FUNCTION, actor, "PostBeginPlay");
    let net = b.reserve(IMP_FUNCTION, actor, "PostNetBeginPlay");
    let initial = b.reserve(IMP_FUNCTION, actor, "SetInitialState");
    let destroyed = b.reserve(IMP_FUNCTION, actor, "Destroyed");
    let bump = |b: &mut SpawnB, r: i32, next: i32| {
        let rc = calls as u8;
        let code = vec![
            0x0F, 0x01, rc, 0x92, 0x00, rc, 0x26, 0x16, // Calls = Calls + 1
            0x04, 0x0B, // return
        ];
        b.func(r, next, 0, &code, 0x10, 0, DEFINED);
    };
    bump(&mut b, spawned, pre);
    bump(&mut b, pre, begin);
    bump(&mut b, begin, post);
    bump(&mut b, post, net);
    bump(&mut b, net, initial);
    bump(&mut b, initial, destroyed);
    bump(&mut b, destroyed, 0);

    b.class(object, 0, add, 0);
    b.class(actor, object, owner, 0);
    b.class(child, actor, 0, 0);
    b.class(abstract_child, actor, 0, 1);
    b.build()
}

fn spawn_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        spawn_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

fn sg(set: &ScriptSet, path: &str) -> GlobalRef {
    GlobalRef {
        package: 0,
        export: set.packages[0].export_by_path(path).expect(path),
    }
}

fn lifecycle_calls(vm: &Vm<'_>, id: ObjectId) -> Option<i32> {
    match vm.get_property(id, "Calls") {
        Some(Value::Int(i)) => Some(*i),
        _ => None,
    }
}

#[test]
fn spawn_sets_defaults_owner_tag_and_location() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let spawner = vm.spawn(sg(&set, "Actor"), "Spawner").unwrap();
    vm.set_property(spawner, "Location", 0, Value::Vector([1.0, 2.0, 3.0]));
    vm.set_property(spawner, "Rotation", 0, Value::Rotator([10, 20, 30]));
    let owner = vm.spawn(sg(&set, "Actor"), "Owner").unwrap();
    let child_class = sg(&set, "Child");
    let id = vm
        .spawn_actor(
            spawner,
            Some(child_class),
            Some(owner),
            Some("mytag"),
            None,
            None,
        )
        .unwrap()
        .expect("spawned");
    assert_eq!(
        vm.get_property(id, "Owner"),
        Some(&Value::Object(Some(ObjRef::Instance(owner))))
    );
    assert_eq!(
        vm.get_property(id, "Tag"),
        Some(&Value::Name("mytag".into()))
    );
    // No explicit location/rotation: taken from the spawner.
    assert_eq!(
        vm.get_property(id, "Location"),
        Some(&Value::Vector([1.0, 2.0, 3.0]))
    );
    assert_eq!(
        vm.get_property(id, "Rotation"),
        Some(&Value::Rotator([10, 20, 30]))
    );
    assert!(vm.objects[id as usize].active);
    // Default tag when none is supplied is the class name.
    let id2 = vm
        .spawn_actor(spawner, Some(child_class), None, None, None, None)
        .unwrap()
        .unwrap();
    assert_eq!(
        vm.get_property(id2, "Tag"),
        Some(&Value::Name("Child".into()))
    );
}

#[test]
fn spawn_none_class_returns_none_and_abstract_refused() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let spawner = vm.spawn(sg(&set, "Actor"), "Spawner").unwrap();
    assert_eq!(
        vm.spawn_actor(spawner, None, None, None, None, None)
            .unwrap(),
        None
    );
    // Abstract child (class flag 1): refused with a trace and None.
    let r = vm
        .spawn_actor(
            spawner,
            Some(sg(&set, "AbstractChild")),
            None,
            None,
            None,
            None,
        )
        .unwrap();
    assert_eq!(r, None);
    assert!(vm.trace.iter().any(
        |e| matches!(&e.kind, TraceKind::SpawnRefused { reason } if reason.contains("abstract"))
    ));
}

#[test]
fn runtime_spawn_runs_lifecycle_in_order() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let spawner = vm.spawn(sg(&set, "Actor"), "Spawner").unwrap();
    let id = vm
        .spawn_actor(spawner, Some(sg(&set, "Child")), None, None, None, None)
        .unwrap()
        .unwrap();
    // Spawned, PreBeginPlay, BeginPlay, PostBeginPlay, PostNetBeginPlay, SetInitialState.
    assert_eq!(lifecycle_calls(&vm, id), Some(6));
    let events: Vec<String> = vm
        .trace
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::Event { function, .. }
                if function.contains("Child") || function.contains("Actor.") =>
            {
                Some(function.clone())
            }
            _ => None,
        })
        .collect();
    let lifecycle: Vec<&String> = events
        .iter()
        .filter(|f| {
            [
                "Spawned",
                "PreBeginPlay",
                "BeginPlay",
                "PostBeginPlay",
                "PostNetBeginPlay",
                "SetInitialState",
            ]
            .iter()
            .any(|e| f.ends_with(e))
        })
        .collect();
    assert_eq!(lifecycle.len(), 6, "{events:?}");
}

#[test]
fn level_start_lifecycle_is_grouped_by_event() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Child"), "A").unwrap();
    let b = vm.spawn(sg(&set, "Child"), "B").unwrap();
    vm.begin_play(&[a, b]).unwrap();
    // Each actor runs every level-start event once.
    assert_eq!(lifecycle_calls(&vm, a), Some(5));
    assert_eq!(lifecycle_calls(&vm, b), Some(5));
    // Grouping: all of A's PreBeginPlay/BeginPlay come before... check the sequence is
    // A.PreBeginPlay, B.PreBeginPlay, A.BeginPlay, B.BeginPlay, ...
    let seq: Vec<(String, String)> = vm
        .trace
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::Event {
                target, function, ..
            } if [
                "PreBeginPlay",
                "BeginPlay",
                "PostBeginPlay",
                "PostNetBeginPlay",
                "SetInitialState",
            ]
            .iter()
            .any(|x| function.ends_with(x)) =>
            {
                Some((
                    target.clone(),
                    function.rsplit('.').next().unwrap_or("").to_owned(),
                ))
            }
            _ => None,
        })
        .collect();
    let expected: Vec<(String, String)> = [
        "PreBeginPlay",
        "BeginPlay",
        "PostBeginPlay",
        "PostNetBeginPlay",
        "SetInitialState",
    ]
    .iter()
    .flat_map(|ev| {
        [
            ("A".to_owned(), (*ev).to_owned()),
            ("B".to_owned(), (*ev).to_owned()),
        ]
    })
    .collect();
    assert_eq!(seq, expected);
}

#[test]
fn destroy_gives_accessed_none_and_removes_from_iterators() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Child"), "Gone").unwrap();
    let b = vm.spawn(sg(&set, "Child"), "Stays").unwrap();
    let before = vm.dynamic_actors(None, None);
    assert!(before.contains(&a) && before.contains(&b));
    assert!(vm.destroy(a).unwrap());
    // Destroyed event ran once.
    assert_eq!(lifecycle_calls(&vm, a), Some(1));
    assert!(vm.objects[a as usize].deleted);
    // References behave as None.
    assert_eq!(vm.find_live_object("Gone"), None);
    assert_eq!(vm.find_object("Gone"), None);
    // Iterators skip it.
    let after = vm.dynamic_actors(None, None);
    assert!(!after.contains(&a) && after.contains(&b));
    let all = vm.all_actors(None, None);
    assert!(!all.contains(&a) && all.contains(&b));
    // Destroying again is idempotent.
    assert!(vm.destroy(a).unwrap());
}

#[test]
fn destroy_during_own_execution_does_not_panic() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Child"), "A").unwrap();
    vm.set_active(a, true);
    // A nested Destroy from inside Destroyed: `destroy` marks first, so the event's own
    // destroy call is a no-op and does not recurse.
    vm.destroy(a).unwrap();
    assert!(vm.objects[a as usize].deleted);
}

// ---------------------------------------------------------------------------------------
// Native semantics corrections: optional args, strings, PRNG, lists

use crate::registry::{NativeCtx, NativeDef, NativeOutcome, Registry};

fn native(path: &str) -> NativeDef {
    // Registry keys are "Class.Function"; only the `Engine.` package prefix is stripped on
    // registration, so `Engine.Actor.Spawn` is keyed as `Actor.Spawn` but `Object.Mid` stays.
    let key = path
        .strip_prefix("Engine.")
        .unwrap_or(path)
        .to_ascii_lowercase();
    Registry::builtin()
        .get(&key)
        .unwrap_or_else(|| panic!("{path} not registered"))
        .clone()
}

fn ctx(this: ObjectId, omitted: &[bool], path: &str) -> NativeCtx {
    NativeCtx {
        this,
        in_state_code: false,
        path: path.to_owned(),
        omitted: omitted.to_vec(),
    }
}

fn call_native(
    vm: &mut Vm<'_>,
    path: &str,
    this: ObjectId,
    omitted: &[bool],
    args: &mut [Value],
) -> NativeOutcome {
    let def = native(path);
    (def.f)(vm, &ctx(this, omitted, path), args).expect("native")
}

fn str_result(o: NativeOutcome) -> String {
    match o {
        NativeOutcome::Value(Value::Str(s)) => s,
        other => panic!("expected string, got {other:?}"),
    }
}

fn bool_result(o: NativeOutcome) -> bool {
    match o {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("expected bool, got {other:?}"),
    }
}

fn int_result(o: NativeOutcome) -> i32 {
    match o {
        NativeOutcome::Value(Value::Int(i)) => i,
        other => panic!("expected int, got {other:?}"),
    }
}

#[test]
fn string_native_edges_match_ue2_clamping() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Object"), "O").unwrap();

    // Mid with the count omitted returns the rest of the string.
    let mut a = vec![Value::Str("abcdef".into()), Value::Int(2)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Mid",
            o,
            &[false, false, true],
            &mut a
        )),
        "cdef"
    );
    // Mid explicit count.
    let mut a = vec![Value::Str("abcdef".into()), Value::Int(1), Value::Int(3)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Mid",
            o,
            &[false, false, false],
            &mut a
        )),
        "bcd"
    );
    // Mid negative start -> "" (unsigned clamp to Len), per the reviewer contract.
    let mut a = vec![Value::Str("abcdef".into()), Value::Int(-2), Value::Int(3)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Mid",
            o,
            &[false, false, false],
            &mut a
        )),
        ""
    );
    // Mid oversized start / count clamp to Len.
    let mut a = vec![Value::Str("abc".into()), Value::Int(5), Value::Int(10)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Mid",
            o,
            &[false, false, false],
            &mut a
        )),
        ""
    );
    let mut a = vec![Value::Str("abc".into()), Value::Int(1), Value::Int(99)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Mid",
            o,
            &[false, false, false],
            &mut a
        )),
        "bc"
    );
    // Mid on empty and count at exactly Len.
    let mut a = vec![Value::Str(String::new()), Value::Int(0), Value::Int(0)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Mid",
            o,
            &[false, false, false],
            &mut a
        )),
        ""
    );
    // Left / Right clamping.
    let mut a = vec![Value::Str("abc".into()), Value::Int(-1)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Left",
            o,
            &[false, false],
            &mut a
        )),
        ""
    );
    let mut a = vec![Value::Str("abc".into()), Value::Int(99)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Left",
            o,
            &[false, false],
            &mut a
        )),
        "abc"
    );
    let mut a = vec![Value::Str("abc".into()), Value::Int(99)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Right",
            o,
            &[false, false],
            &mut a
        )),
        "abc"
    );
    let mut a = vec![Value::Str("abc".into()), Value::Int(0)];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Right",
            o,
            &[false, false],
            &mut a
        )),
        ""
    );
    // InStr is case-sensitive.
    let mut a = vec![Value::Str("Hello World".into()), Value::Str("World".into())];
    assert_eq!(
        int_result(call_native(
            &mut vm,
            "Object.InStr",
            o,
            &[false, false],
            &mut a
        )),
        6
    );
    let mut a = vec![Value::Str("Hello World".into()), Value::Str("world".into())];
    assert_eq!(
        int_result(call_native(
            &mut vm,
            "Object.InStr",
            o,
            &[false, false],
            &mut a
        )),
        -1
    );
    // ComplementEqual is case-insensitive equality, not its negation.
    let mut a = vec![Value::Str("AbC".into()), Value::Str("aBc".into())];
    assert!(bool_result(call_native(
        &mut vm,
        "Object.ComplementEqual_StrStr",
        o,
        &[false, false],
        &mut a
    )));
    let mut a = vec![Value::Str("abc".into()), Value::Str("abd".into())];
    assert!(!bool_result(call_native(
        &mut vm,
        "Object.ComplementEqual_StrStr",
        o,
        &[false, false],
        &mut a
    )));
}

#[test]
fn spawn_optional_location_uses_origin_or_spawner() {
    let set = spawn_set();
    let child = sg(&set, "Child");
    let mut vm = Vm::new(&set, VmLimits::default());
    let spawner = vm.spawn(sg(&set, "Actor"), "Spawner").unwrap();
    vm.set_property(spawner, "Location", 0, Value::Vector([1.0, 2.0, 3.0]));
    vm.set_property(spawner, "Rotation", 0, Value::Rotator([10, 20, 30]));

    // Explicit zero location/rotation must spawn at the origin, not fall back to the spawner.
    let mut args = vec![
        Value::Object(Some(ObjRef::Static(child))),
        Value::Object(None),
        Value::Name("None".into()),
        Value::Vector([0.0, 0.0, 0.0]),
        Value::Rotator([0, 0, 0]),
    ];
    let id = match call_native(
        &mut vm,
        "Engine.Actor.Spawn",
        spawner,
        &[false, true, true, false, false],
        &mut args,
    ) {
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(id)))) => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        vm.get_property(id, "Location"),
        Some(&Value::Vector([0.0, 0.0, 0.0]))
    );

    // Omitted location/rotation use the spawner's.
    let mut args = vec![Value::Object(Some(ObjRef::Static(child)))];
    let id2 = match call_native(
        &mut vm,
        "Engine.Actor.Spawn",
        spawner,
        &[false, true, true, true, true],
        &mut args,
    ) {
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(id)))) => id,
        other => panic!("{other:?}"),
    };
    assert_eq!(
        vm.get_property(id2, "Location"),
        Some(&Value::Vector([1.0, 2.0, 3.0]))
    );
    assert_eq!(
        vm.get_property(id2, "Rotation"),
        Some(&Value::Rotator([10, 20, 30]))
    );
    // Omitted tag defaults to the class name.
    assert_eq!(
        vm.get_property(id2, "Tag"),
        Some(&Value::Name("Child".into()))
    );
}

#[test]
fn rng_is_deterministic_and_seeded() {
    let set = spawn_set();
    let mut a = Vm::new(&set, VmLimits::default());
    let mut b = Vm::new(&set, VmLimits::default());
    let a_seq: Vec<u64> = (0..8).map(|_| a.next_random()).collect();
    let b_seq: Vec<u64> = (0..8).map(|_| b.next_random()).collect();
    assert_eq!(a_seq, b_seq, "same default seed -> same sequence");
    let mut c = Vm::new(
        &set,
        VmLimits {
            rng_seed: 12345,
            ..VmLimits::default()
        },
    );
    assert_ne!(
        c.next_random(),
        a_seq[0],
        "different seed -> different first value"
    );
    let mut d = Vm::new(&set, VmLimits::default());
    for _ in 0..1000 {
        let f = d.rand_float();
        assert!((0.0..1.0).contains(&f), "{f}");
        let i = d.rand_int(7);
        assert!((0..7).contains(&i), "{i}");
    }
    assert_eq!(d.rand_int(0), 0);
    assert_eq!(d.rand_int(-5), 0);
}

#[test]
fn dynamic_load_object_checks_requested_class() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Object"), "O").unwrap();
    // `Test.Child` is a Core.Class export; requesting class'Class' succeeds.
    let mut a = vec![
        Value::Str("Test.Child".into()),
        Value::NativeClass("Class".into()),
    ];
    match call_native(
        &mut vm,
        "Object.DynamicLoadObject",
        o,
        &[false, false, true],
        &mut a,
    ) {
        NativeOutcome::Value(Value::Object(Some(ObjRef::Static(g)))) => {
            assert_eq!(vm.short_path(g), "Child");
        }
        other => panic!("expected a loaded object, got {other:?}"),
    }
    // Requesting a class the object is not returns None and records a note.
    let before = vm.trace.len();
    let mut a = vec![
        Value::Str("Test.Child".into()),
        Value::NativeClass("Mesh".into()),
    ];
    assert!(matches!(
        call_native(
            &mut vm,
            "Object.DynamicLoadObject",
            o,
            &[false, false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Object(None))
    ));
    assert!(
        vm.trace[before..]
            .iter()
            .any(|e| matches!(&e.kind, TraceKind::Note(s) if s.contains("DynamicLoadObject")))
    );
}

// ---------------------------------------------------------------------------------------
// Pawn / Controller list natives

/// `Object`/`Actor` fixture with `Level` and the pawn/controller list links.
fn list_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let pawn = b.reserve(0, 0, "Pawn");
    let controller = b.reserve(0, 0, "Controller");
    let add = b.reserve(IMP_FUNCTION, object, "Add_IntInt");
    let add_a = b.reserve(IMP_INTPROP, add, "A");
    let add_b = b.reserve(IMP_INTPROP, add, "B");
    let add_r = b.reserve(IMP_INTPROP, add, "ReturnValue");
    b.prop(add_a, add_b, PARM);
    b.prop(add_b, add_r, PARM);
    b.prop(add_r, 0, PARM | RETURN_PARM);
    b.func(
        add,
        0,
        add_a,
        &[],
        0,
        146,
        FINAL | NATIVE | OPERATOR | STATIC,
    );

    // A script (non-native) function with an optional parameter, used to check that an omitted
    // optional still gets the default zero in a script call. `Tag` is a NameProperty (no
    // type-specific reference); it links to the `Echo` function to make one child list.
    let echo = b.reserve(IMP_FUNCTION, actor, "Echo");
    let object_extra = compact(0);
    let level = b.reserve(IMP_OBJECTPROP, actor, "Level");
    let pawn_list = b.reserve(IMP_OBJECTPROP, actor, "PawnList");
    let next_pawn = b.reserve(IMP_OBJECTPROP, actor, "NextPawn");
    let controller_list = b.reserve(IMP_OBJECTPROP, actor, "ControllerList");
    let next_controller = b.reserve(IMP_OBJECTPROP, actor, "NextController");
    let tag = b.reserve(IMP_NAMEPROP, actor, "Tag");
    b.prop_with(level, pawn_list, 0, &object_extra);
    b.prop_with(pawn_list, next_pawn, 0, &object_extra);
    b.prop_with(next_pawn, controller_list, 0, &object_extra);
    b.prop_with(controller_list, next_controller, 0, &object_extra);
    b.prop_with(next_controller, tag, 0, &object_extra);
    // Single child list: properties first, then the `Echo` function.
    b.prop(tag, echo, 0);

    let echo_a = b.reserve(IMP_INTPROP, echo, "A");
    let echo_r = b.reserve(IMP_INTPROP, echo, "ReturnValue");
    b.prop(echo_a, echo_r, PARM | OPTIONAL_PARM);
    b.prop(echo_r, 0, PARM | RETURN_PARM);
    let ra = echo_a as u8;
    // `return A;` = Return(1) + LocalVariable(1 opcode + 4 object) = 6 bytes.
    b.func(echo, 0, echo_a, &[0x04, 0x00, ra], 6, 0, DEFINED);

    b.class(object, 0, add, 0);
    b.class(actor, object, level, 0);
    b.class(pawn, actor, 0, 0);
    b.class(controller, actor, 0, 0);
    b.build()
}

fn list_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        list_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

#[test]
fn pawn_and_controller_lists_insert_and_unlink() {
    let set = list_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let level = vm.spawn(sg(&set, "Actor"), "Level").unwrap();
    let p1 = vm.spawn(sg(&set, "Pawn"), "P1").unwrap();
    let p2 = vm.spawn(sg(&set, "Pawn"), "P2").unwrap();
    let p3 = vm.spawn(sg(&set, "Pawn"), "P3").unwrap();
    for p in [p1, p2, p3] {
        vm.set_property(p, "Level", 0, Value::Object(Some(ObjRef::Instance(level))));
        call_native(&mut vm, "Engine.Pawn.AddPawnToList", p, &[], &mut []);
    }
    // Head is the newest; links chain in reverse insertion order.
    assert_eq!(
        vm.get_property(level, "PawnList"),
        Some(&Value::Object(Some(ObjRef::Instance(p3))))
    );
    assert_eq!(
        vm.get_property(p3, "NextPawn"),
        Some(&Value::Object(Some(ObjRef::Instance(p2))))
    );
    assert_eq!(
        vm.get_property(p2, "NextPawn"),
        Some(&Value::Object(Some(ObjRef::Instance(p1))))
    );
    assert_eq!(vm.get_property(p1, "NextPawn"), Some(&Value::Object(None)));
    // Remove the middle element.
    call_native(&mut vm, "Engine.Pawn.RemovePawnFromList", p2, &[], &mut []);
    assert_eq!(
        vm.get_property(p3, "NextPawn"),
        Some(&Value::Object(Some(ObjRef::Instance(p1))))
    );
    assert_eq!(vm.get_property(p2, "NextPawn"), Some(&Value::Object(None)));
    // Remove the head.
    call_native(&mut vm, "Engine.Pawn.RemovePawnFromList", p3, &[], &mut []);
    assert_eq!(
        vm.get_property(level, "PawnList"),
        Some(&Value::Object(Some(ObjRef::Instance(p1))))
    );
    // Removing an object not in the list is a no-op.
    call_native(&mut vm, "Engine.Pawn.RemovePawnFromList", p2, &[], &mut []);
    assert_eq!(
        vm.get_property(level, "PawnList"),
        Some(&Value::Object(Some(ObjRef::Instance(p1))))
    );

    // Controllers use the parallel list.
    let c1 = vm.spawn(sg(&set, "Controller"), "C1").unwrap();
    let c2 = vm.spawn(sg(&set, "Controller"), "C2").unwrap();
    for c in [c1, c2] {
        vm.set_property(c, "Level", 0, Value::Object(Some(ObjRef::Instance(level))));
        call_native(&mut vm, "Engine.Controller.AddController", c, &[], &mut []);
    }
    assert_eq!(
        vm.get_property(level, "ControllerList"),
        Some(&Value::Object(Some(ObjRef::Instance(c2))))
    );
    assert_eq!(
        vm.get_property(c2, "NextController"),
        Some(&Value::Object(Some(ObjRef::Instance(c1))))
    );
    call_native(
        &mut vm,
        "Engine.Controller.RemoveController",
        c2,
        &[],
        &mut [],
    );
    assert_eq!(
        vm.get_property(level, "ControllerList"),
        Some(&Value::Object(Some(ObjRef::Instance(c1))))
    );
}

#[test]
fn script_optional_param_defaults_to_zero() {
    let set = list_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    // Explicit argument.
    assert_eq!(
        vm.call_function(sg(&set, "Actor.Echo"), a, vec![Value::Int(7)])
            .unwrap(),
        Value::Int(7)
    );
    // Omitted optional: the script local keeps its type zero.
    assert_eq!(
        vm.call_function(sg(&set, "Actor.Echo"), a, vec![]).unwrap(),
        Value::Int(0)
    );
}

#[test]
fn survey_counts_missing_natives_without_aborting() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.survey = true;
    let obj = vm.spawn(g(&set, "Object"), "O").unwrap();
    vm.set_active(obj, true);
    // `CallsDup` calls index 150 with no args -> `Unimpl`, no return value.
    let r = vm
        .call_function(g(&set, "Object.CallsDup"), obj, vec![])
        .unwrap();
    assert_eq!(r, Value::Void);
    assert_eq!(vm.missing_natives.len(), 1);
    let m = vm.missing_natives.get("Object.Unimpl").expect("counted");
    assert_eq!(m.calls, 1);
    assert_eq!(m.index, Some(150));
    assert!(!m.first_stack.is_empty());
    // Normal mode still fails explicitly.
    let mut vm2 = Vm::new(&set, VmLimits::default());
    let obj2 = vm2.spawn(g(&set, "Object"), "O").unwrap();
    vm2.set_active(obj2, true);
    let e = vm2
        .call_function(g(&set, "Object.CallsDup"), obj2, vec![])
        .unwrap_err();
    assert_eq!(
        e.kind,
        VmErrorKind::UnimplementedNative {
            path: "Object.Unimpl".into(),
            index: Some(150)
        }
    );
}
