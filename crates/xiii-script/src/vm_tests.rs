//! Synthetic interpreter tests: a generated version-100 package with an `Object` class
//! (native operators, script functions) and an `Actor` subclass with two states.

use xiii_package::Limits;

use crate::bytecode::ScriptLimits;
use crate::linker::{GlobalRef, ScriptPackage, ScriptSet};
use crate::reflect::function_flags as ff;
use crate::reflect::property_flags as pf;
use crate::tests::{Exp, build_package, compact};
use crate::value::{ObjRef, ObjectId, Value};
use crate::vm::{Latent, TraceKind, Vm, VmErrorKind, VmLimits};

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
            (core, class, -1, self.name("ObjectProperty")),
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
    // `new` is implemented: `new None` yields `None`; the function then returns void.
    assert_eq!(
        vm.call_function(g(&set, "Object.CallsNew"), obj, vec![])
            .unwrap(),
        Value::Void
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
    assert_eq!(defs.len(), 183);
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
/// `Core.ByteProperty` (appended after `ArrayProperty`; see [`SpawnB::build`]).
const IMP_BYTEPROP: i32 = -13;
/// `Core.Struct` import (appended after `ByteProperty`; used by the AI fixture).
const IMP_STRUCT_CLASS: i32 = -14;

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

    /// Dynamic `ArrayProperty` whose element template is the export `inner`.
    fn prop_array(&mut self, r: i32, next: i32, flags: u32, inner: i32) {
        let extra = compact(inner);
        self.prop_with(r, next, flags, &extra);
    }

    /// `ArrayProperty` with an explicit static `ArrayDim` (the payload's i16 field).
    fn prop_array_dim(&mut self, r: i32, next: i32, flags: u32, inner: i32, dim: i16) {
        let extra = compact(inner);
        let mut p = compact(0);
        p.extend(compact(0));
        p.extend(compact(next));
        p.extend(dim.to_le_bytes());
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

    /// A `Core.Struct` with the given first `children` property.
    fn strukt(&mut self, r: i32, children: i32) {
        let friendly = self.exports[(r - 1) as usize].name;
        // Leading `UObject` tagged-property terminator, as for properties.
        let mut p = compact(0);
        p.extend(self.header(0, 0, children, friendly, &[], 0));
        self.set(r, p);
    }

    /// A `Core.Class` whose tagged defaults set one int property to `default_value`.
    fn class_with_int_default(
        &mut self,
        r: i32,
        sup: i32,
        children: i32,
        class_flags: u16,
        default_name: i32,
        default_value: i32,
    ) {
        let friendly = self.exports[(r - 1) as usize].name;
        let system = self.name("System");
        let mut p = self.header(sup, 0, children, friendly, &[], 0);
        p.extend(0u64.to_le_bytes());
        p.extend(u64::MAX.to_le_bytes());
        p.extend(0xFFFFu16.to_le_bytes());
        p.extend(0u16.to_le_bytes());
        p.extend(class_flags.to_le_bytes());
        p.extend([0u8; 16]);
        p.extend(compact(0)); // dependencies
        p.extend(compact(0)); // package imports
        p.extend(compact(0)); // within
        p.extend(compact(system));
        p.extend(compact(0)); // hide categories
        if default_name != 0 {
            // IntProperty tag: name, info 0x22 (size code 2 = 4 bytes), value.
            p.extend(compact(default_name));
            p.push(0x22);
            p.extend(default_value.to_le_bytes());
        }
        p.extend(compact(0)); // defaults terminator
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
            (core, class, -1, self.name("ArrayProperty")),
            (core, class, -1, self.name("ByteProperty")),
            (core, class, -1, self.name("Struct")),
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
use crate::vm::VmResult;

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

// ---------------------------------------------------------------------------------------
// World physics bridge: Move/SetLocation/Trace/FastTrace/SetCollision(Size) + touching

use crate::physics::{MoveOutcome, WorldHit, WorldPhysics};

const IMP_ARRAYPROP: i32 = -12;

/// `Object`/`Actor`/`Child`/`LevelInfo` fixture with collision fields, a dynamic `Touching`
/// array and `Touch`/`UnTouch` counters.
fn phys_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let child = b.reserve(0, 0, "Child");
    let levelinfo = b.reserve(0, 0, "LevelInfo");

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

    let object_extra = compact(0);
    let vector_extra = compact(IMP_STRUCT);
    let rotator_extra = compact(IMP_STRUCT - 1);

    // Touch/UnTouch handlers first (their refs go into the property chain).
    let touch_fn = b.reserve(IMP_FUNCTION, actor, "Touch");
    let touch_other = b.reserve(IMP_OBJECTPROP, touch_fn, "Other");
    let untouch_fn = b.reserve(IMP_FUNCTION, actor, "UnTouch");
    let untouch_other = b.reserve(IMP_OBJECTPROP, untouch_fn, "Other");

    let owner = b.reserve(IMP_OBJECTPROP, actor, "Owner");
    let level = b.reserve(IMP_OBJECTPROP, actor, "Level");
    let base = b.reserve(IMP_OBJECTPROP, actor, "Base");
    let tag = b.reserve(IMP_NAMEPROP, actor, "Tag");
    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let rotation = b.reserve(IMP_STRUCTPROP, actor, "Rotation");
    let radius = b.reserve(IMP_FLOATPROP, actor, "CollisionRadius");
    let height = b.reserve(IMP_FLOATPROP, actor, "CollisionHeight");
    let collide_actors = b.reserve(IMP_BOOLPROP, actor, "bCollideActors");
    let collide_world = b.reserve(IMP_BOOLPROP, actor, "bCollideWorld");
    let collide_placing = b.reserve(IMP_BOOLPROP, actor, "bCollideWhenPlacing");
    let block_actors = b.reserve(IMP_BOOLPROP, actor, "bBlockActors");
    let block_players = b.reserve(IMP_BOOLPROP, actor, "bBlockPlayers");
    let block_zero = b.reserve(IMP_BOOLPROP, actor, "bBlockZeroExtentTraces");
    let block_nonzero = b.reserve(IMP_BOOLPROP, actor, "bBlockNonZeroExtentTraces");
    let movable = b.reserve(IMP_BOOLPROP, actor, "bMovable");
    let bstatic = b.reserve(IMP_BOOLPROP, actor, "bStatic");
    let touches = b.reserve(IMP_INTPROP, actor, "Touches");
    let untouches = b.reserve(IMP_INTPROP, actor, "UnTouches");
    let touching = b.reserve(IMP_ARRAYPROP, actor, "Touching");
    let touching_template = b.reserve(IMP_OBJECTPROP, touching, "Touching");

    b.prop_with(owner, level, 0, &object_extra);
    b.prop_with(level, base, 0, &object_extra);
    b.prop_with(base, tag, 0, &object_extra);
    b.prop(tag, location, 0);
    b.prop_with(location, rotation, 0, &vector_extra);
    b.prop_with(rotation, radius, 0, &rotator_extra);
    b.prop(radius, height, 0);
    b.prop(height, collide_actors, 0);
    b.prop(collide_actors, collide_world, 0);
    b.prop(collide_world, collide_placing, 0);
    b.prop(collide_placing, block_actors, 0);
    b.prop(block_actors, block_players, 0);
    b.prop(block_players, block_zero, 0);
    b.prop(block_zero, block_nonzero, 0);
    b.prop(block_nonzero, movable, 0);
    b.prop(movable, bstatic, 0);
    b.prop(bstatic, touches, 0);
    b.prop(touches, untouches, 0);
    b.prop(untouches, touching, 0);
    b.prop_with(touching_template, 0, 0, &object_extra);
    b.prop_array(touching, touch_fn, 0, touching_template);

    let tc = touches as u8;
    let touch_code = vec![0x0F, 0x01, tc, 0x92, 0x00, tc, 0x26, 0x16, 0x04, 0x0B];
    b.prop_with(touch_other, 0, PARM, &object_extra);
    b.func(
        touch_fn,
        untouch_fn,
        touch_other,
        &touch_code,
        0x10,
        0,
        DEFINED,
    );
    let uc = untouches as u8;
    let untouch_code = vec![0x0F, 0x01, uc, 0x92, 0x00, uc, 0x26, 0x16, 0x04, 0x0B];
    b.prop_with(untouch_other, 0, PARM, &object_extra);
    b.func(
        untouch_fn,
        0,
        untouch_other,
        &untouch_code,
        0x10,
        0,
        DEFINED,
    );

    b.class(object, 0, add, 0);
    b.class(actor, object, owner, 0);
    b.class(child, actor, 0, 0);
    b.class(levelinfo, actor, 0, 0);
    b.build()
}

fn phys_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        phys_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

fn pg(set: &ScriptSet, path: &str) -> GlobalRef {
    GlobalRef {
        package: 0,
        export: set.packages[0].export_by_path(path).expect(path),
    }
}

// ---------------------------------------------------------------------------------------
// Navigation / Controller pathing

use crate::navigation::{NavEdgeInfo, NavPointInfo, NavigationData, reach_flags};

/// `Object`/`Actor`/`Pawn`/`Controller` fixture for the pathing natives. `Actor` carries
/// `Location`/`Rotation`/`CollisionRadius`/`CollisionHeight`; `Pawn` adds `GroundSpeed` and
/// `BaseEyeHeight`; `Controller` adds `Pawn`, `Destination`, `FocalPoint`, `MoveTarget`,
/// `RouteDist` and a `RouteCache[16]` object array (the real engine.u `ArrayDim` is 16).
fn nav_fixture() -> Vec<u8> {
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let controller = b.reserve(0, 0, "Controller");
    let pawn = b.reserve(0, 0, "Pawn");

    let object_extra = compact(0);
    let vector_extra = compact(IMP_STRUCT);
    let rotator_extra = compact(IMP_STRUCT - 1);

    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let rotation = b.reserve(IMP_STRUCTPROP, actor, "Rotation");
    let radius = b.reserve(IMP_FLOATPROP, actor, "CollisionRadius");
    let height = b.reserve(IMP_FLOATPROP, actor, "CollisionHeight");
    let ground = b.reserve(IMP_FLOATPROP, pawn, "GroundSpeed");
    let eye = b.reserve(IMP_FLOATPROP, pawn, "BaseEyeHeight");

    let c_pawn = b.reserve(IMP_OBJECTPROP, controller, "Pawn");
    let dest = b.reserve(IMP_STRUCTPROP, controller, "Destination");
    let focal = b.reserve(IMP_STRUCTPROP, controller, "FocalPoint");
    let move_target = b.reserve(IMP_OBJECTPROP, controller, "MoveTarget");
    let route_dist = b.reserve(IMP_FLOATPROP, controller, "RouteDist");
    let route_cache = b.reserve(IMP_ARRAYPROP, controller, "RouteCache");
    let route_template = b.reserve(IMP_OBJECTPROP, route_cache, "RouteCache");

    b.prop_with(location, rotation, 0, &vector_extra);
    b.prop_with(rotation, radius, 0, &rotator_extra);
    b.prop(radius, height, 0);
    b.prop_with(c_pawn, dest, 0, &object_extra);
    b.prop_with(dest, focal, 0, &vector_extra);
    b.prop_with(focal, move_target, 0, &vector_extra);
    b.prop_with(move_target, route_dist, 0, &object_extra);
    b.prop(route_dist, route_cache, 0);
    b.prop_array_dim(route_cache, 0, 0, route_template, 16);
    b.prop_with(route_template, 0, 0, &object_extra);
    b.prop(ground, eye, 0);

    b.class(object, 0, 0, 0);
    b.class(actor, object, location, 0);
    b.class(controller, actor, c_pawn, 0);
    b.class(pawn, actor, ground, 0);
    b.build()
}

fn nav_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        nav_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

/// Fixture for the XIII AI natives (`item3g`): `Object/Actor/Pawn/Controller/IAController`, a
/// `PatrolPoint` class, and an `AllianceEntry` struct (`AllianceName` name, `AllianceLevel` float)
/// backing `Pawn.InitialAlliances[4]`.
fn ai_fixture() -> Vec<u8> {
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let pawn = b.reserve(0, 0, "Pawn");
    let controller = b.reserve(0, 0, "Controller");
    let ia_controller = b.reserve(0, 0, "IAController");
    let patrol = b.reserve(0, 0, "PatrolPoint");

    let object_extra = compact(0);
    let vector_extra = compact(IMP_STRUCT);
    let rotator_extra = compact(IMP_STRUCT - 1);

    let alliance_entry = b.reserve(IMP_STRUCT_CLASS, 0, "AllianceEntry");
    let entry_name = b.reserve(IMP_NAMEPROP, alliance_entry, "AllianceName");
    let entry_level = b.reserve(IMP_FLOATPROP, alliance_entry, "AllianceLevel");
    b.prop(entry_name, entry_level, 0);
    b.prop(entry_level, 0, 0);
    b.strukt(alliance_entry, entry_name);

    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let rotation = b.reserve(IMP_STRUCTPROP, actor, "Rotation");
    let radius = b.reserve(IMP_FLOATPROP, actor, "CollisionRadius");
    let height = b.reserve(IMP_FLOATPROP, actor, "CollisionHeight");

    let ground = b.reserve(IMP_FLOATPROP, pawn, "GroundSpeed");
    let eye = b.reserve(IMP_FLOATPROP, pawn, "BaseEyeHeight");
    let alliance = b.reserve(IMP_NAMEPROP, pawn, "Alliance");
    let initial_alliances = b.reserve(IMP_STRUCTPROP, pawn, "InitialAlliances");

    let c_pawn = b.reserve(IMP_OBJECTPROP, controller, "Pawn");
    let c_xiii = b.reserve(IMP_OBJECTPROP, controller, "XIII");
    let c_base = b.reserve(IMP_OBJECTPROP, controller, "BaseS");
    let c_start = b.reserve(IMP_OBJECTPROP, controller, "StartSpot");
    let c_dest = b.reserve(IMP_STRUCTPROP, controller, "Destination");

    b.prop_with(location, rotation, 0, &vector_extra);
    b.prop_with(rotation, radius, 0, &rotator_extra);
    b.prop(radius, height, 0);
    b.prop(height, 0, 0);
    b.prop(ground, eye, 0);
    b.prop(eye, alliance, 0);
    b.prop(alliance, initial_alliances, 0);
    b.prop_array_dim(initial_alliances, 0, 0, alliance_entry, 4);
    b.prop_with(c_pawn, c_xiii, 0, &object_extra);
    b.prop_with(c_xiii, c_base, 0, &object_extra);
    b.prop_with(c_base, c_start, 0, &object_extra);
    b.prop_with(c_start, c_dest, 0, &object_extra);
    b.prop_with(c_dest, 0, 0, &vector_extra);

    b.class(object, 0, 0, 0);
    b.class(actor, object, location, 0);
    b.class(pawn, actor, ground, 0);
    b.class(controller, actor, c_pawn, 0);
    b.class(ia_controller, controller, 0, 0);
    b.class(patrol, actor, 0, 0);
    b.build()
}

fn ai_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        ai_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

/// Test navigation graph: Nav0 -500-> Nav1 -500-> Nav2, in Unreal space.
struct MockNav {
    points: Vec<NavPointInfo>,
    edges: Vec<NavEdgeInfo>,
}

impl MockNav {
    fn line() -> Self {
        let p = |actor: &str, x: f32| NavPointInfo {
            actor: actor.into(),
            location: [x, 0.0, 0.0],
            collision_radius: 120.0,
            collision_height: 120.0,
        };
        Self {
            points: vec![p("Nav0", 0.0), p("Nav1", 500.0), p("Nav2", 1000.0)],
            edges: vec![
                NavEdgeInfo {
                    start: 0,
                    end: 1,
                    collision_radius: 120,
                    collision_height: 120,
                    reach_flags: reach_flags::WALK,
                    distance: 500,
                },
                NavEdgeInfo {
                    start: 1,
                    end: 2,
                    collision_radius: 120,
                    collision_height: 120,
                    reach_flags: reach_flags::WALK,
                    distance: 500,
                },
            ],
        }
    }

    /// A graph with the given points and no edges.
    fn with_points(points: Vec<NavPointInfo>) -> Self {
        Self {
            points,
            edges: Vec::new(),
        }
    }
}

impl NavigationData for MockNav {
    fn points(&self) -> &[NavPointInfo] {
        &self.points
    }
    fn edges(&self) -> &[NavEdgeInfo] {
        &self.edges
    }
}

/// Test provider: static axis-aligned walls (min, max) in Unreal coordinates.
struct MockWorld {
    walls: Vec<([f32; 3], [f32; 3])>,
    blocked_point: bool,
}

impl MockWorld {
    fn new() -> Self {
        Self {
            walls: Vec::new(),
            blocked_point: false,
        }
    }

    fn with_wall(mut self, min: [f32; 3], max: [f32; 3]) -> Self {
        self.walls.push((min, max));
        self
    }
}

impl WorldPhysics for MockWorld {
    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        let d = [end[0] - start[0], end[1] - start[1], end[2] - start[2]];
        let mut best: Option<(f32, [f32; 3])> = None;
        for &(mn, mx) in &self.walls {
            if let Some((t, n)) = swept_aabb(start, d, extent, mn, mx)
                && best.is_none_or(|(bt, _)| t < bt)
            {
                best = Some((t, n));
            }
        }
        best.map(|(t, n)| WorldHit {
            location: [
                start[0] + d[0] * t,
                start[1] + d[1] * t,
                start[2] + d[2] * t,
            ],
            normal: n,
            time: t,
        })
    }

    fn move_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        let end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        match self.trace(start, end, extent) {
            Some(hit) => MoveOutcome {
                end: [
                    start[0] + delta[0] * hit.time,
                    start[1] + delta[1] * hit.time,
                    start[2] + delta[2] * hit.time,
                ],
                hit: Some(hit),
            },
            None => MoveOutcome { end, hit: None },
        }
    }

    fn point_free(&mut self, location: [f32; 3], extent: [f32; 3]) -> bool {
        if self.blocked_point {
            return false;
        }
        !self
            .walls
            .iter()
            .any(|&(mn, mx)| aabb_overlaps(location, extent, mn, mx))
    }
}

fn aabb_overlaps(c: [f32; 3], e: [f32; 3], mn: [f32; 3], mx: [f32; 3]) -> bool {
    (0..3).all(|i| c[i] + e[i] > mn[i] && c[i] - e[i] < mx[i])
}

/// Swept AABB vs static AABB; returns `(fraction, normal)`.
fn swept_aabb(
    start: [f32; 3],
    d: [f32; 3],
    e: [f32; 3],
    mn: [f32; 3],
    mx: [f32; 3],
) -> Option<(f32, [f32; 3])> {
    let mut t_enter = 0.0f32;
    let mut t_exit = 1.0f32;
    let mut axis = 0usize;
    for i in 0..3 {
        let smin = start[i] - e[i];
        let smax = start[i] + e[i];
        if d[i].abs() < 1e-9 {
            if smax <= mn[i] || smin >= mx[i] {
                return None;
            }
        } else {
            let mut t1 = (mn[i] - smax) / d[i];
            let mut t2 = (mx[i] - smin) / d[i];
            if t1 > t2 {
                std::mem::swap(&mut t1, &mut t2);
            }
            if t1 > t_enter {
                t_enter = t1;
                axis = i;
            }
            if t2 < t_exit {
                t_exit = t2;
            }
            if t_enter > t_exit {
                return None;
            }
        }
    }
    if t_enter > 1.0 {
        return None;
    }
    let sign = if d[axis] > 0.0 {
        -1.0
    } else if d[axis] < 0.0 {
        1.0
    } else {
        0.0
    };
    let mut n = [0.0; 3];
    n[axis] = sign;
    Some((t_enter.max(0.0), n))
}

fn try_native(
    vm: &mut Vm<'_>,
    path: &str,
    this: ObjectId,
    omitted: &[bool],
    args: &mut [Value],
) -> VmResult<NativeOutcome> {
    let def = native(path);
    (def.f)(vm, &ctx(this, omitted, path), args)
}

fn phys_actor(vm: &mut Vm<'_>, set: &ScriptSet, name: &str, loc: [f32; 3]) -> ObjectId {
    let id = vm.spawn(pg(set, "Actor"), name).unwrap();
    vm.set_property(id, "Location", 0, Value::Vector(loc));
    id
}

fn set_collision_fields(vm: &mut Vm<'_>, id: ObjectId, colliding: bool, blocking: bool) {
    vm.set_property(id, "bCollideActors", 0, Value::Bool(colliding));
    vm.set_property(id, "bCollideWorld", 0, Value::Bool(true));
    vm.set_property(id, "bBlockActors", 0, Value::Bool(blocking));
    vm.set_property(id, "bBlockPlayers", 0, Value::Bool(blocking));
    vm.set_property(id, "bBlockNonZeroExtentTraces", 0, Value::Bool(blocking));
    vm.set_property(id, "bBlockZeroExtentTraces", 0, Value::Bool(blocking));
    vm.set_property(id, "bMovable", 0, Value::Bool(true));
    vm.set_property(id, "CollisionRadius", 0, Value::Float(10.0));
    vm.set_property(id, "CollisionHeight", 0, Value::Float(10.0));
}

#[test]
fn move_into_wall_stops_at_hit_and_bcollideworld_false_ignores_it() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([100.0, -1000.0, -1000.0], [200.0, 1000.0, 1000.0]),
    ));
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, false, false);

    let mut args = [Value::Vector([100.0, 0.0, 0.0])];
    let moved = match try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap() {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(!moved, "blocked by the wall");
    assert_eq!(
        vm.get_property(a, "Location"),
        Some(&Value::Vector([90.0, 0.0, 0.0]))
    );

    // bCollideWorld = false: the wall is ignored.
    vm.set_property(a, "bCollideWorld", 0, Value::Bool(false));
    let mut args = [Value::Vector([100.0, 0.0, 0.0])];
    let moved = match try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap() {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(moved);
    assert_eq!(
        vm.get_property(a, "Location"),
        Some(&Value::Vector([190.0, 0.0, 0.0]))
    );
}

#[test]
fn move_without_provider_fails_and_survey_counts_it() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, false, false);
    for path in [
        "Engine.Actor.Move",
        "Engine.Actor.SetLocation",
        "Engine.Actor.Trace",
        "Engine.Actor.FastTrace",
    ] {
        let (mut args, omitted): (Vec<Value>, Vec<bool>) = match path {
            "Engine.Actor.Trace" => (
                vec![Value::Vector([0.0; 3]); 9],
                vec![false, false, false, true, true, true, true, true, true],
            ),
            "Engine.Actor.FastTrace" => (
                vec![Value::Vector([1.0, 0.0, 0.0]), Value::Vector([0.0; 3])],
                vec![false, true, true, true],
            ),
            _ => (vec![Value::Vector([1.0, 0.0, 0.0])], vec![false]),
        };
        let e = try_native(&mut vm, path, a, &omitted, &mut args).unwrap_err();
        assert!(
            matches!(&e.kind, VmErrorKind::NoPhysicsProvider { native } if native == path.trim_start_matches("Engine.")),
            "{path}: {e}"
        );
    }
    // Survey mode: counted like a missing native and the run continues.
    vm.survey = true;
    let mut args = [Value::Vector([1.0, 0.0, 0.0])];
    let r = try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Bool(false)));
    let m = vm.missing_natives.get("Actor.Move").expect("counted");
    assert_eq!(m.index, Some(266));
    assert_eq!(m.calls, 1);
}

#[test]
fn move_into_cylinder_touches_both_sides_and_leaving_untouches() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    let b = phys_actor(&mut vm, &set, "B", [50.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, true, false);
    set_collision_fields(&mut vm, b, true, false);

    let mut args = [Value::Vector([50.0, 0.0, 0.0])];
    try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap();
    assert_eq!(vm.get_property(a, "Touches"), Some(&Value::Int(1)));
    assert_eq!(
        vm.get_property(b, "Touches"),
        Some(&Value::Int(1)),
        "both sides"
    );
    assert_eq!(vm.touching_list(a), vec![b]);
    assert_eq!(vm.touching_list(b), vec![a]);

    // Moving while still overlapping must not touch again.
    let mut args = [Value::Vector([1.0, 0.0, 0.0])];
    try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap();
    assert_eq!(vm.get_property(a, "Touches"), Some(&Value::Int(1)));
    assert_eq!(vm.get_property(b, "Touches"), Some(&Value::Int(1)));

    // Leaving the cylinder sends UnTouch to both and clears both arrays.
    let mut args = [Value::Vector([100.0, 0.0, 0.0])];
    try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap();
    assert_eq!(vm.get_property(a, "UnTouches"), Some(&Value::Int(1)));
    assert_eq!(vm.get_property(b, "UnTouches"), Some(&Value::Int(1)));
    assert!(vm.touching_list(a).is_empty());
    assert!(vm.touching_list(b).is_empty());
}

#[test]
fn host_written_location_drives_touch_refresh() {
    // Ownership contract for the Bevy `--play` host: the movement simulation owns the player
    // pawn's Location and writes it into the VM before the VM tick; the VM delivers Touch from
    // that host-written Location (no script Move), so walking into a trigger volume fires it.
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let player = phys_actor(&mut vm, &set, "Player", [0.0, 0.0, 0.0]);
    let trigger = phys_actor(&mut vm, &set, "Trigger", [500.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, player, true, false);
    set_collision_fields(&mut vm, trigger, true, false);

    // Apart, then the host writes the new Location and refreshes (one fixed tick).
    vm.refresh_touching_of(player).unwrap();
    assert!(vm.touching_list(player).is_empty());
    vm.set_property(player, "Location", 0, Value::Vector([500.0, 0.0, 0.0]));
    vm.refresh_touching_of(player).unwrap();
    assert_eq!(vm.get_property(player, "Touches"), Some(&Value::Int(1)));
    assert_eq!(vm.get_property(trigger, "Touches"), Some(&Value::Int(1)));
    assert_eq!(vm.touching_list(player), vec![trigger]);

    // The host writes the Location away and refreshes: UnTouch on both sides.
    vm.set_property(player, "Location", 0, Value::Vector([-500.0, 0.0, 0.0]));
    vm.refresh_touching_of(player).unwrap();
    assert_eq!(vm.get_property(trigger, "UnTouches"), Some(&Value::Int(1)));
    assert!(vm.touching_list(player).is_empty());
}

#[test]
fn exact_contact_boundary_overlaps() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    let b = phys_actor(&mut vm, &set, "B", [20.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, true, false);
    set_collision_fields(&mut vm, b, true, false);
    // Zero delta still recomputes touching; distance == r1+r2 counts as overlap in XIII's
    // decoded comparison (`VSize <= r1+r2`).
    let mut args = [Value::Vector([0.0, 0.0, 0.0])];
    try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap();
    assert_eq!(vm.get_property(a, "Touches"), Some(&Value::Int(1)));
    assert_eq!(vm.get_property(b, "Touches"), Some(&Value::Int(1)));
}

#[test]
fn set_collision_false_ends_touching() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    let b = phys_actor(&mut vm, &set, "B", [5.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, true, false);
    set_collision_fields(&mut vm, b, true, false);
    let mut args = [Value::Vector([0.0, 0.0, 0.0])];
    try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap();
    assert_eq!(vm.touching_list(a), vec![b]);

    let mut args = [Value::Bool(false), Value::Bool(false), Value::Bool(false)];
    try_native(
        &mut vm,
        "Engine.Actor.SetCollision",
        a,
        &[false, false, false],
        &mut args,
    )
    .unwrap();
    assert_eq!(vm.get_property(a, "UnTouches"), Some(&Value::Int(1)));
    assert_eq!(vm.get_property(b, "UnTouches"), Some(&Value::Int(1)));
    assert!(vm.touching_list(a).is_empty());
}

#[test]
fn set_collision_omitted_arguments_keep_current_values() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, true, true);
    let mut args = [Value::Bool(false), Value::Bool(false), Value::Bool(false)];
    try_native(
        &mut vm,
        "Engine.Actor.SetCollision",
        a,
        &[true, true, true],
        &mut args,
    )
    .unwrap();
    assert_eq!(
        vm.get_property(a, "bCollideActors"),
        Some(&Value::Bool(true))
    );
    assert_eq!(vm.get_property(a, "bBlockActors"), Some(&Value::Bool(true)));

    let mut args = [Value::Bool(false), Value::Bool(false), Value::Bool(false)];
    try_native(
        &mut vm,
        "Engine.Actor.SetCollision",
        a,
        &[false, true, true],
        &mut args,
    )
    .unwrap();
    assert_eq!(
        vm.get_property(a, "bCollideActors"),
        Some(&Value::Bool(false))
    );
    assert_eq!(vm.get_property(a, "bBlockActors"), Some(&Value::Bool(true)));
}

#[test]
fn blocking_actor_stops_the_move_and_is_not_touched() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    let b = phys_actor(&mut vm, &set, "B", [50.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, true, true);
    set_collision_fields(&mut vm, b, true, true);

    let mut args = [Value::Vector([50.0, 0.0, 0.0])];
    let moved = match try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap() {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(!moved);
    // Stops exactly at contact (distance == r1 + r2), never past it.
    match vm.get_property(a, "Location") {
        Some(Value::Vector(v)) => assert!((v[0] - 30.0).abs() < 1e-3, "{v:?}"),
        other => panic!("{other:?}"),
    }
    // An actor cannot touch what blocks it (upstream TryMove).
    assert_eq!(vm.get_property(a, "Touches"), Some(&Value::Int(0)));
    assert_eq!(vm.get_property(b, "Touches"), Some(&Value::Int(0)));
}

#[test]
fn trace_hits_nearer_of_world_and_actor_and_fasttrace_ignores_actors() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([100.0, -100.0, -100.0], [200.0, 100.0, 100.0]),
    ));
    let _li = vm.spawn(pg(&set, "LevelInfo"), "LevelInfo0").unwrap();
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, false);
    let b = phys_actor(&mut vm, &set, "B", [40.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, b, true, false);

    // Actor closer than the wall -> the actor is returned.
    let mut args = vec![
        Value::Vector([0.0; 3]),
        Value::Vector([0.0; 3]),
        Value::Vector([200.0, 0.0, 0.0]),
        Value::Vector([0.0; 3]),
        Value::Bool(true),
        Value::Vector([0.0; 3]),
        Value::Object(None),
        Value::Int(0),
        Value::Int(0),
    ];
    let out = try_native(
        &mut vm,
        "Engine.Actor.Trace",
        tracer,
        &[false, false, false, false, false, true, true, true, true],
        &mut args,
    )
    .unwrap();
    match out {
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(i)))) => assert_eq!(i, b),
        other => panic!("{other:?}"),
    }
    match &args[0] {
        Value::Vector(v) => assert!((v[0] - 30.0).abs() < 0.01, "{v:?}"),
        other => panic!("{other:?}"),
    }

    // Actor beyond the wall -> the world hit returns the map LevelInfo.
    vm.set_property(b, "Location", 0, Value::Vector([150.0, 0.0, 0.0]));
    let mut args = vec![
        Value::Vector([0.0; 3]),
        Value::Vector([0.0; 3]),
        Value::Vector([200.0, 0.0, 0.0]),
        Value::Vector([0.0; 3]),
        Value::Bool(true),
        Value::Vector([0.0; 3]),
        Value::Object(None),
        Value::Int(0),
        Value::Int(0),
    ];
    let out = try_native(
        &mut vm,
        "Engine.Actor.Trace",
        tracer,
        &[false, false, false, false, false, true, true, true, true],
        &mut args,
    )
    .unwrap();
    let li = vm.find_level_info().unwrap();
    match out {
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(i)))) => assert_eq!(i, li),
        other => panic!("{other:?}"),
    }

    // FastTrace is world-only: the actor between start and end does not block it.
    vm.set_property(b, "Location", 0, Value::Vector([40.0, 0.0, 0.0]));
    let mut args = [Value::Vector([60.0, 0.0, 0.0]), Value::Vector([0.0; 3])];
    let clear = match try_native(
        &mut vm,
        "Engine.Actor.FastTrace",
        tracer,
        &[false, false, true, true],
        &mut args,
    )
    .unwrap()
    {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(clear, "world-only FastTrace ignores the actor");
    // ... but the wall blocks it.
    let mut args = [Value::Vector([200.0, 0.0, 0.0]), Value::Vector([0.0; 3])];
    let clear = match try_native(
        &mut vm,
        "Engine.Actor.FastTrace",
        tracer,
        &[false, false, true, true],
        &mut args,
    )
    .unwrap()
    {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(!clear);
}

#[test]
fn set_location_refuses_encroachment_and_moves_when_free() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([40.0, -5.0, -5.0], [60.0, 5.0, 5.0]),
    ));
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    let b = phys_actor(&mut vm, &set, "B", [500.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, true, false);
    set_collision_fields(&mut vm, b, true, false);
    // Both sides block: B blocks the destination.
    vm.set_property(a, "bBlockActors", 0, Value::Bool(true));
    vm.set_property(b, "bBlockActors", 0, Value::Bool(true));
    vm.set_property(b, "bBlockNonZeroExtentTraces", 0, Value::Bool(true));
    let mut args = [Value::Vector([500.0, 5.0, 0.0])];
    let ok = match try_native(&mut vm, "Engine.Actor.SetLocation", a, &[false], &mut args).unwrap()
    {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(!ok, "encroached by a blocking actor");
    assert_eq!(
        vm.get_property(a, "Location"),
        Some(&Value::Vector([0.0, 0.0, 0.0]))
    );

    // World-blocked destination is refused.
    let mut args = [Value::Vector([50.0, 0.0, 0.0])];
    let ok = match try_native(&mut vm, "Engine.Actor.SetLocation", a, &[false], &mut args).unwrap()
    {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(!ok, "destination inside the wall");
    assert_eq!(
        vm.get_property(a, "Location"),
        Some(&Value::Vector([0.0, 0.0, 0.0]))
    );

    // A free destination moves and touches an actor already there.
    let c = phys_actor(&mut vm, &set, "C", [0.0, 100.0, 0.0]);
    set_collision_fields(&mut vm, c, true, false);
    let mut args = [Value::Vector([0.0, 100.0, 0.0])];
    let ok = match try_native(&mut vm, "Engine.Actor.SetLocation", a, &[false], &mut args).unwrap()
    {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(ok);
    assert_eq!(
        vm.get_property(a, "Location"),
        Some(&Value::Vector([0.0, 100.0, 0.0]))
    );
    assert_eq!(vm.get_property(a, "Touches"), Some(&Value::Int(1)));
    assert_eq!(vm.get_property(c, "Touches"), Some(&Value::Int(1)));
}

#[test]
fn touching_actors_iterator_filters_by_base_class() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let a = phys_actor(&mut vm, &set, "A", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, a, true, false);
    let b = vm.spawn(pg(&set, "Child"), "B").unwrap();
    vm.set_property(b, "Location", 0, Value::Vector([5.0, 0.0, 0.0]));
    set_collision_fields(&mut vm, b, true, false);
    let c = phys_actor(&mut vm, &set, "C", [0.0, 5.0, 0.0]);
    set_collision_fields(&mut vm, c, true, false);
    let mut args = [Value::Vector([0.0, 0.0, 0.0])];
    try_native(&mut vm, "Engine.Actor.Move", a, &[false], &mut args).unwrap();
    assert_eq!(vm.touching_list(a).len(), 2);

    let base = Value::Object(Some(ObjRef::Static(pg(&set, "Child"))));
    let mut args = [base.clone(), Value::Object(None)];
    let items = match try_native(
        &mut vm,
        "Engine.Actor.TouchingActors",
        a,
        &[false, false],
        &mut args,
    )
    .unwrap()
    {
        NativeOutcome::Iterate(items) => items,
        other => panic!("{other:?}"),
    };
    assert_eq!(items.len(), 1);
    assert_eq!(items[0], Value::Object(Some(ObjRef::Instance(b))));
}

// ---------------------------------------------------------------------------------------
// `New` opcode: non-actor construction with class defaults

/// `Object` with an int `Value` defaulting to 7 and two functions that `new` an `Object`
/// or an `Actor`.
fn new_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let make_object = b.reserve(IMP_FUNCTION, object, "MakeObject");
    let make_actor = b.reserve(IMP_FUNCTION, object, "MakeActor");
    let value = b.reserve(IMP_INTPROP, object, "Value");
    let value_name = b.name("Value");
    let object_extra = compact(0);
    let mo_ret = b.reserve(IMP_OBJECTPROP, make_object, "ReturnValue");
    let ma_ret = b.reserve(IMP_OBJECTPROP, make_actor, "ReturnValue");

    // Child chain: Value -> MakeObject -> MakeActor; the returns are children of the functions.
    b.prop_with(mo_ret, 0, PARM | RETURN_PARM, &object_extra);
    b.prop_with(ma_ret, 0, PARM | RETURN_PARM, &object_extra);
    b.prop(value, make_object, 0);

    // `return new (None, None, None) Object;` — ObjectConst (0x20) + export ref.
    let co = object as u8;
    let ca = actor as u8;
    let make_object_code = [0x04, 0x11, 0x0B, 0x0B, 0x0B, 0x20, co];
    let make_actor_code = [0x04, 0x11, 0x0B, 0x0B, 0x0B, 0x20, ca];
    b.func(
        make_object,
        make_actor,
        mo_ret,
        &make_object_code,
        10,
        0,
        DEFINED,
    );
    b.func(make_actor, 0, ma_ret, &make_actor_code, 10, 0, DEFINED);
    b.class_with_int_default(object, 0, value, 0, value_name, 7);
    b.class(actor, object, 0, 0);
    b.build()
}

fn new_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        new_fixture(),
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
fn new_object_applies_defaults_and_makes_distinct_instances() {
    let set = new_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let host = vm.spawn(sg(&set, "Object"), "Host").unwrap();
    vm.set_active(host, true);
    let id1 = match vm
        .call_function(sg(&set, "Object.MakeObject"), host, vec![])
        .unwrap()
    {
        Value::Object(Some(ObjRef::Instance(i))) => i,
        other => panic!("expected an instance, got {other:?}"),
    };
    // Class defaults (Value = 7) are applied.
    assert_eq!(vm.get_property(id1, "Value"), Some(&Value::Int(7)));
    assert!(vm.is_a(id1, "Object") && !vm.is_a(id1, "Actor"));
    let id2 = match vm
        .call_function(sg(&set, "Object.MakeObject"), host, vec![])
        .unwrap()
    {
        Value::Object(Some(ObjRef::Instance(i))) => i,
        other => panic!("expected an instance, got {other:?}"),
    };
    assert_ne!(id1, id2, "each `new` is a fresh object");
    // No lifecycle events: a non-actor gets no BeginPlay/PostBeginPlay.
    assert!(
        !vm.trace
            .iter()
            .any(|e| matches!(&e.kind, TraceKind::Event { target, .. } if target == "Object"))
    );
}

#[test]
fn new_on_actor_class_fails_explicitly() {
    let set = new_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let host = vm.spawn(sg(&set, "Object"), "Host").unwrap();
    vm.set_active(host, true);
    let e = vm
        .call_function(sg(&set, "Object.MakeActor"), host, vec![])
        .unwrap_err();
    assert!(
        matches!(&e.kind, VmErrorKind::NewOnActor { class } if class.ends_with("Actor")),
        "{e}"
    );
}

// ---------------------------------------------------------------------------------------
// Animation natives, FinishAnim latency and SetViewTarget

/// `Object`/`Actor`/`PlayerController` fixture with the animation channels, a latent
/// `FinishAnim` state and `SetViewTarget`.
fn anim_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let pc = b.reserve(0, 0, "PlayerController");
    let object_extra = compact(0);

    let anim_sequence = b.reserve(IMP_NAMEPROP, actor, "AnimSequence");
    let anim_rate = b.reserve(IMP_FLOATPROP, actor, "AnimRate");
    let anim_frame = b.reserve(IMP_FLOATPROP, actor, "AnimFrame");
    let banim_finished = b.reserve(IMP_BOOLPROP, actor, "bAnimFinished");
    let mesh = b.reserve(IMP_OBJECTPROP, actor, "Mesh");
    let counter = b.reserve(IMP_INTPROP, actor, "Counter");

    let link = b.reserve(IMP_FUNCTION, actor, "LinkSkelAnim");
    let link_anim = b.reserve(IMP_OBJECTPROP, link, "Anim");
    let play = b.reserve(IMP_FUNCTION, actor, "PlayAnim");
    let play_seq = b.reserve(IMP_NAMEPROP, play, "Sequence");
    let play_rate = b.reserve(IMP_FLOATPROP, play, "Rate");
    let play_tween = b.reserve(IMP_FLOATPROP, play, "TweenTime");
    let play_ch = b.reserve(IMP_INTPROP, play, "Channel");
    let loopf = b.reserve(IMP_FUNCTION, actor, "LoopAnim");
    let loop_seq = b.reserve(IMP_NAMEPROP, loopf, "Sequence");
    let loop_rate = b.reserve(IMP_FLOATPROP, loopf, "Rate");
    let loop_tween = b.reserve(IMP_FLOATPROP, loopf, "TweenTime");
    let loop_ch = b.reserve(IMP_INTPROP, loopf, "Channel");
    let tween = b.reserve(IMP_FUNCTION, actor, "TweenAnim");
    let tween_seq = b.reserve(IMP_NAMEPROP, tween, "Sequence");
    let tween_time = b.reserve(IMP_FLOATPROP, tween, "Time");
    let tween_ch = b.reserve(IMP_INTPROP, tween, "Channel");
    let isanim = b.reserve(IMP_FUNCTION, actor, "IsAnimating");
    let ia_ch = b.reserve(IMP_INTPROP, isanim, "Channel");
    let ia_ret = b.reserve(IMP_BOOLPROP, isanim, "ReturnValue");
    let finish = b.reserve(IMP_FUNCTION, actor, "FinishAnim");
    let fin_ch = b.reserve(IMP_INTPROP, finish, "Channel");
    let has = b.reserve(IMP_FUNCTION, actor, "HasAnim");
    let has_seq = b.reserve(IMP_NAMEPROP, has, "Sequence");
    let has_ret = b.reserve(IMP_BOOLPROP, has, "ReturnValue");
    let animating = b.reserve(IMP_STATE, actor, "Animating");

    let view_target = b.reserve(IMP_OBJECTPROP, pc, "ViewTarget");
    let svt = b.reserve(IMP_FUNCTION, pc, "SetViewTarget");
    let svt_param = b.reserve(IMP_OBJECTPROP, svt, "NewViewTarget");

    b.prop(anim_sequence, anim_rate, 0);
    b.prop(anim_rate, anim_frame, 0);
    b.prop(anim_frame, banim_finished, 0);
    b.prop(banim_finished, mesh, 0);
    b.prop_with(mesh, counter, 0, &object_extra);
    b.prop(counter, link, 0);

    b.prop_with(link_anim, 0, PARM, &object_extra);
    b.func(link, play, link_anim, &[], 0, 413, FINAL | NATIVE | STATIC);

    b.prop(play_seq, play_rate, PARM);
    b.prop(play_rate, play_tween, PARM);
    b.prop(play_tween, play_ch, PARM);
    b.prop(play_ch, 0, PARM);
    b.func(play, loopf, play_seq, &[], 0, 259, FINAL | NATIVE | STATIC);

    b.prop(loop_seq, loop_rate, PARM);
    b.prop(loop_rate, loop_tween, PARM);
    b.prop(loop_tween, loop_ch, PARM);
    b.prop(loop_ch, 0, PARM);
    b.func(loopf, tween, loop_seq, &[], 0, 260, FINAL | NATIVE | STATIC);

    b.prop(tween_seq, tween_time, PARM);
    b.prop(tween_time, tween_ch, PARM);
    b.prop(tween_ch, 0, PARM);
    b.func(
        tween,
        isanim,
        tween_seq,
        &[],
        0,
        294,
        FINAL | NATIVE | STATIC,
    );

    b.prop(ia_ch, ia_ret, PARM);
    b.prop(ia_ret, 0, PARM | RETURN_PARM);
    b.func(isanim, finish, ia_ch, &[], 0, 282, FINAL | NATIVE | STATIC);

    b.prop(fin_ch, 0, PARM);
    b.func(
        finish,
        has,
        fin_ch,
        &[],
        0,
        261,
        FINAL | NATIVE | STATIC | LATENT,
    );

    b.prop(has_seq, has_ret, PARM);
    b.prop(has_ret, 0, PARM | RETURN_PARM);
    b.func(
        has,
        animating,
        has_seq,
        &[],
        0,
        263,
        FINAL | NATIVE | STATIC,
    );

    // State `Animating`: Counter=0; FinishAnim(0); Counter=1; stop.
    let begin = b.name("Begin") as u8;
    let rc = counter as u8;
    let mut anim_code = vec![0x0F, 0x01, rc, 0x25];
    anim_code.extend([0x61, 0x05, 0x25, 0x16]);
    anim_code.extend([0x0F, 0x01, rc, 0x26]);
    anim_code.push(0x08);
    anim_code.extend([0x0C, begin, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
    b.state(animating, 0, &anim_code, 0x24, 0x13);

    b.prop_with(view_target, svt, 0, &object_extra);
    b.prop_with(svt_param, 0, PARM, &object_extra);
    b.func(svt, 0, svt_param, &[], 0, 513, FINAL | NATIVE | STATIC);

    b.class(object, 0, 0, 0);
    b.class(actor, object, anim_sequence, 0);
    b.class(pc, actor, view_target, 0);
    b.build()
}

fn anim_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        anim_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

fn anim_end_count(vm: &Vm<'_>) -> usize {
    vm.trace
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::AnimEnd { .. }))
        .count()
}

fn play_anim(vm: &mut Vm<'_>, id: ObjectId, seq: &str, rate: f32, ch: i32) {
    let mut args = [
        Value::Name(seq.to_owned()),
        Value::Float(rate),
        Value::Float(0.0),
        Value::Int(ch),
    ];
    try_native(
        vm,
        "Engine.Actor.PlayAnim",
        id,
        &[false, false, false, false],
        &mut args,
    )
    .expect("PlayAnim");
}

#[test]
fn play_anim_fires_anim_end_once_at_the_right_tick() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(4, 1.0)));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    play_anim(&mut vm, a, "Walk", 1.0, 0);

    // 4 frames at 1 fps with a 0.5 s step: 8 ticks exactly.
    for _ in 0..7 {
        vm.tick(0.5).unwrap();
    }
    assert!(
        vm.anim_channel_active(a, 0),
        "still animating before the end"
    );
    assert_eq!(anim_end_count(&vm), 0);
    vm.tick(0.5).unwrap();
    assert!(!vm.anim_channel_active(a, 0));
    assert_eq!(anim_end_count(&vm), 1, "AnimEnd fires once at the end");
    assert_eq!(
        vm.get_property(a, "bAnimFinished"),
        Some(&Value::Bool(true))
    );
    assert_eq!(vm.get_property(a, "AnimFrame"), Some(&Value::Float(4.0)));
    // Further ticks do not fire it again.
    vm.tick(0.5).unwrap();
    assert_eq!(anim_end_count(&vm), 1);
}

#[test]
fn loop_anim_loops_without_anim_end_and_reports_is_animating() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(4, 1.0)));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    let mut args = [
        Value::Name("Walk".into()),
        Value::Float(1.0),
        Value::Float(0.0),
        Value::Int(0),
    ];
    try_native(
        &mut vm,
        "Engine.Actor.LoopAnim",
        a,
        &[false, false, false, false],
        &mut args,
    )
    .unwrap();
    for _ in 0..20 {
        vm.tick(0.5).unwrap();
    }
    assert_eq!(anim_end_count(&vm), 0, "LoopAnim never ends");
    assert!(vm.anim_channel_active(a, 0));

    let mut args = [Value::Int(0)];
    let r = try_native(&mut vm, "Engine.Actor.IsAnimating", a, &[false], &mut args).unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Bool(true)));
}

#[test]
fn finish_anim_resumes_state_code_on_anim_end() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(4, 1.0)));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    play_anim(&mut vm, a, "Walk", 1.0, 0);
    vm.goto_state(a, "Animating", None).unwrap();
    // First tick runs Counter=0 then suspends inside FinishAnim.
    vm.tick(0.5).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(0)));
    assert!(
        vm.trace
            .iter()
            .any(|e| matches!(&e.kind, TraceKind::AnimSuspend { .. }))
    );
    for _ in 0..7 {
        vm.tick(0.5).unwrap();
    }
    // The animation ended on tick 8: state code resumed and ran Counter=1, then stop.
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(1)));
    assert!(
        vm.trace.iter().any(|e| matches!(&e.kind, TraceKind::LatentResume { native, .. } if native == "Actor.FinishAnim"))
    );
    assert!(
        vm.trace
            .iter()
            .any(|e| matches!(&e.kind, TraceKind::StateStop { .. }))
    );
}

#[test]
fn animation_without_provider_fails_and_survey_counts_it() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let cases: [(&str, Vec<Value>, Vec<bool>); 4] = [
        (
            "Engine.Actor.PlayAnim",
            vec![
                Value::Name("Walk".into()),
                Value::Float(1.0),
                Value::Float(0.0),
                Value::Int(0),
            ],
            vec![false, false, false, false],
        ),
        (
            "Engine.Actor.LoopAnim",
            vec![Value::Name("Walk".into())],
            vec![false],
        ),
        (
            "Engine.Actor.TweenAnim",
            vec![Value::Name("Walk".into()), Value::Float(0.5)],
            vec![false, false],
        ),
        (
            "Engine.Actor.HasAnim",
            vec![Value::Name("Walk".into())],
            vec![false],
        ),
    ];
    for (path, mut args, omitted) in cases {
        let e = try_native(&mut vm, path, a, &omitted, &mut args).unwrap_err();
        assert!(
            matches!(&e.kind, VmErrorKind::NoAnimationProvider { native } if native == path.trim_start_matches("Engine.")),
            "{path}: {e}"
        );
    }
    // Survey mode counts it and continues.
    vm.survey = true;
    let mut args = [Value::Name("Walk".into())];
    try_native(&mut vm, "Engine.Actor.LoopAnim", a, &[false], &mut args).unwrap();
    let m = vm.missing_natives.get("Actor.LoopAnim").expect("counted");
    assert_eq!(m.index, Some(260));
    assert_eq!(m.calls, 1);
}

/// Test provider with explicit notify times.
struct ScriptedAnim {
    frames: u32,
    rate: f32,
    notifies: Vec<(f32, String)>,
}

impl crate::animation::AnimationData for ScriptedAnim {
    fn sequence(
        &mut self,
        _source: &str,
        _seq: &str,
    ) -> Result<Option<crate::animation::SeqInfo>, String> {
        Ok(Some(crate::animation::SeqInfo {
            frames: self.frames,
            rate: self.rate,
            notifies: self.notifies.clone(),
        }))
    }
}

#[test]
fn animation_notifies_fire_when_crossed() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(ScriptedAnim {
        frames: 4,
        rate: 1.0,
        notifies: vec![(0.5, "Notify".into())],
    }));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    play_anim(&mut vm, a, "Walk", 1.0, 0);
    // Notify at time01 0.5 == frame 2: crossed on the 4th 0.5 s tick.
    for _ in 0..3 {
        vm.tick(0.5).unwrap();
    }
    assert!(
        !vm.trace
            .iter()
            .any(|e| matches!(e.kind, TraceKind::AnimNotify { .. }))
    );
    vm.tick(0.5).unwrap();
    let n = vm
        .trace
        .iter()
        .filter(
            |e| matches!(&e.kind, TraceKind::AnimNotify { function, .. } if function == "Notify"),
        )
        .count();
    assert_eq!(n, 1, "notify fires exactly once");
}

#[test]
fn link_skel_anim_records_the_animation_source() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let anim = sg(&set, "Actor");
    let mut args = [Value::Object(Some(ObjRef::Static(anim)))];
    try_native(&mut vm, "Engine.Actor.LinkSkelAnim", a, &[false], &mut args).unwrap();
    let path = vm.set().path(anim);
    assert_eq!(
        vm.objects[a as usize].anim.linked_anims,
        vec![path],
        "the linked MeshAnimation path is remembered"
    );
    // The render `Mesh` is not clobbered with the MeshAnimation (upstream keeps them separate).
    assert_ne!(
        vm.get_property(a, "Mesh"),
        Some(&Value::Object(Some(ObjRef::Static(anim))))
    );
    // Linking the same animation twice is idempotent.
    let mut args = [Value::Object(Some(ObjRef::Static(anim)))];
    try_native(&mut vm, "Engine.Actor.LinkSkelAnim", a, &[false], &mut args).unwrap();
    assert_eq!(vm.objects[a as usize].anim.linked_anims.len(), 1);
}

/// Provider that records which sources it was asked about and answers from a script.
struct RecordingAnim {
    queried: std::rc::Rc<std::cell::RefCell<Vec<String>>>,
    answer: fn(&str) -> Result<Option<crate::animation::SeqInfo>, String>,
}

impl crate::animation::AnimationData for RecordingAnim {
    fn sequence(
        &mut self,
        source: &str,
        _seq: &str,
    ) -> Result<Option<crate::animation::SeqInfo>, String> {
        self.queried.borrow_mut().push(source.to_owned());
        (self.answer)(source)
    }
}

#[test]
fn animation_sources_are_queried_linked_first_then_mesh() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let queried = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    vm.set_animation_data(Box::new(RecordingAnim {
        queried: queried.clone(),
        answer: |_| Ok(None),
    }));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let linked = sg(&set, "Actor");
    let mut args = [Value::Object(Some(ObjRef::Static(linked)))];
    try_native(&mut vm, "Engine.Actor.LinkSkelAnim", a, &[false], &mut args).unwrap();
    // A distinct `Mesh` source (export 0 of the fixture package is `Object`).
    let mesh = GlobalRef {
        package: 0,
        export: 0,
    };
    vm.set_property(a, "Mesh", 0, Value::Object(Some(ObjRef::Static(mesh))));
    // `HasAnim` queries every source; all return None, so it answers false without erroring.
    let mut args = [Value::Name("Walk".into())];
    let r = try_native(&mut vm, "Engine.Actor.HasAnim", a, &[false], &mut args).unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Bool(false)));
    let sources = vm.animation_sources(a);
    assert_eq!(sources.len(), 2, "{sources:?}");
    assert_eq!(
        *queried.borrow(),
        sources,
        "query order is the source order"
    );
    assert!(
        sources[0].contains("Actor") && sources[1].contains("Object"),
        "{sources:?}"
    );
}

#[test]
fn animation_decode_error_is_explicit() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(RecordingAnim {
        queried: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        answer: |_| Err("corrupt payload at 0x10".into()),
    }));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let mut args = [
        Value::Name("Walk".into()),
        Value::Float(1.0),
        Value::Float(0.0),
        Value::Int(0),
    ];
    let e = try_native(
        &mut vm,
        "Engine.Actor.PlayAnim",
        a,
        &[false, false, false, false],
        &mut args,
    )
    .unwrap_err();
    assert!(
        matches!(&e.kind, VmErrorKind::AnimationDataError { message, .. } if message.contains("corrupt")),
        "{e}"
    );
}

#[test]
fn anim_blend_params_stores_channel_values_and_defaults() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let mut args = [
        Value::Int(2),
        Value::Float(0.25),
        Value::Float(0.5),
        Value::Float(0.75),
        Value::Name("Pelvis".into()),
    ];
    try_native(
        &mut vm,
        "Engine.Actor.AnimBlendParams",
        a,
        &[false, false, false, false, false],
        &mut args,
    )
    .unwrap();
    let p = vm.objects[a as usize].anim.blend_params.get(&2).unwrap();
    assert_eq!(p.blend_alpha, 0.25);
    assert_eq!(p.in_time, 0.5);
    assert_eq!(p.out_time, 0.75);
    assert_eq!(p.bone_name.as_deref(), Some("Pelvis"));
    // Omitted optional arguments: upstream defaults are BlendAlpha=1, InTime=OutTime=0, no bone.
    let mut args = [Value::Int(5)];
    try_native(
        &mut vm,
        "Engine.Actor.AnimBlendParams",
        a,
        &[false, true, true, true, true],
        &mut args,
    )
    .unwrap();
    let p = vm.objects[a as usize].anim.blend_params.get(&5).unwrap();
    assert_eq!(p.blend_alpha, 1.0);
    assert_eq!(p.in_time, 0.0);
    assert_eq!(p.out_time, 0.0);
    assert_eq!(p.bone_name, None);
    // `BoneName = None` is also no filter.
    let mut args = [
        Value::Int(6),
        Value::Float(0.5),
        Value::Float(0.0),
        Value::Float(0.0),
        Value::Name("None".into()),
    ];
    try_native(
        &mut vm,
        "Engine.Actor.AnimBlendParams",
        a,
        &[false, false, false, false, false],
        &mut args,
    )
    .unwrap();
    assert_eq!(
        vm.objects[a as usize]
            .anim
            .blend_params
            .get(&6)
            .unwrap()
            .bone_name,
        None
    );
}

#[test]
fn set_view_target_sets_the_field() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let pc = vm.spawn(sg(&set, "PlayerController"), "PC").unwrap();
    let target = vm.spawn(sg(&set, "Actor"), "T").unwrap();
    let mut args = [Value::Object(Some(ObjRef::Instance(target)))];
    try_native(
        &mut vm,
        "Engine.PlayerController.SetViewTarget",
        pc,
        &[false],
        &mut args,
    )
    .unwrap();
    assert_eq!(
        vm.get_property(pc, "ViewTarget"),
        Some(&Value::Object(Some(ObjRef::Instance(target))))
    );
}

#[test]
fn vector_rotator_operators_match_the_basis() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let rotate = |vm: &mut Vm<'_>, path: &str, v: [f32; 3], r: [i32; 3]| -> [f32; 3] {
        let mut args = [Value::Vector(v), Value::Rotator(r)];
        match try_native(vm, path, a, &[false, false], &mut args).unwrap() {
            NativeOutcome::Value(Value::Vector(out)) => out,
            other => panic!("{other:?}"),
        }
    };
    let close = |a: [f32; 3], b: [f32; 3]| (0..3).all(|i| (a[i] - b[i]).abs() < 1e-4);

    // Identity rotator: both directions are the identity.
    assert!(close(
        rotate(
            &mut vm,
            "Object.GreaterGreater_VectorRotator",
            [1.0, 2.0, 3.0],
            [0, 0, 0]
        ),
        [1.0, 2.0, 3.0]
    ));
    assert!(close(
        rotate(
            &mut vm,
            "Object.LessLess_VectorRotator",
            [1.0, 2.0, 3.0],
            [0, 0, 0]
        ),
        [1.0, 2.0, 3.0]
    ));
    // Yaw 16384 == 90 degrees: local+X becomes world+Y.
    assert!(close(
        rotate(
            &mut vm,
            "Object.GreaterGreater_VectorRotator",
            [1.0, 0.0, 0.0],
            [0, 16384, 0]
        ),
        [0.0, 1.0, 0.0]
    ));
    assert!(close(
        rotate(
            &mut vm,
            "Object.LessLess_VectorRotator",
            [0.0, 1.0, 0.0],
            [0, 16384, 0]
        ),
        [1.0, 0.0, 0.0]
    ));
    // Pitch 16384 == 90 degrees: local+X becomes world+Z.
    assert!(close(
        rotate(
            &mut vm,
            "Object.GreaterGreater_VectorRotator",
            [1.0, 0.0, 0.0],
            [16384, 0, 0]
        ),
        [0.0, 0.0, 1.0]
    ));
    // Round trip local -> world -> local.
    let r = [4096, 12000, 3000];
    let v = [3.0, -1.0, 2.0];
    let world = rotate(&mut vm, "Object.GreaterGreater_VectorRotator", v, r);
    let back = rotate(&mut vm, "Object.LessLess_VectorRotator", world, r);
    assert!(close(back, v), "{back:?}");
}

// ---------------------------------------------------------------------------------------
// Possession and presentation-event fixtures
// ---------------------------------------------------------------------------------------

fn push_iv(code: &mut Vec<u8>, r: i32) {
    code.push(0x01);
    code.extend(compact(r));
}

fn push_lv(code: &mut Vec<u8>, i: i32) {
    code.push(0x00);
    code.extend(compact(i));
}

/// Synthetic possession fixture: a `Controller` with a `Pawn` link and an `Init` state, and a
/// `Pawn` with `Controller`/`ControllerClass` links whose `PostBeginPlay` spawns and possesses.
fn possession_fixture() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let controller = b.reserve(0, 0, "Controller");
    let pawn = b.reserve(0, 0, "Pawn");

    let c_pawn = b.reserve(IMP_OBJECTPROP, controller, "Pawn");
    let p_controller = b.reserve(IMP_OBJECTPROP, pawn, "Controller");
    let p_ctrlclass = b.reserve(IMP_OBJECTPROP, pawn, "ControllerClass");
    let possess = b.reserve(IMP_FUNCTION, controller, "Possess");
    let pbp = b.reserve(IMP_FUNCTION, pawn, "PostBeginPlay");
    let init = b.reserve(IMP_STATE, controller, "Init");
    let a_pawn = b.reserve(IMP_OBJECTPROP, possess, "aPawn");
    // Native declarations so the bytecode's native indices resolve (their bodies are in the VM).
    let ne_oo = b.reserve(IMP_FUNCTION, object, "NotEqual_ObjectObject");
    let gts = b.reserve(IMP_FUNCTION, object, "GotoState");
    let spawn = b.reserve(IMP_FUNCTION, actor, "Spawn");
    let ne_a = b.reserve(IMP_OBJECTPROP, ne_oo, "A");
    let ne_b = b.reserve(IMP_OBJECTPROP, ne_oo, "B");
    let gts_state = b.reserve(IMP_NAMEPROP, gts, "NewState");
    let spawn_class = b.reserve(IMP_OBJECTPROP, spawn, "SpawnClass");

    // Controller children: Pawn (link) -> Possess (function) -> Init (state).
    b.prop_with(c_pawn, possess, 0, &compact(0));
    b.func(possess, init, a_pawn, &[], 0, 0, ff::DEFINED);
    b.state(init, 0, &[], 0, 0);
    // Pawn children: Controller -> ControllerClass -> PostBeginPlay.
    b.prop_with(p_controller, p_ctrlclass, 0, &compact(0));
    b.prop_with(p_ctrlclass, pbp, 0, &compact(0));
    b.prop_with(a_pawn, 0, pf::PARM, &compact(0));
    // Object children: NotEqual_ObjectObject -> GotoState; Actor children: Spawn.
    b.func(ne_oo, gts, ne_a, &[], 0, 119, ff::FINAL | ff::NATIVE);
    b.func(gts, 0, gts_state, &[], 0, 113, ff::FINAL | ff::NATIVE);
    b.func(spawn, 0, spawn_class, &[], 0, 278, ff::FINAL | ff::NATIVE);
    b.prop_with(ne_a, ne_b, pf::PARM, &compact(0));
    b.prop_with(ne_b, 0, pf::PARM, &compact(0));
    b.prop(gts_state, 0, pf::PARM);
    b.prop_with(spawn_class, 0, pf::PARM, &compact(0));

    let possess_name = b.name("Possess");
    let init_name = b.name("Init");

    // Controller.Possess(aPawn): self.Pawn = aPawn; aPawn.Controller = self; GotoState('Init').
    let mut code = Vec::new();
    code.push(0x0F); // Let
    push_iv(&mut code, c_pawn);
    push_lv(&mut code, a_pawn);
    code.push(0x0F); // Let
    code.push(0x19); // Context aPawn
    push_lv(&mut code, a_pawn);
    code.extend([0, 0, 0]); // Context Skip (u16) + Size (u8), unused by the VM
    push_iv(&mut code, p_controller);
    code.push(0x17); // Self
    code.push(0x71); // GotoState (native 113)
    code.push(0x21); // NameConst
    code.extend(compact(init_name));
    code.push(0x16); // EndFunctionParms
    code.push(0x04); // Return
    code.push(0x0B); // Nothing
    b.func(possess, init, a_pawn, &code, 36, 0, ff::DEFINED);

    // Pawn.PostBeginPlay(): if (ControllerClass != None) Controller = Spawn(ControllerClass);
    //                        if (Controller != None) Controller.Possess(self);
    let mut code = Vec::new();
    code.push(0x07); // JumpIfNot skip1
    code.extend(25u16.to_le_bytes());
    code.push(0x77); // != (native 119)
    push_iv(&mut code, p_ctrlclass);
    code.push(0x2A); // NoObject
    code.push(0x16); // EndFunctionParms
    code.push(0x0F); // Let
    push_iv(&mut code, p_controller);
    code.push(0x61); // Spawn (native 278)
    code.push(0x16);
    push_iv(&mut code, p_ctrlclass);
    code.push(0x16);
    code.push(0x07); // JumpIfNot skip2
    code.extend(52u16.to_le_bytes());
    code.push(0x77);
    push_iv(&mut code, p_controller);
    code.push(0x2A);
    code.push(0x16);
    code.push(0x19); // Context Controller
    push_iv(&mut code, p_controller);
    code.extend([0, 0, 0]); // Context Skip + Size
    code.push(0x1B); // VirtualFunction Possess
    code.extend(compact(possess_name));
    code.push(0x17); // Self
    code.push(0x16); // EndFunctionParms
    code.push(0x04); // Return
    code.push(0x0B); // Nothing
    b.func(pbp, 0, 0, &code, 54, 0, ff::DEFINED);

    b.class(object, 0, ne_oo);
    b.class(actor, object, spawn);
    b.class(controller, actor, c_pawn);
    b.class(pawn, actor, p_controller);
    b.build()
}

/// Synthetic presentation fixture: the `Actor` presentation natives with their decoded
/// signatures (`PlaySound`/`PlayMusic` take a Sound plus five optional ints;
/// `ReplaceATextureByAnOther` takes two Texture objects).
fn presentation_fixture() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let play_sound = b.reserve(IMP_FUNCTION, actor, "PlaySound");
    let play_music = b.reserve(IMP_FUNCTION, actor, "PlayMusic");
    let replace = b.reserve(IMP_FUNCTION, actor, "ReplaceATextureByAnOther");

    let sound = b.reserve(IMP_OBJECTPROP, play_sound, "Sound");
    let p1 = b.reserve(IMP_INTPROP, play_sound, "Param1");
    let p2 = b.reserve(IMP_INTPROP, play_sound, "Param2");
    let p3 = b.reserve(IMP_INTPROP, play_sound, "Param3");
    let p4 = b.reserve(IMP_INTPROP, play_sound, "Param4");
    let p5 = b.reserve(IMP_INTPROP, play_sound, "Param5");
    let msound = b.reserve(IMP_OBJECTPROP, play_music, "Sound");
    let src = b.reserve(IMP_OBJECTPROP, replace, "SrcTexture");
    let dst = b.reserve(IMP_OBJECTPROP, replace, "DestTexture");

    b.func(
        play_sound,
        play_music,
        sound,
        &[],
        0,
        264,
        ff::FINAL | ff::NATIVE,
    );
    b.func(
        play_music,
        replace,
        msound,
        &[],
        0,
        358,
        ff::FINAL | ff::NATIVE,
    );
    b.func(replace, 0, src, &[], 0, 0, ff::FINAL | ff::NATIVE);
    b.prop_with(sound, p1, pf::PARM, &compact(0));
    b.prop(p1, p2, pf::PARM);
    b.prop(p2, p3, pf::PARM);
    b.prop(p3, p4, pf::PARM);
    b.prop(p4, p5, pf::PARM);
    b.prop(p5, 0, 0);
    b.prop_with(msound, 0, pf::PARM, &compact(0));
    b.prop_with(src, dst, pf::PARM, &compact(0));
    b.prop_with(dst, 0, pf::PARM, &compact(0));
    b.class(object, 0, 0);
    b.class(actor, object, play_sound);
    b.build()
}

#[test]
fn possession_links_pawn_and_controller_and_enters_initial_state() {
    let set = set_of(possession_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let pawn = vm.spawn(g(&set, "Pawn"), "P").unwrap();
    vm.set_active(pawn, true);
    vm.set_property(
        pawn,
        "ControllerClass",
        0,
        Value::Object(Some(ObjRef::Static(g(&set, "Controller")))),
    );
    vm.send_event(pawn, "PostBeginPlay", Vec::new()).unwrap();
    let controller = vm
        .obj_prop(pawn, "Controller")
        .expect("a controller was spawned");
    assert!(vm.is_a(controller, "Controller"));
    // Both directions of the link.
    assert_eq!(vm.obj_prop(controller, "Pawn"), Some(pawn));
    assert_eq!(vm.obj_prop(pawn, "Controller"), Some(controller));
    // The controller entered its (only) state through `Possess`.
    assert_eq!(vm.state_name(controller).as_deref(), Some("Init"));
    assert!(vm.trace.iter().any(|e| matches!(
        &e.kind,
        TraceKind::Spawned { class, .. } if class.ends_with("Controller")
    )));
}

#[test]
fn pawn_without_controller_class_stays_uncontrolled() {
    let set = set_of(possession_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let pawn = vm.spawn(g(&set, "Pawn"), "P").unwrap();
    vm.set_active(pawn, true);
    vm.send_event(pawn, "PostBeginPlay", Vec::new()).unwrap();
    assert_eq!(
        vm.get_property(pawn, "Controller"),
        Some(&Value::Object(None))
    );
    assert!(
        !vm.trace
            .iter()
            .any(|e| matches!(&e.kind, TraceKind::Spawned { .. }))
    );
}

#[test]
fn play_sound_and_music_emit_one_event_with_decoded_arguments() {
    use crate::events::PresentationEvent;
    let set = set_of(presentation_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(g(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    let sound = ObjRef::Static(g(&set, "Object"));
    vm.call_function(
        g(&set, "Actor.PlaySound"),
        a,
        vec![
            Value::Object(Some(sound)),
            Value::Int(3),
            Value::Int(80),
            Value::Int(7),
            Value::Int(2),
            Value::Int(9),
        ],
    )
    .unwrap();
    let events = vm.drain_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        PresentationEvent::PlaySound(e) => {
            assert_eq!(e.actor, "A");
            assert_eq!(e.sound.as_deref(), Some("Test.Object"));
            assert_eq!(
                (e.slot, e.volume, e.radius, e.pitch, e.param5),
                (Some(3), Some(80), Some(7), Some(2), Some(9))
            );
        }
        other => panic!("{other:?}"),
    }
    // `drain_events` empties the queue.
    assert!(vm.drain_events().is_empty());
    vm.call_function(g(&set, "Actor.PlayMusic"), a, vec![Value::Object(None)])
        .unwrap();
    let events = vm.drain_events();
    assert_eq!(events.len(), 1);
    assert!(matches!(&events[0], PresentationEvent::PlayMusic(_)));
}

#[test]
fn replace_texture_emits_the_event() {
    use crate::events::PresentationEvent;
    let set = set_of(presentation_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(g(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    let src = ObjRef::Static(g(&set, "Object"));
    let dst = ObjRef::Static(g(&set, "Actor"));
    vm.call_function(
        g(&set, "Actor.ReplaceATextureByAnOther"),
        a,
        vec![Value::Object(Some(src)), Value::Object(Some(dst))],
    )
    .unwrap();
    let events = vm.drain_events();
    assert_eq!(events.len(), 1);
    match &events[0] {
        PresentationEvent::ReplaceTexture {
            actor,
            source,
            destination,
            ..
        } => {
            assert_eq!(actor, "A");
            assert_eq!(source.as_deref(), Some("Test.Object"));
            assert_eq!(destination.as_deref(), Some("Test.Actor"));
        }
        other => panic!("{other:?}"),
    }
    assert!(vm.drain_events().is_empty());
}

// ---------------------------------------------------------------------------------------
// Controller pathing natives over the decoded navigation graph

fn call_native_stateful(
    vm: &mut Vm<'_>,
    path: &str,
    this: ObjectId,
    omitted: &[bool],
    args: &mut [Value],
) -> NativeOutcome {
    let def = native(path);
    let mut c = ctx(this, omitted, path);
    c.in_state_code = true;
    (def.f)(vm, &c, args).expect("native")
}

fn spawn_at(vm: &mut Vm<'_>, set: &ScriptSet, class: &str, name: &str, loc: [f32; 3]) -> ObjectId {
    let id = vm.spawn(pg(set, class), name).unwrap();
    vm.set_property(id, "Location", 0, Value::Vector(loc));
    id
}

/// A pawn + controller wired together, with the given collision size and speed.
fn nav_actor_pair(
    vm: &mut Vm<'_>,
    set: &ScriptSet,
    radius: f32,
    height: f32,
    speed: f32,
) -> (ObjectId, ObjectId) {
    let pawn = spawn_at(vm, set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(pawn, "CollisionRadius", 0, Value::Float(radius));
    vm.set_property(pawn, "CollisionHeight", 0, Value::Float(height));
    vm.set_property(pawn, "GroundSpeed", 0, Value::Float(speed));
    let ctrl = vm.spawn(pg(set, "Controller"), "C").unwrap();
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    (ctrl, pawn)
}

fn route_cache_elem(vm: &Vm<'_>, ctrl: ObjectId, i: usize) -> Value {
    let base = vm.objects[ctrl as usize]
        .layout
        .slot_by_name("RouteCache")
        .expect("RouteCache")
        .base;
    vm.objects[ctrl as usize].props[base + i].clone()
}

#[test]
fn find_path_toward_returns_first_node_and_fills_route_cache() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let n0 = spawn_at(&mut vm, &set, "Actor", "Nav0", [0.0, 0.0, 0.0]);
    let n1 = spawn_at(&mut vm, &set, "Actor", "Nav1", [500.0, 0.0, 0.0]);
    let n2 = spawn_at(&mut vm, &set, "Actor", "Nav2", [1000.0, 0.0, 0.0]);
    let (ctrl, _pawn) = nav_actor_pair(&mut vm, &set, 40.0, 80.0, 100.0);
    vm.set_navigation(Box::new(MockNav::line()));
    let target = spawn_at(&mut vm, &set, "Actor", "T", [1000.0, 0.0, 0.0]);

    let mut args = [
        Value::Object(Some(ObjRef::Instance(target))),
        Value::Bool(false),
    ];
    let out = call_native(
        &mut vm,
        "Engine.Controller.FindPathToward",
        ctrl,
        &[false, false],
        &mut args,
    );
    // The path is Nav0, Nav1, Nav2; RouteCache drops the node the pawn stands on, so the first
    // move target is Nav1.
    assert_eq!(
        out,
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(n1))))
    );
    assert_eq!(
        route_cache_elem(&vm, ctrl, 0),
        Value::Object(Some(ObjRef::Instance(n1)))
    );
    assert_eq!(
        route_cache_elem(&vm, ctrl, 1),
        Value::Object(Some(ObjRef::Instance(n2)))
    );
    assert_eq!(route_cache_elem(&vm, ctrl, 2), Value::Object(None));
    match vm.get_property(ctrl, "RouteDist") {
        Some(Value::Float(d)) => assert!((*d - 1000.0).abs() < 1.0, "RouteDist {d}"),
        other => panic!("RouteDist {other:?}"),
    }
    // Nav0 is only referenced through the graph; keep the id used so the test reads naturally.
    assert!(vm.vector_prop(n0, "Location").is_some());
}

#[test]
fn find_path_to_unreachable_clears_route_cache_and_returns_none() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav0", [0.0, 0.0, 0.0]);
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav1", [500.0, 0.0, 0.0]);
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav2", [1000.0, 0.0, 0.0]);
    let (ctrl, _pawn) = nav_actor_pair(&mut vm, &set, 40.0, 80.0, 100.0);
    // Only 0 -> 1 exists; the nearest node to the target (Nav2) is unreachable.
    let mut nav = MockNav::line();
    nav.edges.truncate(1);
    vm.set_navigation(Box::new(nav));

    let mut args = [Value::Vector([1000.0, 0.0, 0.0]), Value::Bool(false)];
    let out = call_native(
        &mut vm,
        "Engine.Controller.FindPathTo",
        ctrl,
        &[false, false],
        &mut args,
    );
    assert_eq!(out, NativeOutcome::Value(Value::Object(None)));
    assert_eq!(route_cache_elem(&vm, ctrl, 0), Value::Object(None));
    assert_eq!(vm.get_property(ctrl, "RouteDist"), Some(&Value::Float(0.0)));
}

#[test]
fn a_pawn_too_big_for_the_reach_spec_finds_no_path() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav0", [0.0, 0.0, 0.0]);
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav1", [500.0, 0.0, 0.0]);
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav2", [1000.0, 0.0, 0.0]);
    // Pawn radius 80 > the reach spec's 20: the edge is not traversable.
    let (ctrl, _pawn) = nav_actor_pair(&mut vm, &set, 80.0, 80.0, 100.0);
    let mut nav = MockNav::line();
    nav.edges[0].collision_radius = 20;
    vm.set_navigation(Box::new(nav));
    let target = spawn_at(&mut vm, &set, "Actor", "T", [1000.0, 0.0, 0.0]);

    let mut args = [
        Value::Object(Some(ObjRef::Instance(target))),
        Value::Bool(false),
    ];
    let out = call_native(
        &mut vm,
        "Engine.Controller.FindPathToward",
        ctrl,
        &[false, false],
        &mut args,
    );
    assert_eq!(out, NativeOutcome::Value(Value::Object(None)));
}

#[test]
fn pathing_without_a_nav_provider_fails_and_survey_counts_it() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let (ctrl, _pawn) = nav_actor_pair(&mut vm, &set, 40.0, 80.0, 100.0);
    let target = spawn_at(&mut vm, &set, "Actor", "T", [1000.0, 0.0, 0.0]);
    let mut args = [
        Value::Object(Some(ObjRef::Instance(target))),
        Value::Bool(false),
    ];
    let e = try_native(
        &mut vm,
        "Engine.Controller.FindPathToward",
        ctrl,
        &[false, false],
        &mut args,
    )
    .unwrap_err();
    assert!(
        matches!(&e.kind, VmErrorKind::NoNavProvider { native } if native == "Controller.FindPathToward"),
        "{e}"
    );
    // Survey mode counts it instead.
    vm.survey = true;
    let r = try_native(
        &mut vm,
        "Engine.Controller.FindPathToward",
        ctrl,
        &[false, false],
        &mut args,
    )
    .unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Object(None)));
    assert!(vm.missing_natives.contains_key("Controller.FindPathToward"));
}

#[test]
fn move_to_sets_destination_and_latent_then_completes_at_ground_speed() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let (ctrl, pawn) = nav_actor_pair(&mut vm, &set, 1.0, 40.0, 100.0);

    let mut args = [Value::Vector([300.0, 0.0, 0.0])];
    let out = call_native_stateful(
        &mut vm,
        "Engine.Controller.MoveTo",
        ctrl,
        &[false, true, true],
        &mut args,
    );
    assert_eq!(out, NativeOutcome::Value(Value::Void));
    assert_eq!(
        vm.get_property(ctrl, "Destination"),
        Some(&Value::Vector([300.0, 0.0, 0.0]))
    );
    match &vm.pending_latent {
        Some(crate::vm::Latent::Move { destination, .. }) => {
            assert_eq!(*destination, [300.0, 0.0, 0.0]);
        }
        other => panic!("no Move latent: {other:?}"),
    }
    // 300 UU at the pawn's GroundSpeed of 100 UU/s, dt 0.1 -> exactly 30 ticks.
    let mut ticks = 0;
    loop {
        let arrived = vm
            .move_pawn_step(pawn, [300.0, 0.0, 0.0], 0.0, 0.1)
            .unwrap();
        ticks += 1;
        if arrived {
            break;
        }
        assert!(ticks <= 31, "did not arrive");
    }
    assert_eq!(ticks, 30);
    assert_eq!(vm.vector_prop(pawn, "Location"), Some([300.0, 0.0, 0.0]));
}

#[test]
fn move_toward_sets_move_target_and_destination() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let (ctrl, pawn) = nav_actor_pair(&mut vm, &set, 1.0, 40.0, 100.0);
    let target = spawn_at(&mut vm, &set, "Actor", "T", [250.0, 0.0, 0.0]);

    let mut args = [Value::Object(Some(ObjRef::Instance(target)))];
    let out = call_native_stateful(
        &mut vm,
        "Engine.Controller.MoveToward",
        ctrl,
        &[false, true, true, true],
        &mut args,
    );
    assert_eq!(out, NativeOutcome::Value(Value::Void));
    assert_eq!(
        vm.get_property(ctrl, "MoveTarget"),
        Some(&Value::Object(Some(ObjRef::Instance(target))))
    );
    assert_eq!(
        vm.get_property(ctrl, "Destination"),
        Some(&Value::Vector([250.0, 0.0, 0.0]))
    );
    // MoveToward completes after the expected number of ticks at GroundSpeed: 250 UU at
    // 100 UU/s with dt 0.1 -> exactly 25 steps.
    let mut ticks = 0;
    loop {
        let arrived = vm
            .move_pawn_step(pawn, [250.0, 0.0, 0.0], 0.0, 0.1)
            .unwrap();
        ticks += 1;
        if arrived {
            break;
        }
        assert!(ticks <= 26, "MoveToward did not arrive");
    }
    assert_eq!(ticks, 25);
    assert_eq!(vm.vector_prop(pawn, "Location"), Some([250.0, 0.0, 0.0]));
}

#[test]
fn line_of_sight_to_uses_the_pawn_eyes_and_reports_the_blocking_wall() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    // A thin wall between the two pawns at x = 50.
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([50.0, -10.0, -10.0], [51.0, 10.0, 200.0]),
    ));
    let (ctrl, pawn) = nav_actor_pair(&mut vm, &set, 10.0, 40.0, 100.0);
    vm.set_property(pawn, "BaseEyeHeight", 0, Value::Float(20.0));
    let other = spawn_at(&mut vm, &set, "Pawn", "Other", [100.0, 0.0, 0.0]);
    vm.set_property(other, "BaseEyeHeight", 0, Value::Float(20.0));

    let mut args = [Value::Object(Some(ObjRef::Instance(other)))];
    let blocked = call_native(
        &mut vm,
        "Engine.Controller.LineOfSightTo",
        ctrl,
        &[false],
        &mut args,
    );
    assert_eq!(blocked, NativeOutcome::Value(Value::Bool(false)));

    // No wall: clear line of sight.
    let mut vm2 = Vm::new(&set, VmLimits::default());
    vm2.set_physics(Box::new(MockWorld::new()));
    let (ctrl2, pawn2) = nav_actor_pair(&mut vm2, &set, 10.0, 40.0, 100.0);
    vm2.set_property(pawn2, "BaseEyeHeight", 0, Value::Float(20.0));
    let other2 = spawn_at(&mut vm2, &set, "Pawn", "Other", [100.0, 0.0, 0.0]);
    vm2.set_property(other2, "BaseEyeHeight", 0, Value::Float(20.0));
    let mut args2 = [Value::Object(Some(ObjRef::Instance(other2)))];
    let clear = call_native(
        &mut vm2,
        "Engine.Controller.LineOfSightTo",
        ctrl2,
        &[false],
        &mut args2,
    );
    assert_eq!(clear, NativeOutcome::Value(Value::Bool(true)));
}

#[test]
fn vrand_is_a_unit_vector() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Object"), "O").unwrap();
    let mut args = [];
    let out = call_native(&mut vm, "Object.VRand", o, &[], &mut args);
    let NativeOutcome::Value(Value::Vector(v)) = out else {
        panic!("{out:?}");
    };
    let len = (v[0] * v[0] + v[1] * v[1] + v[2] * v[2]).sqrt();
    assert!((len - 1.0).abs() < 1e-4, "len {len}");
}

#[test]
fn move_to_outside_state_code_is_rejected() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let (ctrl, _pawn) = nav_actor_pair(&mut vm, &set, 10.0, 40.0, 100.0);
    // `ctx` sets in_state_code = false.
    let mut args = [Value::Vector([100.0, 0.0, 0.0])];
    let e = try_native(
        &mut vm,
        "Engine.Controller.MoveTo",
        ctrl,
        &[false, true, true],
        &mut args,
    )
    .unwrap_err();
    assert!(
        matches!(&e.kind, VmErrorKind::LatentOutsideState { path } if path == "Controller.MoveTo"),
        "{e}"
    );
}

/// Synthetic mover: an `Actor` with the `PHYS_MovingBrush` properties, the `Add_IntInt` native
/// and a `KeyFrameReached` handler. The state `Mover.InterpolateTo` leaves behind is set
/// directly by the test (the interpreter has no mover script here).
fn mover_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
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
    let vector_extra = compact(IMP_STRUCT);
    let rotator_extra = compact(IMP_STRUCT - 1);
    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let rotation = b.reserve(IMP_STRUCTPROP, actor, "Rotation");
    let old_pos = b.reserve(IMP_STRUCTPROP, actor, "OldPos");
    let old_rot = b.reserve(IMP_STRUCTPROP, actor, "OldRot");
    let base_pos = b.reserve(IMP_STRUCTPROP, actor, "BasePos");
    let base_rot = b.reserve(IMP_STRUCTPROP, actor, "BaseRot");
    let phys_alpha = b.reserve(IMP_FLOATPROP, actor, "PhysAlpha");
    let phys_rate = b.reserve(IMP_FLOATPROP, actor, "PhysRate");
    let key_num = b.reserve(IMP_BYTEPROP, actor, "KeyNum");
    let interp = b.reserve(IMP_BOOLPROP, actor, "bInterpolating");
    let key_pos = b.reserve(IMP_ARRAYPROP, actor, "KeyPos");
    let key_rot = b.reserve(IMP_ARRAYPROP, actor, "KeyRot");
    let key_hits = b.reserve(IMP_INTPROP, actor, "KeyHits");
    let kf = b.reserve(IMP_FUNCTION, actor, "KeyFrameReached");
    b.prop_with(location, rotation, 0, &vector_extra);
    b.prop_with(rotation, old_pos, 0, &rotator_extra);
    b.prop_with(old_pos, old_rot, 0, &vector_extra);
    b.prop_with(old_rot, base_pos, 0, &rotator_extra);
    b.prop_with(base_pos, base_rot, 0, &vector_extra);
    b.prop_with(base_rot, phys_alpha, 0, &rotator_extra);
    b.prop(phys_alpha, phys_rate, 0);
    b.prop(phys_rate, key_num, 0);
    // ByteProperty carries an `Enum` object reference (None here).
    b.prop_with(key_num, interp, 0, &compact(0));
    b.prop(interp, key_pos, 0);
    b.prop_array_dim(key_pos, key_rot, 0, IMP_STRUCT, 8);
    b.prop_array_dim(key_rot, key_hits, 0, IMP_STRUCT - 1, 8);
    b.prop(key_hits, kf, 0);
    let kh = key_hits as u8;
    let kf_code = vec![
        0x0F, 0x01, kh, 0x92, 0x00, kh, 0x26, 0x16, // KeyHits = KeyHits + 1
        0x04, 0x0B, // return
    ];
    b.func(kf, 0, 0, &kf_code, 0x10, 0, DEFINED);
    b.class(object, 0, add, 0);
    b.class(actor, object, location, 0);
    b.build()
}

fn mover_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Mover",
        mover_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

/// `PHYS_MovingBrush` interpolation advances by `PhysRate*dt`, snaps to the key at
/// `PhysAlpha >= 1` and fires `KeyFrameReached` exactly once.
#[test]
fn synthetic_mover_interpolates_and_fires_keyframe_reached() {
    let set = mover_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "M").unwrap();
    vm.set_active(a, true);
    vm.set_property(a, "PhysRate", 0, Value::Float(2.0)); // 0.5 s to the key
    vm.set_property(a, "PhysAlpha", 0, Value::Float(0.0));
    vm.set_property(a, "KeyNum", 0, Value::Byte(1));
    vm.set_property(a, "BasePos", 0, Value::Vector([0.0, 0.0, 0.0]));
    vm.set_property(a, "BaseRot", 0, Value::Rotator([0, 0, 0]));
    vm.set_property(a, "OldPos", 0, Value::Vector([0.0, 0.0, 0.0]));
    vm.set_property(a, "OldRot", 0, Value::Rotator([0, 0, 0]));
    vm.set_property(a, "KeyPos", 1, Value::Vector([100.0, 0.0, 0.0]));
    vm.set_property(a, "KeyRot", 1, Value::Rotator([0, 18000, 0]));
    vm.set_property(a, "bInterpolating", 0, Value::Bool(true));

    vm.tick(0.25).unwrap();
    let loc = vm.vector_prop(a, "Location").unwrap();
    assert!((loc[0] - 50.0).abs() < 1e-3, "half-way location {loc:?}");
    assert_eq!(
        vm.get_property(a, "bInterpolating"),
        Some(&Value::Bool(true))
    );
    assert_eq!(vm.get_property(a, "KeyHits"), Some(&Value::Int(0)));

    vm.tick(0.25).unwrap();
    assert_eq!(vm.vector_prop(a, "Location").unwrap()[0], 100.0);
    assert_eq!(
        vm.get_property(a, "Rotation"),
        Some(&Value::Rotator([0, 18000, 0]))
    );
    assert_eq!(
        vm.get_property(a, "bInterpolating"),
        Some(&Value::Bool(false))
    );
    assert_eq!(
        vm.get_property(a, "KeyHits"),
        Some(&Value::Int(1)),
        "KeyFrameReached must fire exactly once"
    );
    // Finished: further ticks do not move it and do not fire the event again.
    vm.tick(0.25).unwrap();
    assert_eq!(vm.vector_prop(a, "Location").unwrap()[0], 100.0);
    assert_eq!(vm.get_property(a, "KeyHits"), Some(&Value::Int(1)));
}

/// `Actor.FinishInterpolation` is latent (sets `Latent::Interp`) and refuses a call outside state
/// code. The resume path (`bInterpolating` clearing) is exercised by the opt-in Plage01 door test.
#[test]
fn finish_interpolation_is_latent_and_needs_state_code() {
    let set = mover_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "M").unwrap();
    vm.set_active(a, true);
    let def = native("Engine.Actor.FinishInterpolation");
    let mut args = [];
    let e = (def.f)(
        &mut vm,
        &ctx(a, &[], "Engine.Actor.FinishInterpolation"),
        &mut args,
    )
    .unwrap_err();
    assert!(
        matches!(e.kind, VmErrorKind::LatentOutsideState { .. }),
        "{e}"
    );
    assert!(vm.pending_latent.is_none());
    let inner = NativeCtx {
        this: a,
        in_state_code: true,
        path: "Engine.Actor.FinishInterpolation".to_owned(),
        omitted: Vec::new(),
    };
    (def.f)(&mut vm, &inner, &mut args).expect("native");
    assert!(
        matches!(vm.pending_latent, Some(Latent::Interp { .. })),
        "{:?}",
        vm.pending_latent
    );
}

// ---------------------------------------------------------------------------------------
// XIII AI natives (item3g)

fn alliance_entry(name: &str, level: f32) -> Value {
    Value::Struct(vec![
        ("alliancename".to_owned(), Value::Name(name.to_owned())),
        ("alliancelevel".to_owned(), Value::Float(level)),
    ])
}

/// Calls `IAController.AllianceLevel` on `ctrl` with `enemy`.
fn alliance_level(vm: &mut Vm<'_>, ctrl: ObjectId, enemy: Option<ObjectId>) -> i32 {
    let mut args = [match enemy {
        Some(e) => Value::Object(Some(ObjRef::Instance(e))),
        None => Value::Object(None),
    }];
    match call_native(vm, "IAController.AllianceLevel", ctrl, &[false], &mut args) {
        NativeOutcome::Value(Value::Int(v)) => v,
        other => panic!("{other:?}"),
    }
}

#[test]
fn alliance_level_table_and_guards() {
    let set = ai_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let ctrl = vm.spawn(pg(&set, "IAController"), "C").unwrap();
    let base = spawn_at(&mut vm, &set, "Pawn", "Base", [0.0, 0.0, 0.0]);
    let xiii = spawn_at(&mut vm, &set, "Pawn", "XIII", [0.0, 0.0, 0.0]);
    vm.set_property(base, "Alliance", 0, Value::Name("NMI".into()));
    for (i, e) in [
        alliance_entry("Player", -1.0),
        alliance_entry("NMI", 1.0),
        alliance_entry("Civil", 0.0),
        alliance_entry("Faction", 0.5),
    ]
    .into_iter()
    .enumerate()
    {
        assert!(vm.set_property(base, "InitialAlliances", i, e), "slot {i}");
    }
    vm.set_property(
        ctrl,
        "BaseS",
        0,
        Value::Object(Some(ObjRef::Instance(base))),
    );
    vm.set_property(ctrl, "XIII", 0, Value::Object(Some(ObjRef::Instance(xiii))));

    // The controller's own XIII is always an enemy.
    assert_eq!(alliance_level(&mut vm, ctrl, Some(xiii)), -1);
    // A null argument (missing BaseS check cannot help) returns -1 via the XIII comparison
    // only when XIII is null; here it takes the null-arg path.
    assert_eq!(alliance_level(&mut vm, ctrl, None), -1);

    // Matching `NMI` -> the stored level.
    let nmi = spawn_at(&mut vm, &set, "Pawn", "NmiFriend", [0.0, 0.0, 0.0]);
    vm.set_property(nmi, "Alliance", 0, Value::Name("NMI".into()));
    assert_eq!(alliance_level(&mut vm, ctrl, Some(nmi)), 1);

    // `Faction` at 0.5 is truncated toward zero.
    let faction = spawn_at(&mut vm, &set, "Pawn", "FactionPal", [0.0, 0.0, 0.0]);
    vm.set_property(faction, "Alliance", 0, Value::Name("Faction".into()));
    assert_eq!(alliance_level(&mut vm, ctrl, Some(faction)), 0);

    // `None` alliance never matches even when an entry is named `None` (engine guard).
    let none = spawn_at(&mut vm, &set, "Pawn", "NoAlliance", [0.0, 0.0, 0.0]);
    vm.set_property(none, "Alliance", 0, Value::Name("None".into()));
    assert_eq!(alliance_level(&mut vm, ctrl, Some(none)), 0);

    // Unmatched alliance -> neutral 0.
    let other = spawn_at(&mut vm, &set, "Pawn", "Stranger", [0.0, 0.0, 0.0]);
    vm.set_property(other, "Alliance", 0, Value::Name("Zorg".into()));
    assert_eq!(alliance_level(&mut vm, ctrl, Some(other)), 0);

    // A controller with no BaseS returns -1 (engine early-out).
    let lonely = vm.spawn(pg(&set, "IAController"), "Lonely").unwrap();
    assert_eq!(alliance_level(&mut vm, lonely, Some(nmi)), -1);
}

#[test]
fn spine_control_and_bone_direction_are_stored_per_actor() {
    let set = ai_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let pawn = spawn_at(&mut vm, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    let mut args = [Value::Bool(true), Value::Int(2000), Value::Float(0.9)];
    assert_eq!(
        call_native(
            &mut vm,
            "Engine.Pawn.SpineYawControl",
            pawn,
            &[false, false, false],
            &mut args
        ),
        NativeOutcome::Value(Value::Void)
    );
    let mut args = [
        Value::Name("X Spine".into()),
        Value::Rotator([10, 20, 30]),
        Value::Vector([1.0, 2.0, 3.0]),
        Value::Float(0.5),
    ];
    assert_eq!(
        call_native(
            &mut vm,
            "Engine.Actor.SetBoneDirection",
            pawn,
            &[false, false, false, false],
            &mut args
        ),
        NativeOutcome::Value(Value::Void)
    );
    let state = vm.bone_state(pawn).expect("bone state");
    assert_eq!(
        state.spine,
        Some(crate::vm::SpineControl {
            is_controlled: true,
            max_value: 2000,
            rotation_speed: 0.9
        })
    );
    assert_eq!(state.directions.len(), 1);
    assert_eq!(state.directions[0].bone, "X Spine");
    assert_eq!(state.directions[0].turn, [10, 20, 30]);
    assert_eq!(state.directions[0].trans, [1.0, 2.0, 3.0]);
    assert_eq!(state.directions[0].alpha, 0.5);
}

#[test]
fn halte_au_feu_clears_the_pawn_bone_state() {
    let set = ai_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let ctrl = vm.spawn(pg(&set, "IAController"), "C").unwrap();
    let pawn = spawn_at(&mut vm, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    vm.set_spine_control(pawn, true, 100, 1.0);
    vm.add_bone_direction(pawn, "X Spine".into(), [0, 0, 0], [0.0; 3], 0.0);
    let mut args: [Value; 0] = [];
    assert_eq!(
        call_native(&mut vm, "IAController.HalteAuFeu", ctrl, &[], &mut args),
        NativeOutcome::Value(Value::Void)
    );
    let state = vm.bone_state(pawn).expect("bone state");
    assert!(state.spine.is_none());
    assert!(state.directions.is_empty());
    // A controller with no pawn is a no-op.
    let lonely = vm.spawn(pg(&set, "IAController"), "Lonely").unwrap();
    let mut args: [Value; 0] = [];
    assert_eq!(
        call_native(&mut vm, "IAController.HalteAuFeu", lonely, &[], &mut args),
        NativeOutcome::Value(Value::Void)
    );
}

#[test]
fn near_wall_traces_forward_from_the_pawn_top() {
    let set = ai_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let ctrl = vm.spawn(pg(&set, "IAController"), "C").unwrap();
    let pawn = spawn_at(&mut vm, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(pawn, "CollisionHeight", 0, Value::Float(80.0));
    vm.set_property(pawn, "Rotation", 0, Value::Rotator([0, 0, 0]));
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([200.0, -50.0, 0.0], [220.0, 50.0, 200.0]),
    ));
    // Facing +X; the wall is at x=200.
    let mut args = [Value::Float(300.0)];
    assert_eq!(
        call_native(&mut vm, "IAController.NearWall", ctrl, &[false], &mut args),
        NativeOutcome::Value(Value::Bool(true))
    );
    let mut args = [Value::Float(100.0)];
    assert_eq!(
        call_native(&mut vm, "IAController.NearWall", ctrl, &[false], &mut args),
        NativeOutcome::Value(Value::Bool(false))
    );
}

#[test]
fn test_direction_traces_and_reports_mindist_clearance() {
    let set = ai_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let ctrl = vm.spawn(pg(&set, "IAController"), "C").unwrap();
    let pawn = spawn_at(&mut vm, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(pawn, "CollisionHeight", 0, Value::Float(80.0));
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    vm.set_physics(Box::new(MockWorld::new()));
    // Clear 300 UU to +X: pick = the end point, 300 UU from the pawn.
    let mut args = [
        Value::Float(150.0),
        Value::Float(300.0),
        Value::Vector([1.0, 0.0, 0.0]),
        Value::Vector([0.0; 3]),
    ];
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.TestDirection",
            ctrl,
            &[false, false, false, true],
            &mut args
        ),
        NativeOutcome::Value(Value::Bool(true))
    );
    assert_eq!(args[3], Value::Vector([300.0, 0.0, 80.0]));

    // A wall at x=100: the pick is the hit, closer than mindist=150 -> false.
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([100.0, -50.0, 0.0], [120.0, 50.0, 200.0]),
    ));
    let mut args = [
        Value::Float(150.0),
        Value::Float(300.0),
        Value::Vector([1.0, 0.0, 0.0]),
        Value::Vector([0.0; 3]),
    ];
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.TestDirection",
            ctrl,
            &[false, false, false, true],
            &mut args
        ),
        NativeOutcome::Value(Value::Bool(false))
    );
    match args[3] {
        Value::Vector([x, _, _]) => assert!(x <= 120.0, "pick {x}"),
        ref other => panic!("{other:?}"),
    }
}

#[test]
fn pick_start_point_prefers_a_patrol_point_then_falls_back() {
    let set = ai_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let ctrl = vm.spawn(pg(&set, "IAController"), "C").unwrap();
    let pawn = spawn_at(&mut vm, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    let _nav = spawn_at(&mut vm, &set, "Actor", "Nav0", [0.0, 0.0, 0.0]);
    let patrol = spawn_at(&mut vm, &set, "PatrolPoint", "PP0", [400.0, 0.0, 0.0]);
    vm.set_navigation(Box::new(MockNav::with_points(vec![
        NavPointInfo {
            actor: "Nav0".into(),
            location: [0.0, 0.0, 0.0],
            collision_radius: 120.0,
            collision_height: 120.0,
        },
        NavPointInfo {
            actor: "PP0".into(),
            location: [400.0, 0.0, 0.0],
            collision_radius: 120.0,
            collision_height: 120.0,
        },
    ])));
    let mut args: [Value; 0] = [];
    assert_eq!(
        call_native(&mut vm, "IAController.PickStartPoint", ctrl, &[], &mut args),
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(patrol))))
    );

    // No PatrolPoint: the nearest navigation point is used.
    let mut vm = Vm::new(&set, VmLimits::default());
    let ctrl = vm.spawn(pg(&set, "IAController"), "C").unwrap();
    let pawn = spawn_at(&mut vm, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    let near = spawn_at(&mut vm, &set, "Actor", "Nav0", [0.0, 0.0, 0.0]);
    let _far = spawn_at(&mut vm, &set, "Actor", "Nav1", [900.0, 0.0, 0.0]);
    vm.set_navigation(Box::new(MockNav::with_points(vec![
        NavPointInfo {
            actor: "Nav0".into(),
            location: [0.0, 0.0, 0.0],
            collision_radius: 120.0,
            collision_height: 120.0,
        },
        NavPointInfo {
            actor: "Nav1".into(),
            location: [900.0, 0.0, 0.0],
            collision_radius: 120.0,
            collision_height: 120.0,
        },
    ])));
    let mut args: [Value; 0] = [];
    assert_eq!(
        call_native(&mut vm, "IAController.PickStartPoint", ctrl, &[], &mut args),
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(near))))
    );
}
