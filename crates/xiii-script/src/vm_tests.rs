//! Synthetic interpreter tests: a generated version-100 package with an `Object` class
//! (native operators, script functions) and an `Actor` subclass with two states.

use xiii_package::Limits;

use crate::bytecode::ScriptLimits;
use crate::events::{PresentationEvent, TravelSource};
use crate::linker::{GlobalRef, ScriptPackage, ScriptSet};
use crate::localize::LocalizationData;
use crate::reflect::function_flags as ff;
use crate::reflect::property_flags as pf;
use crate::tests::{Exp, build_package, compact};
use crate::value::{Delegate, ObjRef, ObjectId, Value};
use crate::vm::{Latent, TraceKind, Vm, VmErrorKind, VmLimits};

struct B {
    names: Vec<String>,
    exports: Vec<Exp>,
    /// External imports `(package, class, object)` appended after the seven fixed imports; each
    /// contributes two import entries (the package root, then the object).
    externals: Vec<(String, String, String)>,
}

const IMP_CORE: i32 = -1;
const IMP_FUNCTION: i32 = -2;
const IMP_STATE: i32 = -3;
const IMP_INTPROP: i32 = -4;
const IMP_FLOATPROP: i32 = -5;
const IMP_NAMEPROP: i32 = -6;
const IMP_OBJPROP: i32 = -7;
const B_STRUCTPROP: i32 = -8;
const B_BOOLPROP: i32 = -9;
/// `Core.StrProperty` import appended at the end of the `B` import table (see `B::build`).
const B_STRPROP: i32 = -10;

impl B {
    fn new() -> Self {
        let mut b = Self {
            names: Vec::new(),
            exports: Vec::new(),
            externals: Vec::new(),
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
        self.state_children(r, next, 0, script, mem, labels_at);
    }

    /// A `Core.State` whose `children` point at a state-scoped function (item18 `PlayerTick`).
    fn state_children(
        &mut self,
        r: i32,
        next: i32,
        children: i32,
        script: &[u8],
        mem: u32,
        labels_at: u16,
    ) {
        let friendly = self.exports[(r - 1) as usize].name;
        let mut p = compact(0);
        p.extend(self.header(0, next, children, friendly, script, mem));
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

    /// Appends an external import `package.class` named `object` and returns the object import's
    /// raw reference (the value an `ObjectConst` token uses). Import entries: the nine fixed ones
    /// come first, then two per external (package root, object), so external `n`'s object import
    /// has index `10 + 2n` and raw `-(11 + 2n)`.
    fn add_external(&mut self, package: &str, class: &str, object: &str) -> i32 {
        let n = self.externals.len() as i32;
        self.externals
            .push((package.to_owned(), class.to_owned(), object.to_owned()));
        // `B::build` has 10 fixed imports (through `StrProperty`), so an external's package
        // import is at index 11 and its object import at 12; each extra external adds two.
        -(12 + 2 * n)
    }

    fn build(mut self) -> Vec<u8> {
        let core = self.name("Core");
        let package = self.name("Package");
        let class = self.name("Class");
        let engine = self.name("Engine");
        let externals = std::mem::take(&mut self.externals);
        let mut imports = vec![
            (core, package, 0, core),
            (core, class, -1, self.name("Function")),
            (core, class, -1, self.name("State")),
            (core, class, -1, self.name("IntProperty")),
            (core, class, -1, self.name("FloatProperty")),
            (core, class, -1, self.name("NameProperty")),
            (core, class, -1, self.name("ObjectProperty")),
            (core, class, -1, self.name("StructProperty")),
            (core, class, -1, self.name("BoolProperty")),
            // -10: appended last so every existing negative import index is unchanged.
            (core, class, -1, self.name("StrProperty")),
        ];
        for (pkg, cls, object) in &externals {
            let pn = self.name(pkg);
            let cn = self.name(cls);
            let on = self.name(object);
            let pkg_idx = imports.len() as i32;
            imports.push((core, package, 0, pn));
            imports.push((engine, cn, -(pkg_idx + 1), on));
        }
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
    // Real merged count. Contributions: item14b added 10 AI/perception natives; item3p added the
    // five missing rotator operators (142, 203, 287, 288, 289), the float power operator (170) and
    // a visible Partial for `ParticleEmitter.SetMaxParticles`; item16 added the menu natives
    // (`VideoPlayer.*`, `Actor.*AllSounds`, `PlayerController.ClientTravel`); item15 added
    // Real merged count. Contributions: item14b added 10 AI/perception natives; item3p added the
    // five missing rotator operators (142, 203, 287, 288, 289), the float power operator (170) and
    // a visible Partial for `ParticleEmitter.SetMaxParticles`; item16 added the menu natives
    // (`VideoPlayer.*`, `Actor.*AllSounds`, `PlayerController.ClientTravel`); item15 added
    // `PlayerController.GetDefaultURL` and `CalcFirstPersonView` (`ClientTravel` is shared with
    // item16, registered once); item3o added `SaveAtCheckpoint`, `OrthoRotation` and three
    // `SetBone*` Partials; item16b added `GUIController.GetStyle`/`InitStateFrame`; item18 added
    // `%` (173), `Normalize` (198), `ParticleEmitter.SpawnParticle` and `Actor.KillAllSounds`.
    // Must equal `Registry::builtin().defs().count()`.
    assert_eq!(defs.len(), 302);
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

    /// A `Core.Class` whose tagged defaults set one `ObjectProperty` to `object_ref`.
    fn class_with_object_default(
        &mut self,
        r: i32,
        sup: i32,
        children: i32,
        default_name: i32,
        object_ref: i32,
    ) {
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
        // ObjectProperty tag: name, then info = type code 5 (object) with the compact-reference
        // length in the size nibble (0/1/2 = 1/2/4 bytes), then the compact reference.
        p.extend(compact(default_name));
        let reference = compact(object_ref);
        let size_code = match reference.len() {
            1 => 0u8,
            2 => 1,
            _ => 2,
        };
        p.push(0x05 | (size_code << 4));
        p.extend(&reference);
        p.extend(compact(0)); // defaults terminator
        self.set(r, p);
    }

    fn state(&mut self, r: i32, next: i32, script: &[u8], mem: u32, labels_at: u16) {
        self.state_children(r, next, 0, script, mem, labels_at);
    }

    /// A `Core.State` whose `children` point at a state-scoped function (item18 `PlayerTick`).
    fn state_children(
        &mut self,
        r: i32,
        next: i32,
        children: i32,
        script: &[u8],
        mem: u32,
        labels_at: u16,
    ) {
        let friendly = self.exports[(r - 1) as usize].name;
        let mut p = compact(0);
        p.extend(self.header(0, next, children, friendly, script, mem));
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

/// Native class default: `Camera` is a native class whose `bOnlySpectator` is not in the
/// serialized defaults block. `GameInfo.StartMatch` restarts every non-spectator
/// `PlayerController` with no pawn, so every placed camera would spawn a spurious
/// `XIIIPlayerPawn`; the VM must supply the native default. A sibling class with the same
/// property keeps the zero default (the shim is keyed to the camera lineage).
#[test]
fn camera_native_default_makes_it_a_spectator() {
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let controller = b.reserve(0, 0, "PlayerController");
    let camera = b.reserve(0, 0, "Camera");
    let other = b.reserve(0, 0, "NotACamera");
    let spectator = b.reserve(IMP_BOOLPROP, controller, "bOnlySpectator");
    let is_player = b.reserve(IMP_BOOLPROP, controller, "bIsPlayer");
    b.prop(spectator, is_player, 0);
    b.prop(is_player, 0, 0);
    b.class(object, 0, 0, 0);
    b.class(controller, object, spectator, 0);
    b.class(camera, controller, 0, 0);
    b.class(other, controller, 0, 0);
    let pkg = ScriptPackage::load(
        "Test",
        b.build(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(pkg.errors.is_empty(), "{:?}", pkg.errors);
    let mut set = ScriptSet::new();
    set.add(pkg);
    let mut vm = Vm::new(&set, VmLimits::default());
    let cam = GlobalRef {
        package: 0,
        export: set.packages[0].export_by_path("Camera").unwrap(),
    };
    let other = GlobalRef {
        package: 0,
        export: set.packages[0].export_by_path("NotACamera").unwrap(),
    };
    let cam_layout = vm.class_layout(cam).unwrap();
    let other_layout = vm.class_layout(other).unwrap();
    let get =
        |l: &crate::vm::ClassLayout, n: &str| l.defaults[l.slot_by_name(n).unwrap().base].clone();
    assert_eq!(get(&cam_layout, "bOnlySpectator"), Value::Bool(true));
    assert_eq!(get(&other_layout, "bOnlySpectator"), Value::Bool(false));
}

/// UE2 component/default subobjects are per instance: an actor class's serialized defaults hold
/// a reference to a subobject export whose `Outer` is the class (for example
/// `xidcine.XIIIBreakingGlassEmitter.Emitters[0]` ->
/// `xidcine.XIIIBreakingGlassEmitter.XIIIBreakingGlassEmitterA`). Spawning must create a fresh
/// copy per actor, apply the subobject's own serialized template, and rewrite the property to
/// the instance so `emit.Emitters[0].StartVelocityRange = ...` resolves
/// (`xidcine.BreakableMover.InitializeEmitters` 0x006A).
#[test]
fn spawn_instantiates_a_class_default_subobject_per_instance() {
    let mut b = SpawnB::new();
    let holder = b.reserve(0, 0, "Holder");
    let comp = b.reserve(0, 0, "Comp");
    let value = b.reserve(IMP_INTPROP, comp, "Value");
    let comp_outer = b.reserve(IMP_OBJECTPROP, comp, "Outer");
    // `Range`/`RangeVector` structs and a `Size` struct property, to exercise the FRangeVector
    // template decode (the `XIIIBreakingGlassEmitterA.StartSizeRange` case).
    let range = b.reserve(IMP_STRUCT_CLASS, 0, "Range");
    let rmin = b.reserve(IMP_FLOATPROP, range, "min");
    let rmax = b.reserve(IMP_FLOATPROP, range, "max");
    b.prop(rmin, rmax, 0);
    b.prop(rmax, 0, 0);
    b.strukt(range, rmin);
    let rv = b.reserve(IMP_STRUCT_CLASS, 0, "RangeVector");
    let rvx = b.reserve(IMP_STRUCTPROP, rv, "x");
    let rvy = b.reserve(IMP_STRUCTPROP, rv, "y");
    let rvz = b.reserve(IMP_STRUCTPROP, rv, "z");
    b.prop_with(rvx, rvy, 0, &compact(range));
    b.prop_with(rvy, rvz, 0, &compact(range));
    b.prop_with(rvz, 0, 0, &compact(range));
    b.strukt(rv, rvx);
    let size = b.reserve(IMP_STRUCTPROP, comp, "Size");
    let rv_name = b.exports[(rv - 1) as usize].name;
    b.prop(value, comp_outer, 0);
    b.prop_with(comp_outer, size, 0, &compact(0));
    b.prop_with(size, 0, 0, &compact(rv));
    let comp_prop = b.reserve(IMP_OBJECTPROP, holder, "Comp");
    b.prop_with(comp_prop, 0, 0, &compact(0));
    // The subobject export: outer is the `Holder` class, its class is `Comp`.
    let default_comp = b.reserve(comp, holder, "DefaultComp");
    // Its serialized template sets `Value = 7` and `Size = ((1,10),(100,100),(100,100))`.
    let value_name = b.exports[(value - 1) as usize].name;
    let size_name = b.exports[(size - 1) as usize].name;
    let mut template = compact(value_name);
    template.push(0x22);
    template.extend(7i32.to_le_bytes());
    template.extend(compact(size_name));
    template.push(0x5A); // struct type, explicit u8 size
    template.extend(compact(rv_name));
    template.push(24);
    for r in [[1.0f32, 10.0], [100.0, 100.0], [100.0, 100.0]] {
        template.extend(r[0].to_le_bytes());
        template.extend(r[1].to_le_bytes());
    }
    template.extend(compact(0));
    b.set(default_comp, template);
    // `Holder`'s class default: `Comp = DefaultComp`.
    let comp_name = b.exports[(comp_prop - 1) as usize].name;
    b.class_with_object_default(holder, 0, comp_prop, comp_name, default_comp);
    b.class(comp, 0, value, 0);

    let pkg = ScriptPackage::load(
        "Test",
        b.build(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(pkg.errors.is_empty(), "{:?}", pkg.errors);
    let mut set = ScriptSet::new();
    set.add(pkg);
    let mut vm = Vm::new(&set, VmLimits::default());
    let holder_class = sg(&set, "Holder");

    let h1 = vm.spawn(holder_class, "H1").unwrap();
    let h2 = vm.spawn(holder_class, "H2").unwrap();
    let comp1 = match vm.get_property(h1, "Comp") {
        Some(Value::Object(Some(ObjRef::Instance(i)))) => *i,
        other => panic!("H1.Comp must be an instance, got {other:?}"),
    };
    let comp2 = match vm.get_property(h2, "Comp") {
        Some(Value::Object(Some(ObjRef::Instance(i)))) => *i,
        other => panic!("H2.Comp must be an instance, got {other:?}"),
    };
    assert_ne!(comp1, comp2, "each instance gets its own subobject copy");
    // The subobject's own serialized template was applied.
    assert_eq!(vm.get_property(comp1, "Value"), Some(&Value::Int(7)));
    assert_eq!(vm.get_property(comp2, "Value"), Some(&Value::Int(7)));
    // The `RangeVector` template property decoded into the script layout's nested `{min,max}`.
    let range = |min: f32, max: f32| {
        Value::Struct(vec![
            ("min".into(), Value::Float(min)),
            ("max".into(), Value::Float(max)),
        ])
    };
    let expected = Value::Struct(vec![
        ("x".into(), range(1.0, 10.0)),
        ("y".into(), range(100.0, 100.0)),
        ("z".into(), range(100.0, 100.0)),
    ]);
    assert_eq!(vm.get_property(comp1, "Size").cloned(), Some(expected));
    // The subobject's `Outer` is its owning actor.
    assert_eq!(
        vm.get_property(comp1, "Outer"),
        Some(&Value::Object(Some(ObjRef::Instance(h1))))
    );
    // A write through one copy does not alias the other (the BreakableMover write path).
    vm.set_property(comp1, "Value", 0, Value::Int(42));
    assert_eq!(vm.get_property(comp1, "Value"), Some(&Value::Int(42)));
    assert_eq!(vm.get_property(comp2, "Value"), Some(&Value::Int(7)));
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
fn actor_animation_view_reports_sequence_frame_and_looping() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(4, 1.0)));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    play_anim(&mut vm, a, "Walk", 1.0, 0);
    let view = vm.actor_animation(a).expect("live actor");
    assert_eq!(view.channels.len(), 1);
    let ch = &view.channels[0];
    assert_eq!(ch.channel, 0);
    assert_eq!(ch.sequence, "Walk");
    assert_eq!(ch.frames, 4);
    assert_eq!(ch.rate, 1.0);
    assert!(!ch.looping);
    assert!(ch.active);
    // The reported frame follows the VM's own playback position.
    vm.tick(0.5).unwrap();
    let view = vm.actor_animation(a).expect("live actor");
    assert_eq!(view.channels[0].frame, 0.5);
    // An actor with no animation yields a view with no channels (the host uses the bind pose).
    let b = vm.spawn(sg(&set, "Actor"), "B").unwrap();
    let view = vm.actor_animation(b).expect("live actor");
    assert!(view.channels.is_empty());
    // The `Mesh` accessor is `None` when the actor has no mesh.
    assert_eq!(vm.mesh_object(a), None);
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
fn play_anim_on_a_mesh_less_actor_is_a_noop() {
    // UE2 `AActor::PlayAnim` returns immediately when `Mesh == NULL` (Engine.dll
    // `?PlayAnim@AActor` RVA 0xDF8B0 tests `this+0x138` and jumps to the epilogue), so an
    // actor with no animation source must not error on `PlayAnim`.
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(RecordingAnim {
        queried: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        answer: |_| Ok(None),
    }));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    assert!(
        vm.animation_sources(a).is_empty(),
        "the fixture actor has no Mesh/link"
    );
    play_anim(&mut vm, a, "Select", 1.0, 0);
    assert!(
        !vm.anim_channel_active(a, 0),
        "a mesh-less PlayAnim creates no channel"
    );
    assert!(
        vm.trace
            .iter()
            .any(|e| matches!(&e.kind, TraceKind::Note(s) if s.contains("no mesh"))),
        "the mesh-less no-op is reported, not silent"
    );
}

#[test]
fn play_anim_with_an_unknown_sequence_is_a_ue2_noop() {
    // UE2 `AActor::PlayAnim`/`LoopAnim` resolve the name in the mesh's animation set and play
    // nothing when it is absent. The shipped maps rely on this: `xidcine.Cine2.PostBeginPlay`
    // calls `LoopAnim(DefaultAnim)` with values ("Wait", "acqiesce") no source of the actor
    // carries. A missing name is a visible no-op, not a script failure; a *decode* failure is
    // still an explicit `AnimationDataError` (see `animation_decode_error_is_explicit`).
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(RecordingAnim {
        queried: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())),
        answer: |_| Ok(None),
    }));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    // Link a source so the actor is not mesh-less; the sequence is still unknown.
    let mut args = [Value::Object(Some(ObjRef::Static(GlobalRef {
        package: 0,
        export: 0,
    })))];
    try_native(&mut vm, "Engine.Actor.LinkSkelAnim", a, &[false], &mut args).unwrap();
    let mut args = [
        Value::Name("Wait".into()),
        Value::Float(1.0),
        Value::Float(0.0),
        Value::Int(0),
    ];
    let r = try_native(
        &mut vm,
        "Engine.Actor.LoopAnim",
        a,
        &[false, false, false, false],
        &mut args,
    );
    assert!(r.is_ok(), "an unknown sequence is a no-op, got {r:?}");
    assert!(!vm.anim_channel_active(a, 0), "no channel was started");
    assert!(
        vm.trace
            .iter()
            .any(|e| matches!(&e.kind, TraceKind::Note(s) if s.contains("plays nothing"))),
        "the no-op is reported, not silent"
    );
}

#[test]
fn dynamic_load_object_accepts_a_native_subclass_and_rejects_others() {
    // Engine classes with no decoded `Core.Class` export (`Mesh`/`SkeletalMesh`) still need the
    // subclass test: `Weapon.PostBeginPlay` loads a `SkeletalMesh` with `class'Engine.Mesh'`.
    use crate::registry::native_class_is_a;
    assert!(native_class_is_a("Engine.SkeletalMesh", "Mesh"));
    assert!(native_class_is_a("Engine.StaticMesh", "Mesh"));
    assert!(native_class_is_a("Engine.Mesh", "Engine.Mesh"));
    // Unrelated classes and the reverse direction are still rejected.
    assert!(!native_class_is_a("Engine.Texture", "Mesh"));
    assert!(!native_class_is_a("Core.Class", "Mesh"));
    assert!(!native_class_is_a("Engine.Mesh", "SkeletalMesh"));
}

#[test]
fn levelinfo_get_local_url_returns_the_configured_url() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let mut args = [];
    let r = try_native(&mut vm, "Engine.LevelInfo.GetLocalURL", a, &[], &mut args).unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Str(String::new())));
    let url = "Plage00?Name=XIII?Class=XIII.XIIIPlayerPawn?Team=255";
    let opts = "?Name=XIII?Class=XIII.XIIIPlayerPawn?Team=255";
    vm.set_local_url(url, opts);
    let r = try_native(&mut vm, "Engine.LevelInfo.GetLocalURL", a, &[], &mut args).unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Str(url.to_owned())));
    assert_eq!(vm.url_options(), opts);
    // `GetAddressURL` is the separate runtime-configured `Host:Port`, not the local URL.
    vm.set_address_url(":7777");
    let r = try_native(&mut vm, "Engine.LevelInfo.GetAddressURL", a, &[], &mut args).unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Str(":7777".into())));
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

// ---------------------------------------------------------------------------------------
// Canvas draw-command recording (item10)

/// A one-class package with a `Canvas` carrying the draw properties the natives use.
fn canvas_fixture() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let canvas = b.reserve(0, 0, "Canvas");
    let curx = b.reserve(IMP_FLOATPROP, canvas, "CurX");
    let cury = b.reserve(IMP_FLOATPROP, canvas, "CurY");
    let orgx = b.reserve(IMP_FLOATPROP, canvas, "OrgX");
    let orgy = b.reserve(IMP_FLOATPROP, canvas, "OrgY");
    let clipx = b.reserve(IMP_FLOATPROP, canvas, "ClipX");
    let clipy = b.reserve(IMP_FLOATPROP, canvas, "ClipY");
    let drawcolor = b.reserve(IMP_FLOATPROP, canvas, "DrawColor");
    let font = b.reserve(IMP_NAMEPROP, canvas, "Font");
    let style = b.reserve(IMP_INTPROP, canvas, "Style");
    b.prop(curx, cury, 0);
    b.prop(cury, orgx, 0);
    b.prop(orgx, orgy, 0);
    b.prop(orgy, clipx, 0);
    b.prop(clipx, clipy, 0);
    b.prop(clipy, drawcolor, 0);
    b.prop(drawcolor, font, 0);
    b.prop(font, style, 0);
    b.prop(style, 0, 0);
    b.class(object, 0, 0);
    b.class(canvas, object, curx);
    b.build()
}

/// A 4-unit-wide, 6-unit-tall monospace "font".
struct TinyFonts;
impl crate::canvas::CanvasFonts for TinyFonts {
    fn measure(&self, font: &str, text: &str) -> Option<(f32, f32)> {
        (font == "Tiny").then(|| (text.chars().count() as f32 * 4.0, 6.0))
    }
}

/// Synthetic Canvas natives: cursor movement, text measurement with a tiny font, clip capture,
/// and the null-material tile -> rect path.
#[test]
fn canvas_natives_record_commands_with_cursor_measurement_and_clip() {
    let set = set_of(canvas_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let canvas = vm.spawn(g(&set, "Canvas"), "Canvas0").unwrap();
    vm.set_canvas_fonts(Box::new(TinyFonts));
    assert!(vm.set_property(canvas, "ClipX", 0, Value::Float(320.0)));
    assert!(vm.set_property(canvas, "ClipY", 0, Value::Float(200.0)));
    assert!(vm.set_property(canvas, "Font", 0, Value::Name("Tiny".into())));
    vm.set_property(
        canvas,
        "DrawColor",
        0,
        Value::Struct(vec![
            ("b".into(), Value::Byte(10)),
            ("g".into(), Value::Byte(20)),
            ("r".into(), Value::Byte(30)),
            ("a".into(), Value::Byte(255)),
        ]),
    );
    vm.set_property(canvas, "Style", 0, Value::Byte(5));

    // Cursor movement then text at the pen.
    call_native(
        &mut vm,
        "Engine.Canvas.SetPos",
        canvas,
        &[],
        &mut [Value::Float(10.0), Value::Float(20.0)],
    );
    call_native(
        &mut vm,
        "Engine.Canvas.DrawText",
        canvas,
        &[],
        &mut [Value::Str("abcd".into()), Value::Bool(false)],
    );
    // StrLen writes both out floats.
    let mut args = [
        Value::Str("abcd".into()),
        Value::Float(0.0),
        Value::Float(0.0),
    ];
    call_native(
        &mut vm,
        "Engine.Canvas.StrLen",
        canvas,
        &[false, true, true],
        &mut args,
    );
    assert_eq!(args[1], Value::Float(16.0));
    assert_eq!(args[2], Value::Float(6.0));

    let cmds = vm.drain_canvas();
    match cmds.as_slice() {
        [
            crate::canvas::DrawCommand::Text {
                text,
                x,
                y,
                font,
                color,
                clip,
                style,
                clipped,
                ..
            },
        ] => {
            assert_eq!(text, "abcd");
            assert_eq!((*x, *y), (10.0, 20.0));
            assert_eq!(font.as_deref(), Some("Tiny"));
            assert_eq!(*color, [30, 20, 10, 255]);
            assert_eq!(*clip, [320.0, 200.0]);
            assert_eq!(*style, 5);
            assert!(!clipped);
        }
        other => panic!("{other:?}"),
    }
    // Cursor advanced by the measured width and stayed on the same line (CR=false).
    assert_eq!(vm.get_property(canvas, "CurX"), Some(&Value::Float(26.0)));
    assert_eq!(vm.get_property(canvas, "CurY"), Some(&Value::Float(20.0)));

    // A null-material tile becomes a Rect and advances CurX by XL.
    call_native(
        &mut vm,
        "Engine.Canvas.SetPos",
        canvas,
        &[],
        &mut [Value::Float(0.0), Value::Float(0.0)],
    );
    call_native(
        &mut vm,
        "Engine.Canvas.DrawTile",
        canvas,
        &[],
        &mut [
            Value::Object(None),
            Value::Float(8.0),
            Value::Float(4.0),
            Value::Float(0.0),
            Value::Float(0.0),
            Value::Float(1.0),
            Value::Float(1.0),
        ],
    );
    let cmds = vm.drain_canvas();
    assert!(
        matches!(
            cmds.as_slice(),
            [crate::canvas::DrawCommand::Rect {
                xl: 8.0,
                yl: 4.0,
                ..
            }]
        ),
        "{cmds:?}"
    );
    assert_eq!(vm.get_property(canvas, "CurX"), Some(&Value::Float(8.0)));
    // A missing font records a visible note instead of silently measuring zero.
    vm.set_property(canvas, "Font", 0, Value::Object(None));
    let mut args = [Value::Str("x".into()), Value::Float(9.0), Value::Float(9.0)];
    call_native(
        &mut vm,
        "Engine.Canvas.StrLen",
        canvas,
        &[false, true, true],
        &mut args,
    );
    assert!(
        vm.trace
            .iter()
            .any(|t| matches!(&t.kind, TraceKind::Note(n) if n.contains("no decoded font")))
    );
}

/// A synthetic package with an `ExtRef.GetExt` function whose body is an `ObjectConst` into an
/// external import `Ext.Snd` of class `Engine.Sound` (the package `Ext` is never loaded as a
/// script package).
fn external_ref_set() -> ScriptSet {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let extref = b.reserve(0, 0, "ExtRef");
    let getext = b.reserve(IMP_FUNCTION, extref, "GetExt");
    let ret = b.reserve(IMP_OBJPROP, getext, "ReturnValue");
    b.prop_with(ret, 0, pf::PARM | pf::RETURN_PARM, &compact(0));
    let raw = b.add_external("Ext", "Sound", "Snd");
    let mut code = vec![0x04, 0x20]; // return ObjectConst(<external import>)
    code.extend(compact(raw));
    b.func(getext, 0, ret, &code, 6, 0, ff::DEFINED);
    b.class(object, 0, 0);
    b.class(extref, object, getext);
    set_of(b.build())
}

#[test]
fn external_lookup_reports_found_missing_and_unknown() {
    let mut set = ScriptSet::new();
    // A registered non-script package with the export path `Snd`.
    let bytes = build_package(
        &["None", "Core", "Package", "Snd"],
        &[(1, 2, 0, 1)],
        &[Exp {
            class: -1,
            outer: 0,
            name: 3,
            flags: 0,
            payload: Vec::new(),
        }],
    );
    set.add_external_package("Ext", &bytes, &Limits::default())
        .expect("external package parses");
    use crate::linker::ExternalLookup;
    assert!(
        matches!(set.external_lookup("Ext.Snd"), ExternalLookup::Found(_)),
        "{:?}",
        set.external_lookup("Ext.Snd")
    );
    assert_eq!(
        set.external_lookup("Ext.Missing"),
        ExternalLookup::MissingExport
    );
    set.add_missing_external("Gone");
    assert_eq!(
        set.external_lookup("Gone.Thing"),
        ExternalLookup::MissingPackage
    );
    // A package never registered is unknown (the VM keeps a lazy external object).
    assert_eq!(set.external_lookup("Never.Thing"), ExternalLookup::Unknown);
}

#[test]
fn object_constant_into_a_non_script_package_keeps_path_and_import_class() {
    let set = external_ref_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let id = vm.spawn(g(&set, "ExtRef"), "R").unwrap();
    vm.set_active(id, true);
    let func = g(&set, "ExtRef.GetExt");
    let v = vm.call_function(func, id, Vec::new()).unwrap();
    let Value::Object(Some(ObjRef::External(x))) = v else {
        panic!("expected an external object value, got {v:?}");
    };
    assert_eq!(
        vm.external_path(&ObjRef::External(x)).as_deref(),
        Some("Ext.Snd")
    );
    // The class comes from the referencing import table (`Engine.Sound`).
    assert_eq!(
        vm.external_object(x).unwrap().class.as_deref(),
        Some("Engine.Sound")
    );
    assert!(vm.external_is_a(x, "Sound"));
    assert!(vm.external_is_a(x, "Engine.Sound"));
    assert!(!vm.external_is_a(x, "Texture"));
    // Equality by path: a second evaluation interns to the same id.
    let v2 = vm.call_function(func, id, Vec::new()).unwrap();
    assert_eq!(v, v2, "external refs with the same path must compare equal");
}

#[test]
fn external_reference_to_a_missing_package_is_an_explicit_error() {
    let mut set = external_ref_set();
    set.add_missing_external("Ext");
    let mut vm = Vm::new(&set, VmLimits::default());
    let id = vm.spawn(g(&set, "ExtRef"), "R").unwrap();
    vm.set_active(id, true);
    let func = g(&set, "ExtRef.GetExt");
    let err = vm
        .call_function(func, id, Vec::new())
        .expect_err("a missing external package must not resolve to None");
    match err.kind {
        VmErrorKind::UnsupportedValue { desc } => {
            assert!(desc.contains("Ext.Snd"), "{desc}");
            assert!(desc.contains("package not found"), "{desc}");
        }
        other => panic!("expected UnsupportedValue, got {other:?}"),
    }
}

/// Synthetic package with `Object` <- `Inventory` <- `Pawn` and a `Pawn.AddInventory` script that
/// reproduces the decoded UE2 chain (walk the `Inventory` links, reject a duplicate, link the new
/// item at the tail). No proprietary data.
fn inventory_package() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let inventory = b.reserve(0, 0, "Inventory");
    let pawn = b.reserve(0, 0, "Pawn");
    // `Inventory` properties (inherited by `Pawn`): the link and the owner.
    let inv_inv = b.reserve(IMP_OBJPROP, inventory, "Inventory");
    let inv_owner = b.reserve(IMP_OBJPROP, inventory, "Owner");
    b.prop_with(inv_inv, inv_owner, 0, &compact(0));
    b.prop_with(inv_owner, 0, 0, &compact(0));
    // Native object operators the script calls (declared so `resolve_native_index` finds them).
    let native_op = ff::FINAL | ff::NATIVE | ff::OPERATOR | ff::STATIC;
    let neq = b.reserve(IMP_FUNCTION, object, "NotEqual_ObjectObject");
    let eq = b.reserve(IMP_FUNCTION, object, "EqualEqual_ObjectObject");
    let neq_a = b.reserve(IMP_OBJPROP, neq, "A");
    let neq_b = b.reserve(IMP_OBJPROP, neq, "B");
    let neq_r = b.reserve(IMP_OBJPROP, neq, "ReturnValue");
    b.prop_with(neq_a, neq_b, pf::PARM, &compact(0));
    b.prop_with(neq_b, neq_r, pf::PARM, &compact(0));
    b.prop_with(neq_r, 0, pf::PARM | pf::RETURN_PARM, &compact(0));
    b.func(neq, eq, neq_a, &[], 0, 119, native_op);
    let eq_a = b.reserve(IMP_OBJPROP, eq, "A");
    let eq_b = b.reserve(IMP_OBJPROP, eq, "B");
    let eq_r = b.reserve(IMP_OBJPROP, eq, "ReturnValue");
    b.prop_with(eq_a, eq_b, pf::PARM, &compact(0));
    b.prop_with(eq_b, eq_r, pf::PARM, &compact(0));
    b.prop_with(eq_r, 0, pf::PARM | pf::RETURN_PARM, &compact(0));
    b.func(eq, 0, eq_a, &[], 0, 114, native_op);
    // `Pawn.AddInventory(Inventory NewItem) -> int` with locals `Inv`, `Last`.
    let add = b.reserve(IMP_FUNCTION, pawn, "AddInventory");
    let newitem = b.reserve(IMP_OBJPROP, add, "NewItem");
    let ret = b.reserve(IMP_INTPROP, add, "ReturnValue");
    let inv = b.reserve(IMP_OBJPROP, add, "Inv");
    let last = b.reserve(IMP_OBJPROP, add, "Last");
    b.prop_with(newitem, ret, pf::PARM, &compact(0));
    b.prop(ret, inv, pf::PARM | pf::RETURN_PARM);
    b.prop_with(inv, last, 0, &compact(0));
    b.prop_with(last, 0, 0, &compact(0));
    let ri = inv as u8; // local `Inv`
    let rn = newitem as u8; // local/param `NewItem`
    let rl = last as u8; // local `Last`
    let pi = inv_inv as u8; // the `Inventory` link property
    #[rustfmt::skip]
    let code: Vec<u8> = vec![
        0x0F, 0x00, rl, 0x17,                                                   // 0000 Last = self
        0x0F, 0x00, ri, 0x01, pi,                                               // 0007 Inv = self.Inventory
        0x07, 0x50, 0x00, 0x77, 0x00, ri, 0x2A, 0x16,                          // 0012 if !(Inv != None) goto 0x50 (END)
        0x07, 0x2E, 0x00, 0x72, 0x00, ri, 0x00, rn, 0x16,                      // 001D if Inv == NewItem goto 0x2E (ELSE)
        0x04, 0x28,                                                             // 002C return false
        0x0F, 0x00, rl, 0x00, ri,                                               // 002E Last = Inv
        0x0F, 0x00, ri, 0x19, 0x00, ri, 0xFF, 0xFF, 0x00, 0x01, pi,           // 0039 Inv = Inv.Inventory
        0x06, 0x12, 0x00,                                                       // 004D goto 0x12 (LOOP)
        0x0F, 0x19, 0x00, rl, 0xFF, 0xFF, 0x00, 0x01, pi, 0x00, rn,           // 0050 Last.Inventory = NewItem
        0x04, 0x27,                                                             // 0064 return true
    ];
    b.func(add, 0, newitem, &code, 102, 0, ff::DEFINED);
    b.class(object, 0, neq);
    b.class(inventory, object, inv_inv);
    b.class(pawn, inventory, add);
    b.build()
}

#[test]
fn synthetic_add_inventory_links_the_chain_and_rejects_duplicates() {
    let set = set_of(inventory_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    let pawn = vm.spawn(g(&set, "Pawn"), "P").unwrap();
    let a = vm.spawn(g(&set, "Inventory"), "A").unwrap();
    let b_item = vm.spawn(g(&set, "Inventory"), "B").unwrap();
    vm.set_active(pawn, true);
    let add = g(&set, "Pawn.AddInventory");
    let call = |vm: &mut Vm, item: ObjectId| {
        vm.call_function(add, pawn, vec![Value::Object(Some(ObjRef::Instance(item)))])
            .unwrap()
    };
    assert_ne!(
        call(&mut vm, a),
        Value::Bool(false),
        "first item must be added"
    );
    assert_eq!(
        vm.get_property(pawn, "Inventory"),
        Some(&Value::Object(Some(ObjRef::Instance(a)))),
        "the pawn must link the first item"
    );
    assert_ne!(
        call(&mut vm, b_item),
        Value::Bool(false),
        "second item must be added"
    );
    assert_eq!(
        vm.get_property(a, "Inventory"),
        Some(&Value::Object(Some(ObjRef::Instance(b_item)))),
        "the chain must link A -> B"
    );
    // A duplicate must be rejected and leave the chain unchanged.
    assert_eq!(
        call(&mut vm, a),
        Value::Bool(false),
        "a duplicate must be refused"
    );
    assert_eq!(
        vm.get_property(pawn, "Inventory"),
        Some(&Value::Object(Some(ObjRef::Instance(a))))
    );
    assert_eq!(
        vm.get_property(a, "Inventory"),
        Some(&Value::Object(Some(ObjRef::Instance(b_item))))
    );
    assert_eq!(
        vm.get_property(b_item, "Inventory"),
        Some(&Value::Object(None)),
        "the tail's link stays None"
    );
}

/// Synthetic package with a `Pickup` class carrying `Location` (`Core.Struct` `Vector`),
/// `CollisionHeight` (`float`) and `bCollideWorld` (`bool`) — the fields `settle_pickups` reads.
fn settle_package() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    // A zero-size `Vector` export (class `Core`, so it is not decoded as a script object); the
    // struct property's `Struct` reference only needs the object *name* to type it as a vector.
    let vector = b.reserve(IMP_CORE, 0, "Vector");
    let pickup = b.reserve(0, 0, "Pickup");
    let loc = b.reserve(B_STRUCTPROP, pickup, "Location");
    let height = b.reserve(IMP_FLOATPROP, loc, "CollisionHeight");
    let collide = b.reserve(B_BOOLPROP, height, "bCollideWorld");
    b.prop_with(loc, height, 0, &compact(vector));
    b.prop(height, collide, 0);
    b.prop(collide, 0, 0);
    b.class(object, 0, 0);
    b.class(actor, object, 0);
    b.class(pickup, actor, loc);
    b.build()
}

#[test]
fn settle_pickups_rests_a_placed_pickup_on_its_support() {
    let set = set_of(settle_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(crate::physics::FlatPhysics::new(100.0)));
    let p = vm.spawn(g(&set, "Pickup"), "Key").unwrap();
    // The decoded map `Location` is the cylinder base: `z == floor` -> embedded; the cylinder
    // centre must become `floor + CollisionHeight`.
    vm.set_property(p, "Location", 0, Value::Vector([10.0, 20.0, 100.0]));
    vm.set_property(p, "CollisionHeight", 0, Value::Float(8.0));
    vm.set_property(p, "bCollideWorld", 0, Value::Bool(true));
    assert_eq!(vm.settle_pickups(), 1, "the embedded pickup must be moved");
    assert_eq!(
        vm.vector_prop(p, "Location").unwrap(),
        [10.0, 20.0, 108.0],
        "the cylinder centre rests on the floor + CollisionHeight"
    );
    // Idempotent once resting.
    assert_eq!(vm.settle_pickups(), 0);
    // A pickup with `bCollideWorld=false` is left alone.
    let q = vm.spawn(g(&set, "Pickup"), "Floating").unwrap();
    vm.set_property(q, "Location", 0, Value::Vector([0.0, 0.0, 100.0]));
    vm.set_property(q, "CollisionHeight", 0, Value::Float(8.0));
    vm.set_property(q, "bCollideWorld", 0, Value::Bool(false));
    assert_eq!(vm.settle_pickups(), 0);
    assert_eq!(vm.vector_prop(q, "Location").unwrap(), [0.0, 0.0, 100.0]);
}

#[test]
fn external_reference_to_a_registered_missing_export_is_an_explicit_error() {
    let mut set = external_ref_set();
    // A registered package that does not contain `Snd`.
    let bytes = build_package(
        &["None", "Core", "Package", "Other"],
        &[(1, 2, 0, 1)],
        &[Exp {
            class: -1,
            outer: 0,
            name: 3,
            flags: 0,
            payload: Vec::new(),
        }],
    );
    set.add_external_package("Ext", &bytes, &Limits::default())
        .unwrap();
    let mut vm = Vm::new(&set, VmLimits::default());
    let id = vm.spawn(g(&set, "ExtRef"), "R").unwrap();
    vm.set_active(id, true);
    let err = vm
        .call_function(g(&set, "ExtRef.GetExt"), id, Vec::new())
        .expect_err("a missing external export must not resolve to None");
    match err.kind {
        VmErrorKind::UnsupportedValue { desc } => {
            assert!(desc.contains("export not found"), "{desc}");
        }
        other => panic!("expected UnsupportedValue, got {other:?}"),
    }
}

// ---------------------------------------------------------------------------------------
// item3j: campaign missing natives

fn vec_result(o: NativeOutcome) -> [f32; 3] {
    match o {
        NativeOutcome::Value(Value::Vector(v)) => v,
        other => panic!("expected vector, got {other:?}"),
    }
}

#[test]
fn campaign_operator_natives_match_ue2_semantics() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Object"), "O").unwrap();

    let mut a = [Value::Vector([2.0, 3.0, 4.0]), Value::Float(2.0)];
    assert_eq!(
        vec_result(call_native(
            &mut vm,
            "Object.Multiply_VectorFloat",
            o,
            &[false, false],
            &mut a
        )),
        [4.0, 6.0, 8.0]
    );
    // A zero scale is a real value, not an "omitted" marker.
    let mut a = [Value::Vector([1.0, -2.0, 3.0]), Value::Float(0.0)];
    assert_eq!(
        vec_result(call_native(
            &mut vm,
            "Object.Multiply_VectorFloat",
            o,
            &[false, false],
            &mut a
        )),
        [0.0, 0.0, 0.0]
    );

    let mut a = [Value::Vector([2.0, 4.0, 6.0]), Value::Float(2.0)];
    assert_eq!(
        vec_result(call_native(
            &mut vm,
            "Object.Divide_VectorFloat",
            o,
            &[false, false],
            &mut a
        )),
        [1.0, 2.0, 3.0]
    );
    // UE2 does not guard division by zero: IEEE infinity, not a silent clamp.
    let mut a = [Value::Vector([1.0, 1.0, 1.0]), Value::Float(0.0)];
    assert!(
        vec_result(call_native(
            &mut vm,
            "Object.Divide_VectorFloat",
            o,
            &[false, false],
            &mut a
        ))[0]
            .is_infinite()
    );

    let mut a = [
        Value::Vector([1.0, 2.0, 3.0]),
        Value::Vector([1.0, 2.0, 3.0]),
    ];
    assert!(bool_result(call_native(
        &mut vm,
        "Object.EqualEqual_VectorVector",
        o,
        &[false, false],
        &mut a
    )));
    let mut a = [
        Value::Vector([1.0, 2.0, 3.0]),
        Value::Vector([1.0, 2.0, 4.0]),
    ];
    assert!(!bool_result(call_native(
        &mut vm,
        "Object.EqualEqual_VectorVector",
        o,
        &[false, false],
        &mut a
    )));
}

#[test]
fn set_bone_scale_per_axis_defaults_omitted_axes_to_one() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    // `SetBoneScalePerAxis(16, 0.5, nothing, nothing, 'X Blink')` (xidcine.Cine2.CineInit.Timer).
    let mut args = [
        Value::Int(16),
        Value::Float(0.5),
        Value::Float(0.0),
        Value::Float(0.0),
        Value::Name("X Blink".into()),
    ];
    call_native(
        &mut vm,
        "Actor.SetBoneScalePerAxis",
        a,
        &[false, false, true, true, false],
        &mut args,
    );
    let bs = vm.bone_state(a).expect("bone state");
    assert_eq!(bs.scales.len(), 1);
    assert_eq!(bs.scales[0].slot, 16);
    assert_eq!(bs.scales[0].scale, [0.5, 1.0, 1.0]);
    assert_eq!(bs.scales[0].bone, "X Blink");
}

#[test]
fn voice_and_onomatopoeia_natives_emit_presentation_events() {
    use crate::events::PresentationEvent;
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    call_native(&mut vm, "Actor.StopVoice", a, &[], &mut []);
    // A null `Sound` does not play.
    let mut args = [Value::Object(None), Value::Int(1), Value::Int(2)];
    call_native(
        &mut vm,
        "Actor.PlaySndPNJOno",
        a,
        &[false, false, false],
        &mut args,
    );
    // A real sound object plays, carrying CodeMesh/Timbre.
    let mut args = [
        Value::Object(Some(ObjRef::Instance(a))),
        Value::Int(3),
        Value::Int(4),
    ];
    call_native(
        &mut vm,
        "Actor.PlaySndPNJOno",
        a,
        &[false, false, false],
        &mut args,
    );
    let mut args = [Value::Object(Some(ObjRef::Instance(a)))];
    call_native(&mut vm, "Actor.StopSound", a, &[false], &mut args);

    let events = vm.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PresentationEvent::StopVoice { .. }))
    );
    let pnjo: Vec<_> = events
        .iter()
        .filter(|e| matches!(e, PresentationEvent::PlaySndPNJOno { .. }))
        .collect();
    assert_eq!(pnjo.len(), 1, "null Sound must not emit: {events:?}");
    match pnjo[0] {
        PresentationEvent::PlaySndPNJOno {
            code_mesh, timbre, ..
        } => {
            assert_eq!((*code_mesh, *timbre), (3, 4));
        }
        _ => unreachable!(),
    }
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PresentationEvent::StopSound { .. }))
    );
}

#[test]
fn client_travel_records_a_host_travel_request() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let c = vm
        .spawn(sg(&set, "Actor"), "XIIIPlayerController0")
        .unwrap();
    let mut a = [
        Value::Str("Plage01.unr".into()),
        Value::Byte(2),
        Value::Bool(true),
    ];
    let out = call_native(
        &mut vm,
        "PlayerController.ClientTravel",
        c,
        &[false, false, false],
        &mut a,
    );
    assert!(matches!(out, NativeOutcome::Value(Value::Void)));
    let req = vm.take_travel_request().expect("travel request");
    assert_eq!(req.url, "Plage01.unr");
    assert_eq!(req.mode, 2);
    assert!(req.items);
    assert_eq!(req.actor, "XIIIPlayerController0");
    assert!(matches!(req.source, TravelSource::ClientTravel));
    assert!(
        vm.take_travel_request().is_none(),
        "the request is consumed once"
    );
    // The request is also queued as a typed presentation event for `--events`/reporting.
    let events = vm.drain_events();
    assert!(
        events
            .iter()
            .any(|e| matches!(e, PresentationEvent::TravelRequest(_))),
        "a TravelRequest presentation event must be queued: {events:?}"
    );
}

#[test]
fn console_command_implements_campaign_commands_and_logs_unknown() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let c = vm.spawn(sg(&set, "Actor"), "PC").unwrap();
    let mut a = [Value::Str("GETPING".into())];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "PlayerController.ConsoleCommand",
            c,
            &[false],
            &mut a
        )),
        "0"
    );
    let before = vm.trace.len();
    let mut a = [Value::Str("MadeUpCommand 1 2".into())];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "PlayerController.ConsoleCommand",
            c,
            &[false],
            &mut a
        )),
        ""
    );
    assert!(
        vm.trace.len() > before,
        "an unknown console command must be logged"
    );
}

#[test]
fn auto_position_snaps_to_the_floor_and_adds_altitude() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    // The synthetic Actor has no `fAltitude`, so the altitude is 0; the trace result is the test.
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([-1000.0, -1000.0, 100.0], [1000.0, 1000.0, 110.0]),
    ));
    let a = spawn_at(&mut vm, &set, "Actor", "Pos", [0.0, 0.0, 500.0]);
    call_native(&mut vm, "PositionInfo.AutoPosition", a, &[], &mut []);
    let z = vm.vector_prop(a, "Location").unwrap()[2];
    assert!((z - 100.0).abs() < 1.0, "Location.z {z} not on the floor");
}

#[test]
fn find_best_path_toward_returns_true_and_fills_route_cache() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav0", [0.0, 0.0, 0.0]);
    let n1 = spawn_at(&mut vm, &set, "Actor", "Nav1", [500.0, 0.0, 0.0]);
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav2", [1000.0, 0.0, 0.0]);
    let (ctrl, _pawn) = nav_actor_pair(&mut vm, &set, 40.0, 80.0, 100.0);
    vm.set_navigation(Box::new(MockNav::line()));
    let target = spawn_at(&mut vm, &set, "Actor", "T", [1000.0, 0.0, 0.0]);
    let mut args = [
        Value::Object(Some(ObjRef::Instance(target))),
        Value::Float(70.0),
        Value::Float(160.0),
    ];
    assert!(bool_result(call_native(
        &mut vm,
        "IAController.FindBestPathToward",
        ctrl,
        &[false, false, false],
        &mut args
    )));
    // RouteCache drops the node the pawn stands on, so the first move target is Nav1.
    assert_eq!(
        route_cache_elem(&vm, ctrl, 0),
        Value::Object(Some(ObjRef::Instance(n1)))
    );
    // Desired None -> false, no path.
    let mut args = [Value::Object(None), Value::Float(70.0), Value::Float(160.0)];
    assert!(!bool_result(call_native(
        &mut vm,
        "IAController.FindBestPathToward",
        ctrl,
        &[false, false, false],
        &mut args
    )));
}

/// A minimal `PlayerController`/`Pawn` tree for the `PlayerCanSeeMe` line-of-sight test.
fn vision_fixture() -> Vec<u8> {
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let pc = b.reserve(0, 0, "PlayerController");
    let pawn_cls = b.reserve(0, 0, "Pawn");
    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let eye = b.reserve(IMP_FLOATPROP, actor, "BaseEyeHeight");
    let pawn_prop = b.reserve(IMP_OBJECTPROP, pc, "Pawn");
    let vector_extra = compact(IMP_STRUCT);
    let object_extra = compact(0);
    b.prop_with(location, eye, 0, &vector_extra);
    b.prop(eye, 0, 0);
    b.prop_with(pawn_prop, 0, 0, &object_extra);
    b.class(object, 0, 0, 0);
    b.class(actor, object, location, 0);
    b.class(pc, actor, pawn_prop, 0);
    b.class(pawn_cls, actor, 0, 0);
    b.build()
}

#[test]
fn player_can_see_me_uses_a_player_line_of_sight() {
    let pkg = ScriptPackage::load(
        "Test",
        vision_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(pkg.errors.is_empty(), "{:?}", pkg.errors);
    let mut set = ScriptSet::new();
    set.add(pkg);
    let g = |path: &str| GlobalRef {
        package: 0,
        export: set.packages[0].export_by_path(path).unwrap(),
    };
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let target = vm.spawn(g("Actor"), "Target").unwrap();
    vm.set_property(target, "Location", 0, Value::Vector([500.0, 0.0, 100.0]));
    let pc = vm.spawn(g("PlayerController"), "PC").unwrap();
    let pawn = vm.spawn(g("Pawn"), "P").unwrap();
    vm.set_property(pawn, "Location", 0, Value::Vector([0.0, 0.0, 100.0]));
    vm.set_property(pawn, "BaseEyeHeight", 0, Value::Float(60.0));
    vm.set_property(pc, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));

    assert!(bool_result(call_native(
        &mut vm,
        "Actor.PlayerCanSeeMe",
        target,
        &[],
        &mut []
    )));
    // A wall between the eye and the target blocks the view.
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([250.0, -100.0, 0.0], [260.0, 100.0, 200.0]),
    ));
    assert!(!bool_result(call_native(
        &mut vm,
        "Actor.PlayerCanSeeMe",
        target,
        &[],
        &mut []
    )));
    // No possessed pawn -> no viewer.
    vm.set_property(pc, "Pawn", 0, Value::Object(None));
    assert!(!bool_result(call_native(
        &mut vm,
        "Actor.PlayerCanSeeMe",
        target,
        &[],
        &mut []
    )));
}

// ---------------------------------------------------------------------------------------
// item14: synthetic combat tests (no proprietary data)

/// Synthetic package with `Object` <- `Actor` carrying the collision fields the actor trace
/// reads (`Location`, `CollisionRadius`, `CollisionHeight`, `bBlockZeroExtentTraces`). The two
/// integer native operators declared under `Object` are the registry's real indices (147 `-`,
/// 152 `<=`).
fn damage_package() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let pawn = b.reserve(0, 0, "Pawn");
    let native_op = ff::FINAL | ff::NATIVE | ff::OPERATOR | ff::STATIC;
    // Native integer operators.
    let sub = b.reserve(IMP_FUNCTION, object, "Subtract_IntInt");
    let le = b.reserve(IMP_FUNCTION, object, "LessEqual_IntInt");
    let sub_a = b.reserve(IMP_INTPROP, sub, "A");
    let sub_b = b.reserve(IMP_INTPROP, sub, "B");
    let sub_r = b.reserve(IMP_INTPROP, sub, "ReturnValue");
    b.prop(sub_a, sub_b, pf::PARM);
    b.prop(sub_b, sub_r, pf::PARM);
    b.prop(sub_r, 0, pf::PARM | pf::RETURN_PARM);
    b.func(sub, le, sub_a, &[], 0, 147, native_op);
    let le_a = b.reserve(IMP_INTPROP, le, "A");
    let le_b = b.reserve(IMP_INTPROP, le, "B");
    let le_r = b.reserve(IMP_INTPROP, le, "ReturnValue");
    b.prop(le_a, le_b, pf::PARM);
    b.prop(le_b, le_r, pf::PARM);
    b.prop(le_r, 0, pf::PARM | pf::RETURN_PARM);
    b.func(le, 0, le_a, &[], 0, 152, native_op);
    // Pawn: Health, bIsDead, Deaths, TakeDamage(int).
    let health = b.reserve(IMP_INTPROP, pawn, "Health");
    let dead = b.reserve(B_BOOLPROP, pawn, "bIsDead");
    let deaths = b.reserve(IMP_INTPROP, pawn, "Deaths");
    let take = b.reserve(IMP_FUNCTION, pawn, "TakeDamage");
    let damage = b.reserve(IMP_INTPROP, take, "Damage");
    b.prop(health, dead, 0);
    b.prop(dead, deaths, 0);
    b.prop(deaths, take, 0);
    b.prop(damage, 0, pf::PARM);
    let (h, dm, dd, deaths_f) = (health as u8, damage as u8, dead as u8, deaths as u8);
    #[rustfmt::skip]
    let take_code: Vec<u8> = vec![
        0x0F, 0x01, h, 0x93, 0x01, h, 0x01, dm, 0x16,       // Health = Health - Damage
        0x07, 0x2B, 0x00, 0x98, 0x01, h, 0x25, 0x16,       // if !(Health <= 0) goto 0x2B
        0x0F, 0x01, dd, 0x27,                                // bIsDead = true
        0x0F, 0x01, deaths_f, 0x26,                          // Deaths = 1
        0x04, 0x0B,                                          // return
    ];
    // 45 is the decoded script's UE memory size (`ScriptSize`), not its byte length: every
    // `object()`/`name()` operand counts 4 memory bytes (1 in the file), so the 6 instance
    // variable operands add 18 to the 27 bytes.
    b.func(take, 0, damage, &take_code, 45, 0, ff::DEFINED);
    b.class(object, 0, sub);
    b.class(actor, object, 0);
    b.class(pawn, actor, health);
    b.build()
}

#[test]
fn synthetic_take_damage_reduces_health_and_dies_at_or_below_zero() {
    let set = set_of(damage_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    let p = vm.spawn(g(&set, "Pawn"), "P").unwrap();
    vm.set_property(p, "Health", 0, Value::Int(50));
    let take = g(&set, "Pawn.TakeDamage");
    // Non-lethal: health drops, the death flag and the counter stay clear.
    vm.call_function(take, p, vec![Value::Int(20)]).unwrap();
    assert_eq!(vm.get_property(p, "Health"), Some(&Value::Int(30)));
    assert_eq!(vm.get_property(p, "bIsDead"), Some(&Value::Bool(false)));
    assert_eq!(vm.get_property(p, "Deaths"), Some(&Value::Int(0)));
    // Exactly zero is a death (`<= 0`).
    vm.call_function(take, p, vec![Value::Int(30)]).unwrap();
    assert_eq!(vm.get_property(p, "Health"), Some(&Value::Int(0)));
    assert_eq!(vm.get_property(p, "bIsDead"), Some(&Value::Bool(true)));
    assert_eq!(vm.get_property(p, "Deaths"), Some(&Value::Int(1)));
    // Beyond zero: health goes negative, the death counter does not double.
    vm.call_function(take, p, vec![Value::Int(10)]).unwrap();
    assert_eq!(vm.get_property(p, "Health"), Some(&Value::Int(-10)));
    assert_eq!(vm.get_property(p, "Deaths"), Some(&Value::Int(1)));
}

/// Synthetic package with an `Actor` carrying the collision fields the actor trace reads.
fn trace_package() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let vector = b.reserve(IMP_CORE, 0, "Vector");
    let rotator = b.reserve(IMP_CORE, 0, "Rotator");
    let loc = b.reserve(B_STRUCTPROP, actor, "Location");
    let rot = b.reserve(B_STRUCTPROP, actor, "Rotation");
    let radius = b.reserve(IMP_FLOATPROP, actor, "CollisionRadius");
    let height = b.reserve(IMP_FLOATPROP, actor, "CollisionHeight");
    let collide = b.reserve(B_BOOLPROP, actor, "bCollideActors");
    let bzero = b.reserve(B_BOOLPROP, actor, "bBlockZeroExtentTraces");
    let bnz = b.reserve(B_BOOLPROP, actor, "bBlockNonZeroExtentTraces");
    b.prop_with(loc, rot, 0, &compact(vector));
    b.prop_with(rot, radius, 0, &compact(rotator));
    b.prop(radius, height, 0);
    b.prop(height, collide, 0);
    b.prop(collide, bzero, 0);
    b.prop(bzero, bnz, 0);
    b.prop(bnz, 0, 0);
    b.class(object, 0, 0);
    b.class(actor, object, loc);
    b.build()
}

fn trace_args() -> Vec<Value> {
    vec![
        Value::Vector([0.0; 3]),           // HitLocation (out)
        Value::Vector([0.0; 3]),           // HitNormal (out)
        Value::Vector([1000.0, 0.0, 0.0]), // TraceEnd
        Value::Vector([0.0; 3]),           // TraceStart
        Value::Bool(true),                 // bTraceActors
        Value::Vector([0.0; 3]),           // Extent (line)
    ]
}

#[test]
fn synthetic_trace_hits_the_nearest_pawn_before_world_geometry() {
    let set = set_of(trace_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    // A world wall beyond both actors: the nearest actor must win.
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([900.0, -100.0, -100.0], [950.0, 100.0, 100.0]),
    ));
    let near = vm.spawn(g(&set, "Actor"), "Near").unwrap();
    let far = vm.spawn(g(&set, "Actor"), "Far").unwrap();
    // The tracer is a third actor off the ray: `Actor.Trace` skips its own object.
    let shooter = vm.spawn(g(&set, "Actor"), "Shooter").unwrap();
    vm.set_active(shooter, true);
    for (id, x) in [(near, 100.0), (far, 300.0)] {
        vm.set_property(id, "Location", 0, Value::Vector([x, 0.0, 0.0]));
        vm.set_property(id, "CollisionRadius", 0, Value::Float(40.0));
        vm.set_property(id, "CollisionHeight", 0, Value::Float(40.0));
        vm.set_property(id, "bBlockZeroExtentTraces", 0, Value::Bool(true));
        vm.set_active(id, true);
    }
    let mut args = trace_args();
    let hit = call_native(
        &mut vm,
        "Actor.Trace",
        shooter,
        &[false, false, false, false, false, false],
        &mut args,
    );
    assert_eq!(
        hit,
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(near)))),
        "the trace must hit the nearest blocking actor"
    );
    // The hit point is on the near cylinder's -X face and the zone is a spine band.
    match &args[0] {
        Value::Vector(l) => assert!((l[0] - 60.0).abs() < 1.0, "hit at {l:?}"),
        other => panic!("expected hit location, got {other:?}"),
    }
    assert_eq!(vm.last_trace_bone(), "X Spine1");
    // `Actor.GetLastTraceBone` returns the recorded zone.
    let bone = call_native(&mut vm, "Actor.GetLastTraceBone", near, &[], &mut []);
    assert_eq!(bone, NativeOutcome::Value(Value::Name("X Spine1".into())));
}

#[test]
fn synthetic_trace_world_geometry_closer_than_the_pawn_wins() {
    let set = set_of(trace_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    // The wall sits between the start and the only actor.
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([50.0, -100.0, -100.0], [60.0, 100.0, 100.0]),
    ));
    let a = vm.spawn(g(&set, "Actor"), "A").unwrap();
    vm.set_property(a, "Location", 0, Value::Vector([300.0, 0.0, 0.0]));
    vm.set_property(a, "CollisionRadius", 0, Value::Float(40.0));
    vm.set_property(a, "CollisionHeight", 0, Value::Float(40.0));
    vm.set_property(a, "bBlockZeroExtentTraces", 0, Value::Bool(true));
    vm.set_active(a, true);
    let mut args = trace_args();
    let hit = call_native(
        &mut vm,
        "Actor.Trace",
        a,
        &[false, false, false, false, false, false],
        &mut args,
    );
    // No LevelInfo in this fixture: a world hit returns Null.
    assert_eq!(
        hit,
        NativeOutcome::Value(Value::Object(None)),
        "the closer world wall must win over the pawn"
    );
    assert_eq!(vm.last_trace_bone(), "None");
}

// Localisation provider, localized class defaults and static calls on class defaults.

/// A provider whose `get` answers `Thing.label` with `value` and everything else `None`.
struct MapLoc(&'static str);

impl LocalizationData for MapLoc {
    fn get(&self, package: &str, section: &str, key: &str) -> Option<String> {
        (package.eq_ignore_ascii_case("Test")
            && section.eq_ignore_ascii_case("Thing")
            && key.eq_ignore_ascii_case("label"))
        .then(|| self.0.to_owned())
    }

    fn language(&self) -> &str {
        "int"
    }
}

/// `Object` -> `Thing` with a `localized` string `Label` (XIII bit `0x400000`) and a plain int
/// `Count`.
fn localized_fixture() -> Vec<u8> {
    use pf::*;
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let thing = b.reserve(0, 0, "Thing");
    let label = b.reserve(B_STRPROP, thing, "Label");
    let count = b.reserve(IMP_INTPROP, thing, "Count");
    b.prop(label, count, LOCALIZED);
    b.prop(count, 0, 0);
    b.class(thing, object, label);
    b.class(object, 0, 0);
    b.build()
}

#[test]
fn localized_class_default_is_filled_from_the_provider() {
    let set = set_of(localized_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_localization(Box::new(MapLoc("from-int")));
    let class = sg(&set, "Thing");
    let layout = vm.class_layout(class).unwrap();
    let value = |n: &str| layout.defaults[layout.slot_by_name(n).unwrap().base].clone();
    assert_eq!(value("label"), Value::Str("from-int".to_owned()));
    // The non-localized sibling keeps its zero default.
    assert_eq!(value("count"), Value::Int(0));
    assert_eq!(vm.localized_overrides, 1);
}

#[test]
fn localized_class_default_miss_leaves_the_serialized_value() {
    struct Missing;
    impl LocalizationData for Missing {
        fn get(&self, _p: &str, _s: &str, _k: &str) -> Option<String> {
            None
        }
        fn language(&self) -> &str {
            "int"
        }
    }
    let set = set_of(localized_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_localization(Box::new(Missing));
    let class = sg(&set, "Thing");
    let layout = vm.class_layout(class).unwrap();
    let base = layout.slot_by_name("label").unwrap().base;
    assert_eq!(layout.defaults[base], Value::Str(String::new()));
    assert_eq!(vm.localized_overrides, 0);
}

#[test]
fn object_localize_returns_the_provider_value_and_the_placeholder_on_miss() {
    let set = set_of(localized_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_localization(Box::new(MapLoc("localized-text")));
    let t = vm.spawn(sg(&set, "Thing"), "T").unwrap();

    let mut hit = [
        Value::Str("Thing".to_owned()),
        Value::Str("Label".to_owned()),
        Value::Str("Test".to_owned()),
    ];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Localize",
            t,
            &[false; 3],
            &mut hit
        )),
        "localized-text"
    );
    assert_eq!(vm.localization_hits, 1);

    let mut miss = [
        Value::Str("Thing".to_owned()),
        Value::Str("Nope".to_owned()),
        Value::Str("Test".to_owned()),
    ];
    assert_eq!(
        str_result(call_native(
            &mut vm,
            "Object.Localize",
            t,
            &[false; 3],
            &mut miss
        )),
        "<?int?Test.Thing.Nope?>"
    );
    assert_eq!(vm.localization_misses, 1);
}

#[test]
fn object_localize_without_a_provider_is_an_explicit_error() {
    let set = set_of(localized_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let t = vm.spawn(sg(&set, "Thing"), "T").unwrap();
    let mut args = [
        Value::Str("Thing".to_owned()),
        Value::Str("Label".to_owned()),
        Value::Str("Test".to_owned()),
    ];
    let def = native("Object.Localize");
    let err = (def.f)(&mut vm, &ctx(t, &[false; 3], "Object.Localize"), &mut args).unwrap_err();
    assert!(matches!(
        err.kind,
        VmErrorKind::NoLocalizationProvider { .. }
    ));
}

/// `Object` -> `Thing` with a `static` `GetLabel()` returning a string constant, and an
/// `Object.Call()` that returns `Thing.static.GetLabel()` through a `ClassContext`.
fn static_on_default_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let thing = b.reserve(0, 0, "Thing");
    let get = b.reserve(IMP_FUNCTION, thing, "GetLabel");
    let get_r = b.reserve(B_STRPROP, get, "ReturnValue");
    b.prop(get_r, 0, RETURN_PARM);
    let mut body = vec![0x04, 0x1F];
    body.extend_from_slice(b"hello");
    body.push(0);
    // Return opcode (1) + StringConst opcode (1) + 5 chars + NUL.
    b.func(get, 0, get_r, &body, 8, 0, STATIC | DEFINED);

    let call = b.reserve(IMP_FUNCTION, object, "Call");
    let call_r = b.reserve(B_STRPROP, call, "ReturnValue");
    b.prop(call_r, 0, RETURN_PARM);
    let get_name = b.exports[(get - 1) as usize].name;
    let mut code = vec![0x04, 0x12, 0x20];
    code.extend(compact(thing));
    code.extend(0u16.to_le_bytes());
    code.push(0);
    code.push(0x38);
    code.extend(compact(get_name));
    code.push(0x16);
    // Return (1) + ClassContext (1) + ObjectConst (1 + object 4) + skip u16 (2) + size (1)
    // + GlobalFunction (1 + name 4) + EndFunctionParms (1).
    b.func(call, 0, call_r, &code, 16, 0, DEFINED);

    b.class(thing, object, get);
    b.class(object, 0, call);
    b.build()
}

#[test]
fn static_function_runs_on_an_inactive_class_default_object() {
    let set = set_of(static_on_default_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let host = vm.spawn(sg(&set, "Object"), "Host").unwrap();
    vm.set_active(host, true);
    // `Object.Call()` evaluates `Thing.static.GetLabel()` on `Default__Thing`, which is not in
    // the executed scope. Before the fix this returned `DeferredWithReturnValue`.
    let value = vm
        .call_function(sg(&set, "Object.Call"), host, Vec::new())
        .expect("a static function on a class default object must run");
    assert_eq!(value, Value::Str("hello".to_owned()));
}

#[test]
fn color_operator_natives_clamp_componentwise() {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let thing = b.reserve(0, 0, "Thing");
    let count = b.reserve(IMP_INTPROP, thing, "Count");
    b.prop(count, 0, 0);
    b.class(thing, object, count);
    b.class(object, 0, 0);
    let set = set_of(b.build());
    let mut vm = Vm::new(&set, VmLimits::default());
    let t = vm.spawn(sg(&set, "Thing"), "T").unwrap();
    let color = |b: u8, g: u8, r: u8, a: u8| {
        Value::Struct(vec![
            ("b".to_owned(), Value::Byte(b)),
            ("g".to_owned(), Value::Byte(g)),
            ("r".to_owned(), Value::Byte(r)),
            ("a".to_owned(), Value::Byte(a)),
        ])
    };
    let channel = |v: &Value, n: &str| match v {
        Value::Struct(f) => f
            .iter()
            .find(|(k, _)| k == n)
            .and_then(|(_, v)| match v {
                Value::Byte(b) => Some(*b),
                _ => None,
            })
            .unwrap(),
        other => panic!("{other:?}"),
    };

    // 255 * 0.5 truncates to 127 (not rounded) and stays in range.
    let mut a = [color(255, 255, 255, 255), Value::Float(0.5)];
    let v = call_native(&mut vm, "Actor.Multiply_ColorFloat", t, &[false; 2], &mut a);
    let NativeOutcome::Value(v) = v else { panic!() };
    assert_eq!(channel(&v, "r"), 127);
    assert_eq!(channel(&v, "a"), 127);

    // Add clamps at 255; subtract clamps at 0.
    let mut a = [color(200, 10, 255, 1), color(200, 10, 255, 1)];
    let v = call_native(&mut vm, "Actor.Add_ColorColor", t, &[false; 2], &mut a);
    let NativeOutcome::Value(v) = v else { panic!() };
    assert_eq!(channel(&v, "b"), 255);
    assert_eq!(channel(&v, "g"), 20);
    let mut a = [color(10, 0, 5, 0), color(20, 0, 255, 0)];
    let v = call_native(&mut vm, "Actor.Subtract_ColorColor", t, &[false; 2], &mut a);
    let NativeOutcome::Value(v) = v else { panic!() };
    assert_eq!(channel(&v, "b"), 0);
    assert_eq!(channel(&v, "r"), 0);
}

#[test]
fn object_class_property_answers_the_objects_class() {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let thing = b.reserve(0, 0, "Thing");
    let cls = b.reserve(IMP_OBJPROP, object, "Class");
    b.prop_with(cls, 0, 0, &compact(0));
    let count = b.reserve(IMP_INTPROP, thing, "Count");
    b.prop(count, 0, 0);
    b.class(thing, object, count);
    b.class(object, 0, cls);
    let set = set_of(b.build());
    let mut vm = Vm::new(&set, VmLimits::default());
    let t = vm.spawn(sg(&set, "Thing"), "T").unwrap();
    // `Object.Class` is the object's UClass, not a serialized null default; scripts read it to
    // identify a class (`default.Class` in the local-message chain).
    assert_eq!(
        vm.get_property(t, "Class"),
        Some(&Value::Object(Some(ObjRef::Static(sg(&set, "Thing")))))
    );
}

// ---------------------------------------------------------------------------------------
// Cinematic/dialogue natives (item3l)

/// `GetWaveDuration` returns the host provider's value; without one it reports 0 (the script's
/// own fallback) rather than inventing a duration.
#[test]
fn get_wave_duration_reports_provider_or_zero() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let mut args = [Value::Str("Plage00_XIIIa_00".to_owned())];
    let r = try_native(
        &mut vm,
        "Engine.Actor.GetWaveDuration",
        a,
        &[false],
        &mut args,
    )
    .unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Float(0.0)));
    vm.set_voice_duration(Box::new(crate::voice::FixedVoiceDuration::new(2.5)));
    let mut args = [Value::Str("Plage00_XIIIa_00".to_owned())];
    let r = try_native(
        &mut vm,
        "Engine.Actor.GetWaveDuration",
        a,
        &[false],
        &mut args,
    )
    .unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Float(2.5)));
    assert!(vm.has_voice_duration());
    assert_eq!(vm.voice_duration("anything"), Some(2.5));
}

/// `PlayStrVoice` emits a `Dialogue` event carrying the voice name, the speaker pawn and the
/// provider duration; a plain actor (no `LineIndex`/`Lines`/`Speakers`) has no subtitle text.
#[test]
fn play_str_voice_emits_dialogue_event_with_speaker_and_duration() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let dm = vm.spawn(sg(&set, "Actor"), "DialogueManager0").unwrap();
    let pam = vm.spawn(sg(&set, "Actor"), "Cine0").unwrap();
    vm.set_voice_duration(Box::new(crate::voice::FixedVoiceDuration::new(2.5)));
    let mut args = [
        Value::Str("Plage00_XIIIa_00".to_owned()),
        Value::Object(Some(ObjRef::Instance(pam))),
    ];
    let r = try_native(
        &mut vm,
        "Engine.Actor.PlayStrVoice",
        dm,
        &[false, false],
        &mut args,
    )
    .unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Bool(true)));
    let events = vm.drain_events();
    // The voice name is also emitted as a `PlaySound` so the existing audio layer speaks it.
    let dialogue = events
        .iter()
        .find_map(|e| match e {
            crate::events::PresentationEvent::Dialogue(d) => Some(d),
            _ => None,
        })
        .unwrap_or_else(|| panic!("no Dialogue event in {events:?}"));
    assert_eq!(dialogue.actor, "DialogueManager0");
    assert_eq!(dialogue.speaker.as_deref(), Some("Cine0"));
    assert_eq!(dialogue.sound, "Plage00_XIIIa_00");
    assert_eq!(dialogue.text, None);
    assert_eq!(dialogue.duration, Some(2.5));
    assert!(
        events.iter().any(|e| matches!(
            e,
            crate::events::PresentationEvent::PlaySound(s)
                if s.sound.as_deref() == Some("Plage00_XIIIa_00")
        )),
        "the voice must also be emitted as a PlaySound: {events:?}"
    );
    // An empty voice name does not emit and reports false (the engine did not start a voice).
    let mut args = [Value::Str(String::new()), Value::Object(None)];
    let r = try_native(
        &mut vm,
        "Engine.Actor.PlayStrVoice",
        dm,
        &[false, false],
        &mut args,
    )
    .unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Bool(false)));
    assert!(vm.drain_events().is_empty());
}

/// The subtitle text comes from the `DialogueManager`'s current line: `LineIndex` selects a
/// `Lines` element, whose `SpeakerIndex`/`SentenceIndex` select the nested speaker sentence.
#[test]
fn dialogue_line_text_reads_nested_speaker_sentences() {
    let lines = Value::Array(vec![
        Value::Struct(vec![
            ("SpeakerIndex".to_owned(), Value::Int(-1)),
            ("SentenceIndex".to_owned(), Value::Int(-1)),
        ]),
        Value::Struct(vec![
            ("SpeakerIndex".to_owned(), Value::Int(1)),
            ("SentenceIndex".to_owned(), Value::Int(0)),
        ]),
    ]);
    let speakers = Value::Array(vec![
        Value::Struct(vec![("Sentences".to_owned(), Value::Array(Vec::new()))]),
        Value::Struct(vec![(
            "Sentences".to_owned(),
            Value::Array(vec![Value::Str("My name is XIII.".to_owned())]),
        )]),
    ]);
    assert_eq!(
        crate::cinematics::line_text_from_values(1, &lines, &speakers).as_deref(),
        Some("My name is XIII.")
    );
    // A line whose indices are -1 (the script's "end of line" sentinel) has no text.
    assert_eq!(
        crate::cinematics::line_text_from_values(0, &lines, &speakers),
        None
    );
    // Out-of-range line and negative line index do not panic.
    assert_eq!(
        crate::cinematics::line_text_from_values(9, &lines, &speakers),
        None
    );
    assert_eq!(
        crate::cinematics::line_text_from_values(-1, &lines, &speakers),
        None
    );
    // A speaker with no sentences at the requested index yields None.
    let bad = Value::Array(vec![Value::Struct(vec![
        ("SpeakerIndex".to_owned(), Value::Int(0)),
        ("SentenceIndex".to_owned(), Value::Int(0)),
    ])]);
    assert_eq!(
        crate::cinematics::line_text_from_values(0, &bad, &speakers),
        None
    );
}

/// UE2 `switch` and `==` on strings/names are case-insensitive (`appStricmp`); the cine
/// interpreter switches on lowercase action words (`dial`) against `Dial` case values.
#[test]
fn values_equal_is_case_insensitive_for_strings_and_names() {
    use crate::value::Value;
    use crate::vm::values_equal;
    assert!(values_equal(
        &Value::Str("dial".into()),
        &Value::Str("Dial".into())
    ));
    assert!(values_equal(
        &Value::Str("Event".into()),
        &Value::Str("event".into())
    ));
    assert!(!values_equal(
        &Value::Str("dial".into()),
        &Value::Str("dialman".into())
    ));
    assert!(values_equal(
        &Value::Name("dial_debut".into()),
        &Value::Name("DIAL_DEBUT".into())
    ));
    // An int/byte pair still compares by value, and unrelated values are not equal.
    assert!(values_equal(&Value::Int(1), &Value::Byte(1)));
    assert!(!values_equal(&Value::Int(1), &Value::Byte(2)));
    assert!(!values_equal(&Value::Str("1".into()), &Value::Int(1)));
}

// ---------------------------------------------------------------------------------------
// item3n: campaign-suspension fixes (cast, class-cast context, Box.IsValid, natives)

#[test]
fn vector_to_rotator_and_string_casts_match_ue2() {
    let set = spawn_set();
    let vm = Vm::new(&set, VmLimits::default());
    // VectorToRotator (0x50): yaw = atan2(Y,X), pitch = atan2(Z,|XY|), roll 0; 65536 per turn.
    assert_eq!(
        vm.primitive_cast(0x50, Value::Vector([1.0, 0.0, 0.0]))
            .unwrap(),
        Value::Rotator([0, 0, 0])
    );
    assert_eq!(
        vm.primitive_cast(0x50, Value::Vector([0.0, 1.0, 0.0]))
            .unwrap(),
        Value::Rotator([0, 16384, 0])
    );
    assert_eq!(
        vm.primitive_cast(0x50, Value::Vector([0.0, 0.0, 1.0]))
            .unwrap(),
        Value::Rotator([16384, 0, 0])
    );
    // The zero vector has no direction: zero rotator (BeyondUnreal "Typecast").
    assert_eq!(
        vm.primitive_cast(0x50, Value::Vector([0.0, 0.0, 0.0]))
            .unwrap(),
        Value::Rotator([0, 0, 0])
    );
    // VectorToString (0x58) / RotatorToString (0x59).
    assert_eq!(
        vm.primitive_cast(0x58, Value::Vector([1.5, -2.0, 0.0]))
            .unwrap(),
        Value::Str("1.50,-2.00,0.00".into())
    );
    assert_eq!(
        vm.primitive_cast(0x59, Value::Rotator([-1, 65536, 32768]))
            .unwrap(),
        Value::Str("65535,0,32768".into())
    );
    // A wrong operand type is still an explicit error, never a silent value.
    assert!(vm.primitive_cast(0x50, Value::Int(1)).is_err());
}

#[test]
fn context_object_class_resolves_a_class_cast_target() {
    use crate::bytecode::{Token, TokenKind};
    use xiii_package::ObjectRef;
    let set = spawn_set();
    let vm = Vm::new(&set, VmLimits::default());
    let child = sg(&set, "Child");
    let no_object = Token {
        offset: 0,
        file_offset: 0,
        memory_size: 1,
        opcode: 0x2A,
        kind: TokenKind::NoObject,
    };
    let cast = Token {
        offset: 0,
        file_offset: 0,
        memory_size: 6,
        opcode: 0x2E,
        kind: TokenKind::DynamicCast {
            class: ObjectRef::Export(child.export),
            expr: Box::new(no_object),
        },
    };
    // Regression: a `DynamicCast` used to fall through to `member_property`, which is `None` for
    // a cast, so `Pawn(Other).IsPlayerPawn()` on a failed cast resolved no return type and
    // suspended with `TypeMismatch { expected: "bool", found: "void" }`.
    assert_eq!(vm.context_object_class(0, 0, &cast), Some(child));
}

#[test]
fn get_axes_fills_the_rotator_basis() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Actor"), "O").unwrap();
    let mut a = vec![
        Value::Rotator([0, 0, 0]),
        Value::Vector([9.0; 3]),
        Value::Vector([9.0; 3]),
        Value::Vector([9.0; 3]),
    ];
    let out = call_native(
        &mut vm,
        "Object.GetAxes",
        o,
        &[false, false, false, false],
        &mut a,
    );
    assert!(matches!(out, NativeOutcome::Value(Value::Void)));
    assert_eq!(a[1], Value::Vector([1.0, 0.0, 0.0]));
    assert_eq!(a[2], Value::Vector([0.0, 1.0, 0.0]));
    assert_eq!(a[3], Value::Vector([0.0, 0.0, 1.0]));
}

/// The rotator operators the campaign survey hit: `Multiply_RotatorFloat` (287,
/// `xidcine.HelicoDeco.HelicoTick`) and `EqualEqual_RotatorRotator` (142,
/// `xiii.MitraillTop.GoToWaitingPos.Tick`), plus the rest of the declared rotator family.
#[test]
fn rotator_operators_scale_divide_and_compare_componentwise() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Object"), "O").unwrap();

    // Multiply_RotatorFloat (287) and Multiply_FloatRotator (288): componentwise, truncated.
    let mut a = vec![Value::Rotator([100, -50, 3]), Value::Float(0.5)];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.Multiply_RotatorFloat",
            o,
            &[false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Rotator([50, -25, 1]))
    );
    let mut a = vec![Value::Float(0.5), Value::Rotator([100, -50, 3])];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.Multiply_FloatRotator",
            o,
            &[false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Rotator([50, -25, 1]))
    );

    // Divide_RotatorFloat (289).
    let mut a = vec![Value::Rotator([100, -50, 3]), Value::Float(2.0)];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.Divide_RotatorFloat",
            o,
            &[false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Rotator([50, -25, 1]))
    );

    // EqualEqual_RotatorRotator (142) and NotEqual_RotatorRotator (203): exact components.
    let mut a = vec![Value::Rotator([1, 2, 3]), Value::Rotator([1, 2, 3])];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.EqualEqual_RotatorRotator",
            o,
            &[false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Bool(true))
    );
    let mut a = vec![Value::Rotator([1, 2, 3]), Value::Rotator([1, 2, 4])];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.EqualEqual_RotatorRotator",
            o,
            &[false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Bool(false))
    );
    assert_eq!(
        call_native(
            &mut vm,
            "Object.NotEqual_RotatorRotator",
            o,
            &[false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Bool(true))
    );

    // A zero divisor is an explicit error, not an infinity.
    let mut a = vec![Value::Rotator([1, 2, 3]), Value::Float(0.0)];
    let def = native("Object.Divide_RotatorFloat");
    let e = (def.f)(&mut vm, &ctx(o, &[false, false], ""), &mut a).unwrap_err();
    assert!(matches!(e.kind, VmErrorKind::DivisionByZero), "{e}");
}

/// `Object.MultiplyMultiply_FloatFloat` (170, `float ** float`), the next operator
/// `xidcine.HelicoDeco.HelicoTick` reaches at 0x01FF.
#[test]
fn float_power_operator_matches_app_pow() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Object"), "O").unwrap();
    let mut a = vec![Value::Float(2.0), Value::Float(10.0)];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.MultiplyMultiply_FloatFloat",
            o,
            &[false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Float(1024.0))
    );
    let mut a = vec![Value::Float(9.0), Value::Float(0.5)];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.MultiplyMultiply_FloatFloat",
            o,
            &[false, false],
            &mut a
        ),
        NativeOutcome::Value(Value::Float(3.0))
    );
}

#[test]
fn get_bounding_box_reports_isvalid_as_a_byte() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Actor"), "O").unwrap();
    vm.set_property(o, "Location", 0, Value::Vector([10.0, 20.0, 30.0]));
    let out = call_native(&mut vm, "Engine.Actor.GetBoundingBox", o, &[], &mut []);
    let NativeOutcome::Value(Value::Struct(members)) = out else {
        panic!("expected a Box struct, got {out:?}");
    };
    let get = |n: &str| members.iter().find(|(k, _)| k == n).map(|(_, v)| v.clone());
    assert_eq!(get("min"), Some(Value::Vector([10.0, 20.0, 30.0])));
    assert_eq!(get("max"), Some(Value::Vector([10.0, 20.0, 30.0])));
    // The script reads `cast<byte->int>(Box.IsValid)`; a byte keeps that cast working.
    assert_eq!(get("isvalid"), Some(Value::Byte(1)));
    assert_eq!(
        vm.primitive_cast(0x3A, Value::Byte(1)).unwrap(),
        Value::Int(1)
    );
}

#[test]
fn stop_animating_clears_every_channel() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Actor"), "O").unwrap();
    vm.objects[o as usize].anim.channels.insert(
        0,
        crate::vm::AnimChannel {
            sequence: "Run".into(),
            frames: 10,
            rate: 30.0,
            frame: 4.0,
            looping: true,
            active: true,
            tween_remaining: 0.0,
            notifies: Vec::new(),
            notify_idx: 0,
        },
    );
    let out = call_native(&mut vm, "Engine.Actor.StopAnimating", o, &[], &mut []);
    assert!(matches!(out, NativeOutcome::Value(Value::Void)));
    assert!(vm.objects[o as usize].anim.channels.is_empty());
}

#[test]
fn snow_natives_record_and_do_not_fail() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Actor"), "L").unwrap();
    let mut a = vec![
        Value::Object(None),
        Value::Int(4),
        Value::Float(0.5),
        Value::Float(10.0),
    ];
    let out = call_native(
        &mut vm,
        "LevelInfo.InitRndCubeSpr",
        o,
        &[false, false, false, false],
        &mut a,
    );
    assert!(matches!(out, NativeOutcome::Value(Value::Void)));
    assert!(vm.trace.iter().any(|e| matches!(
        &e.kind,
        TraceKind::Log(s) if s.contains("InitRndCubeSpr") && s.contains("no particle subsystem")
    )));
    // ChangeRndCubeSprProp returns false (not applied), never a silent true.
    let mut a = vec![Value::Float(1.0), Value::Float(1.0), Value::Float(1.0)];
    let out = call_native(
        &mut vm,
        "LevelInfo.ChangeRndCubeSprProp",
        o,
        &[false, false, false],
        &mut a,
    );
    assert!(matches!(out, NativeOutcome::Value(Value::Bool(false))));
}

/// `ParticleEmitter.SetMaxParticles` has no renderer here; it must record a visible trace note
/// and never silently claim success (`xidcine.BreakableMover.InitializeEmitters`).
#[test]
fn set_max_particles_records_and_does_not_fail() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(sg(&set, "Actor"), "E").unwrap();
    let mut a = vec![Value::Int(12)];
    let out = call_native(
        &mut vm,
        "ParticleEmitter.SetMaxParticles",
        o,
        &[false],
        &mut a,
    );
    assert!(matches!(out, NativeOutcome::Value(Value::Void)));
    assert!(vm.trace.iter().any(|e| matches!(
        &e.kind,
        TraceKind::Note(s) if s.contains("SetMaxParticles(12)") && s.contains("no particle subsystem")
    )));
}

#[test]
fn video_player_status_times_a_host_registered_duration() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    // No open clip: finished.
    assert_eq!(vm.video_status(), 0);
    // A registered duration is timed from `Play`.
    vm.set_video_duration("Cine01.bik", 10.0);
    assert!(vm.video_open("cine01"));
    vm.video_play();
    assert_eq!(vm.video_status(), 1);
    for _ in 0..99 {
        vm.tick(0.1).unwrap();
    }
    assert_eq!(vm.video_status(), 1, "9.9 s < 10 s");
    vm.tick(0.2).unwrap();
    assert_eq!(vm.video_status(), 0, "10.1 s >= 10 s");
    // An unknown clip reports finished immediately (the labelled Partial).
    assert!(!vm.video_open("movie_without_header"));
    vm.video_play();
    assert_eq!(vm.video_status(), 0);
    // A bad duration is rejected, so a corrupt header cannot stop the level end.
    vm.set_video_duration("bad", f32::NAN);
    assert!(!vm.video_open("bad"));
}

#[test]
fn percent_float_float_matches_fmod() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(g(&set, "Object"), "O").unwrap();
    let mut a = vec![Value::Float(7.5), Value::Float(2.0)];
    let out = call_native(
        &mut vm,
        "Object.Percent_FloatFloat",
        o,
        &[false, false],
        &mut a,
    );
    let NativeOutcome::Value(Value::Float(v)) = out else {
        panic!("expected a float");
    };
    assert!((v - 1.5).abs() < 1e-6, "7.5 % 2.0 = {v}");
}

// ---------------------------------------------------------------------------------------
// Delegate opcodes: assignment (0x45/0x44), empty delegate (0x3F), call (0x43).

/// Package with a delegate property `Handler`, the bound function `Target` (returns 42) and
/// `Set` / `Clear` / `Fire` exercising `letdelegate`, `emptyd`, and the delegate call.
fn delegate_fixture() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    // `Core.DelegateProperty` import (the property's class).
    let delegate_class = b.add_external("Core", "Class", "DelegateProperty");
    let handler = b.reserve(delegate_class, object, "Handler");
    let target = b.reserve(IMP_FUNCTION, object, "Target");
    let set = b.reserve(IMP_FUNCTION, object, "Set");
    let fire = b.reserve(IMP_FUNCTION, object, "Fire");
    let clear = b.reserve(IMP_FUNCTION, object, "Clear");
    // Delegate property: `DelegateProperty.Function` ref is None (unused by the VM).
    let extra = compact(0);
    b.prop_with(handler, target, 0, &extra);
    // `return 42`.
    let target_code = [0x04, 0x1D, 42, 0, 0, 0];
    b.func(target, set, 0, &target_code, 6, 0, ff::DEFINED);
    // `self.Handler = delegateprop Target`.
    let target_name = b.name("Target");
    let mut set_code = vec![0x45, 0x01];
    set_code.extend(compact(handler));
    set_code.push(0x44);
    set_code.extend(compact(target_name));
    b.func(set, fire, 0, &set_code, 11, 0, ff::DEFINED);
    // `self.Handler = emptydelegate`.
    let mut clear_code = vec![0x45, 0x01];
    clear_code.extend(compact(handler));
    clear_code.push(0x3F);
    b.func(clear, 0, 0, &clear_code, 7, 0, ff::DEFINED);
    // `return self.delegate Handler:Target()` (the context supplies `self`).
    let mut fire_code = vec![0x04, 0x19, 0x17, 0x00, 0x00, 0x00, 0x43];
    fire_code.extend(compact(handler));
    fire_code.extend(compact(target_name));
    fire_code.push(0x16);
    b.func(fire, clear, 0, &fire_code, 16, 0, ff::DEFINED);
    b.class(object, 0, handler);
    b.build()
}

#[test]
fn delegate_assignment_calls_the_bound_function() {
    let set = set_of(delegate_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(g(&set, "Object"), "D").unwrap();
    vm.set_active(o, true);
    vm.call_function(g(&set, "Object.Set"), o, vec![]).unwrap();
    match vm.get_property(o, "Handler") {
        Some(Value::Delegate(Some(d))) => {
            assert_eq!(d.function, "Target");
            assert_eq!(d.object, Some(ObjRef::Instance(o)));
        }
        other => panic!("Handler = {other:?}"),
    }
    let v = vm.call_function(g(&set, "Object.Fire"), o, vec![]).unwrap();
    assert_eq!(v, Value::Int(42));
}

#[test]
fn empty_delegate_falls_back_to_the_declared_function() {
    let set = set_of(delegate_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let o = vm.spawn(g(&set, "Object"), "D").unwrap();
    vm.set_active(o, true);
    vm.call_function(g(&set, "Object.Set"), o, vec![]).unwrap();
    // Clearing the delegate must not leave a stale binding.
    vm.call_function(g(&set, "Object.Clear"), o, vec![])
        .unwrap();
    assert_eq!(vm.get_property(o, "Handler"), Some(&Value::Delegate(None)));
    // An unbound delegate call runs the declared function on the context.
    let v = vm.call_function(g(&set, "Object.Fire"), o, vec![]).unwrap();
    assert_eq!(v, Value::Int(42));
}

#[test]
fn delegate_values_are_equatable_and_none_is_distinct() {
    let d = Value::Delegate(Some(Delegate {
        object: Some(ObjRef::Instance(3)),
        function: "Target".into(),
    }));
    let same = Value::Delegate(Some(Delegate {
        object: Some(ObjRef::Instance(3)),
        function: "target".into(),
    }));
    // Case-sensitive function names: different spelling is a different delegate.
    assert!(!crate::vm::values_equal(&d, &same));
    assert!(crate::vm::values_equal(&d, &d.clone()));
    assert!(!crate::vm::values_equal(&Value::Delegate(None), &d));
}
