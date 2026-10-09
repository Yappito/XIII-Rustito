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
/// `Core.ByteProperty` fixture sentinel, resolved to a trailing import in `B::build`.
const B_BYTEPROP: i32 = i32::MIN;

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
        // Keep external import indices stable; fixtures that use byte properties add the new
        // import after all existing external pairs and resolve the sentinel export class above.
        if self.exports.iter().any(|e| e.class == B_BYTEPROP) {
            let byteprop_ref = -(imports.len() as i32 + 1);
            imports.push((core, class, -1, self.name("ByteProperty")));
            for export in &mut self.exports {
                if export.class == B_BYTEPROP {
                    export.class = byteprop_ref;
                }
            }
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

/// `Actor` fixture for the per-instance `Tick` memo: a **class** `Tick` (`Counter = 1`) and a
/// **state-scoped** `Tick` in `Waiting` (`Counter = 10`), so the resolved `Tick` differs by
/// state and a stale memo is observable as the wrong constant being written.
fn tick_state_fixture() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let class_tick = b.reserve(IMP_FUNCTION, actor, "Tick");
    let waiting = b.reserve(IMP_STATE, actor, "Waiting");
    let counter = b.reserve(IMP_INTPROP, actor, "Counter");
    let rc = counter as u8;
    // Counter's sibling is the class Tick; the state is the class Tick's sibling.
    b.prop(counter, class_tick, 0);
    // Class Tick: `Counter = 1` (`Let Counter = IntOne`), then return void. Memory: Let(1) +
    // instance var(1 opcode + 4 ref) + int one(1) + return(1) + nothing(1) = 9.
    b.func(
        class_tick,
        waiting,
        0,
        &[0x0F, 0x01, rc, 0x26, 0x04, 0x0B],
        9,
        0,
        ff::DEFINED,
    );
    // State-scoped Tick: `Counter = 10` (int const 10 = `0x2C 0x0A`), return void.
    // Memory: Let(1) + instance var(1 + 4) + int const(2) + return(1) + nothing(1) = 10.
    let state_tick = b.reserve(IMP_FUNCTION, waiting, "Tick");
    b.func(
        state_tick,
        0,
        0,
        &[0x0F, 0x01, rc, 0x2C, 10, 0x04, 0x0B],
        10,
        0,
        ff::DEFINED,
    );
    // The state has no code of its own, only the state-scoped Tick child.
    b.state_children(waiting, 0, state_tick, &[], 0, 0);
    b.class(object, 0, 0);
    b.class(actor, object, counter);
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

fn pressing_fire_package() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let controller = b.reserve(0, 0, "Controller");
    let pawn = b.reserve(0, 0, "Pawn");
    let controller_fire = b.reserve(B_BOOLPROP, controller, "bFire");
    let controller_ai_fire = b.reserve(B_BOOLPROP, controller, "bTire");
    let pawn_controller = b.reserve(IMP_OBJPROP, pawn, "Controller");
    let pawn_fire = b.reserve(B_BOOLPROP, pawn, "bFire");
    let pawn_instigator = b.reserve(IMP_OBJPROP, pawn, "Instigator");
    let pressing_fire = b.reserve(IMP_FUNCTION, pawn, "PressingFire");
    let result = b.reserve(B_BOOLPROP, pressing_fire, "ReturnValue");
    b.prop(controller_fire, controller_ai_fire, 0);
    b.prop(controller_ai_fire, 0, 0);
    b.prop_with(pawn_controller, pawn_fire, 0, &compact(0));
    b.prop(pawn_fire, pawn_instigator, 0);
    b.prop_with(pawn_instigator, 0, 0, &compact(0));
    b.prop(result, 0, pf::PARM | pf::RETURN_PARM);
    b.func(
        pressing_fire,
        0,
        result,
        &[],
        0,
        0,
        ff::FINAL | ff::NATIVE | ff::SIMULATED,
    );
    b.class(object, 0, 0);
    b.class(actor, object, 0);
    b.class(controller, actor, controller_fire);
    b.class(pawn, actor, pawn_controller);
    b.build()
}

#[test]
fn pawn_pressing_fire_reads_controller_bfire_not_pawn_or_ai_btire() {
    let engine = ScriptPackage::load(
        "Engine",
        pressing_fire_package(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("synthetic Engine package");
    let mut set = ScriptSet::new();
    set.add(engine);
    let mut vm = Vm::new(&set, VmLimits::default());
    let pawn = vm.spawn(g(&set, "Pawn"), "Pawn").unwrap();
    let controller = vm.spawn(g(&set, "Controller"), "Controller").unwrap();
    let pressing_fire = g(&set, "Pawn.PressingFire");

    vm.set_property(pawn, "bFire", 0, Value::Bool(true));
    assert_eq!(
        vm.call_function(pressing_fire, pawn, vec![]).unwrap(),
        Value::Bool(false),
        "without a controller, Pawn.bFire does not make PressingFire true"
    );
    vm.set_property(
        pawn,
        "Controller",
        0,
        Value::Object(Some(ObjRef::Instance(controller))),
    );
    vm.set_property(controller, "bTire", 0, Value::Bool(true));
    assert_eq!(
        vm.call_function(pressing_fire, pawn, vec![]).unwrap(),
        Value::Bool(false),
        "IAController.bTire is a separate script property"
    );
    vm.set_property(controller, "bFire", 0, Value::Bool(true));
    assert_eq!(vm.obj_prop(pawn, "Controller"), Some(controller));
    assert_eq!(
        vm.get_property(controller, "bFire"),
        Some(&Value::Bool(true))
    );
    assert_eq!(
        vm.call_function(pressing_fire, pawn, vec![]).unwrap(),
        Value::Bool(true)
    );
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
fn repeating_weapon_script_timer_refires_at_the_configured_interval() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let weapon = vm.spawn(g(&set, "Actor"), "Weapon").unwrap();
    vm.set_active(weapon, true);
    vm.set_timer(weapon, 0.5, true);

    for _ in 0..3 {
        vm.tick(0.125).unwrap();
    }
    assert!(
        !vm.trace
            .iter()
            .any(|event| matches!(event.kind, TraceKind::Timer { .. })),
        "a weapon timer fired before its authored interval"
    );
    vm.tick(0.125).unwrap();

    let timer_events: Vec<_> = vm
        .trace
        .iter()
        .filter(|event| matches!(&event.kind, TraceKind::Timer { actor } if actor == "Weapon"))
        .collect();
    assert_eq!(timer_events.len(), 1);
    assert_eq!(timer_events[0].tick, 4);
    assert!((timer_events[0].time - 0.5).abs() < 1e-6);

    for _ in 0..3 {
        vm.tick(0.125).unwrap();
    }
    vm.tick(0.125).unwrap();
    let refire_ticks: Vec<_> = vm
        .trace
        .iter()
        .filter_map(|event| {
            matches!(&event.kind, TraceKind::Timer { actor } if actor == "Weapon")
                .then_some(event.tick)
        })
        .collect();
    assert_eq!(refire_ticks, [4, 8]);
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
fn tick_memo_re_resolves_after_state_change() {
    let set = set_of(tick_state_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(g(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    // No state: the class `Tick` runs (`Counter = 1`). The memo records `(None, class Tick)`.
    vm.tick(0.016).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(1)));
    // Enter `Waiting`: the state has its own `Tick` (`Counter = 10`). If the memo were not
    // invalidated it would keep calling the class `Tick` and the counter would stay 1.
    vm.goto_state(a, "Waiting", None).unwrap();
    vm.tick(0.016).unwrap();
    assert_eq!(
        vm.get_property(a, "Counter"),
        Some(&Value::Int(10)),
        "state Tick must win over the stored class Tick"
    );
    // The memo is keyed by state: staying in `Waiting` keeps using the state `Tick`.
    vm.tick(0.016).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(10)));
    // Leaving the state (to `None`) re-resolves to the class `Tick` (`Counter = 1`).
    vm.goto_state(a, "None", None).unwrap();
    vm.tick(0.016).unwrap();
    assert_eq!(vm.get_property(a, "Counter"), Some(&Value::Int(1)));
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
    // Merged registry count. Contributions: item14b added 10 AI/perception natives; item3p added the
    // five missing rotator operators (142, 203, 287, 288, 289), the float power operator (170) and
    // a visible Partial for `ParticleEmitter.SetMaxParticles`; item16 added the menu natives
    // (`VideoPlayer.*`, `Actor.*AllSounds`, `PlayerController.ClientTravel`); item15 added
    // `PlayerController.GetDefaultURL` and `CalcFirstPersonView` (`ClientTravel` is shared with
    // item16, registered once); item3o added `SaveAtCheckpoint`, `OrthoRotation` and three
    // `SetBone*` Partials; item16b added `GUIController.GetStyle`/`InitStateFrame`; item18 added
    // `%` (173), `Normalize` (198), `ParticleEmitter.SpawnParticle` and `Actor.KillAllSounds`;
    // item19 added `CineController2.Steering`, Trail presentation Partials, cutscene bone-query
    // Partials and the visible Partial for `LevelInfo.DecAttaque` (588); item14c added four
    // trail/particle Partials (SpawnParticle is shared with item18); item20 adds ten decoded GUI
    // save-slot declarations; item40c adds the headless Interaction.Initialize and ForceFeedback
    // viewport/device Partials; item43 adds Actor.TraceActors; item30b adds the latent
    // Engine.Controller.WaitForLanding (527) and item30c the BaseSoldier.EyePosition override
    // (both from the item30 Plage01 walk) plus the item30 walk's Actor.AnimIsInGroup (395).
    // Must equal `Registry::builtin().defs().count()`.
    assert_eq!(defs.len(), 334);
    // viewport/device Partials; item43 adds Actor.TraceActors; the item47b banque01 regression
    // fix adds `PlayerController.AdjustAimForDisplay` (498); item49b adds
    // `Actor.DetachFromBone` (403). Must equal
    // `Registry::builtin().defs().count()`.
    assert_eq!(defs.len(), 333);
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
    let instigator = b.reserve(IMP_OBJECTPROP, actor, "Instigator");
    let seen_instigator = b.reserve(IMP_OBJECTPROP, actor, "SeenInstigator");
    let level = b.reserve(IMP_OBJECTPROP, actor, "Level");
    let tag = b.reserve(IMP_NAMEPROP, actor, "Tag");
    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let rotation = b.reserve(IMP_STRUCTPROP, actor, "Rotation");
    let calls = b.reserve(IMP_INTPROP, actor, "Calls");
    let bstatic = b.reserve(IMP_BOOLPROP, actor, "bStatic");
    let deleted = b.reserve(IMP_BOOLPROP, actor, "bDeleteMe");
    let spawned = b.reserve(IMP_FUNCTION, actor, "Spawned");
    b.prop_with(owner, instigator, 0, &object_extra);
    b.prop_with(instigator, level, 0, &object_extra);
    b.prop_with(instigator, seen_instigator, 0, &object_extra);
    b.prop_with(seen_instigator, level, 0, &object_extra);
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
    let mut spawned_code = vec![0x0F, 0x01, seen_instigator as u8, 0x01, instigator as u8];
    spawned_code.extend([
        0x0F,
        0x01,
        calls as u8,
        0x92,
        0x00,
        calls as u8,
        0x26,
        0x16,
        0x04,
        0x0B,
    ]);
    b.func(spawned, pre, 0, &spawned_code, 0x1B, 0, DEFINED);
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
fn spawned_actor_inherits_the_spawners_instigator() {
    // Engine.dll: `AActor::execSpawn` (0x103e56b0) passes the calling actor's `Instigator`
    // (this+0x88) as `ULevel::SpawnActor`'s last argument, and `SpawnActor` (0x10388a20) stores
    // that argument directly into the new actor's `Instigator` field (0x10388d91 stores
    // [ebp+0x38] to [newactor+0x88]) - `SetOwner` (0x10352e30) never touches `Instigator`. So
    // the spawned actor's `Instigator` is the *spawning actor's* `Instigator` even when an
    // explicit `SpawnOwner` is supplied (the Weapon.GiveAmmo ammo case: the ammo inherits the
    // weapon's Instigator - the pawn after GiveTo - while its Owner is the weapon).
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let pawn = vm.spawn(sg(&set, "Actor"), "Pawn").unwrap();
    let spawner = vm.spawn(sg(&set, "Actor"), "Spawner").unwrap();
    vm.set_property(
        spawner,
        "Instigator",
        0,
        Value::Object(Some(ObjRef::Instance(pawn))),
    );
    let explicit_owner = vm.spawn(sg(&set, "Actor"), "ExplicitOwner").unwrap();
    // An explicit SpawnOwner does not change the inherited Instigator.
    let id = vm
        .spawn_actor(
            spawner,
            Some(sg(&set, "Child")),
            Some(explicit_owner),
            None,
            None,
            None,
        )
        .unwrap()
        .expect("spawned with owner");
    assert_eq!(
        vm.get_property(id, "Owner"),
        Some(&Value::Object(Some(ObjRef::Instance(explicit_owner))))
    );
    assert_eq!(
        vm.get_property(id, "Instigator"),
        Some(&Value::Object(Some(ObjRef::Instance(pawn))))
    );
    // A spawner without an Instigator leaves the field None (LevelInfo-level spawns).
    let id2 = vm
        .spawn_actor(
            explicit_owner,
            Some(sg(&set, "Child")),
            None,
            None,
            None,
            None,
        )
        .unwrap()
        .expect("spawned without owner");
    assert_eq!(
        vm.get_property(id2, "Instigator"),
        Some(&Value::Object(None))
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
fn item49b_spawn_inherits_instigator_before_spawned_independently_of_owner() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let pawn = vm.spawn(sg(&set, "Actor"), "Pawn").unwrap();
    let proxy = vm.spawn(sg(&set, "Actor"), "Weapon").unwrap();
    let other_owner = vm.spawn(sg(&set, "Actor"), "OtherOwner").unwrap();
    vm.set_property(
        proxy,
        "Instigator",
        0,
        Value::Object(Some(ObjRef::Instance(pawn))),
    );
    for owner in [None, Some(other_owner)] {
        let ammo = vm
            .spawn_actor(proxy, Some(sg(&set, "Child")), owner, None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(obj_prop(&vm, ammo, "Instigator"), Some(pawn));
        assert_eq!(
            obj_prop(&vm, ammo, "SeenInstigator"),
            Some(pawn),
            "Spawned must see inherited instigator"
        );
        assert_eq!(obj_prop(&vm, ammo, "Owner"), owner);
        let nested = vm
            .spawn_actor(ammo, Some(sg(&set, "Child")), None, None, None, None)
            .unwrap()
            .unwrap();
        assert_eq!(obj_prop(&vm, nested, "SeenInstigator"), Some(pawn));
    }
    // A spawner without an Instigator must not substitute itself or its Owner.
    let no_instigator = vm
        .spawn_actor(
            other_owner,
            Some(sg(&set, "Child")),
            Some(pawn),
            None,
            None,
            None,
        )
        .unwrap()
        .unwrap();
    assert_eq!(obj_prop(&vm, no_instigator, "Instigator"), None);
    assert_eq!(obj_prop(&vm, no_instigator, "SeenInstigator"), None);
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
        assert!((0.0..=1.0).contains(&f), "{f}");
        let i = d.rand_int(7);
        assert!((0..7).contains(&i), "{i}");
    }
    assert_eq!(d.rand_int(0), 0);
    assert_eq!(d.rand_int(-5), 0);
}

#[test]
fn item51_retail_frand_stream_includes_endpoint_and_rand_shares_state() {
    let set = spawn_set();
    let mut vm = Vm::new(
        &set,
        VmLimits {
            rng_seed: 1,
            ..VmLimits::default()
        },
    );
    // MSVCR70's seed-1 sequence. Rand(nonpositive) must not consume a step.
    assert_eq!(vm.rand_int(0), 0);
    assert_eq!(vm.rand_int(-1), 0);
    assert_eq!(vm.next_random(), 41);
    assert_eq!(vm.rand_float(), 18467.0 * f32::from_bits(0x38000100));
    assert_eq!(vm.rand_int(100), 6334 % 100);
    // This seed reaches the maximum CRT result on the next step; FRand can be 1.0.
    let mut endpoint = Vm::new(
        &set,
        VmLimits {
            rng_seed: 0x1_f01b_f641,
            ..VmLimits::default()
        },
    );
    assert_eq!(endpoint.rand_float(), 1.0);
    let mut zero = Vm::new(
        &set,
        VmLimits {
            rng_seed: 0xa170_f641,
            ..VmLimits::default()
        },
    );
    assert_eq!(zero.rand_float(), 0.0);
}

#[test]
fn item51_playanim_omitted_rate_moves_but_explicit_zero_holds_through_small_steps() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(30, 30.0)));
    let actor = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(actor, true);
    let mut args = [
        Value::Name("Walk".into()),
        Value::Float(0.0),
        Value::Float(0.0),
        Value::Int(0),
    ];
    try_native(
        &mut vm,
        "Engine.Actor.PlayAnim",
        actor,
        &[false, true, true, true],
        &mut args,
    )
    .unwrap();
    vm.tick(0.1).unwrap();
    assert!((vm.objects[actor as usize].anim.channels[&0].frame - 3.0).abs() < 1e-5);
    try_native(
        &mut vm,
        "Engine.Actor.PlayAnim",
        actor,
        &[false; 4],
        &mut args,
    )
    .unwrap();
    for _ in 0..600 {
        vm.tick(1.0 / 60.0).unwrap();
    }
    assert_eq!(vm.objects[actor as usize].anim.channels[&0].frame, 0.0);
    assert_eq!(vm.objects[actor as usize].anim.channels[&0].rate, 0.0);
    assert!(
        !vm.trace
            .iter()
            .any(|e| matches!(e.kind, TraceKind::AnimEnd { .. }))
    );
}

#[test]
fn item51_trace_additional_categories_work_when_traceactors_is_false() {
    let set = set_of(trace_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let shooter = vm.spawn(g(&set, "Actor"), "Shooter").unwrap();
    let target = vm.spawn(g(&set, "Actor"), "Target").unwrap();
    vm.set_active(target, true);
    vm.set_property(target, "Location", 0, Value::Vector([100.0, 0.0, 0.0]));
    vm.set_property(target, "CollisionRadius", 0, Value::Float(20.0));
    vm.set_property(target, "CollisionHeight", 0, Value::Float(20.0));
    for p in [
        "bCollideActors",
        "bBlockActors",
        "bBlockPlayers",
        "bBlockZeroExtentTraces",
    ] {
        vm.set_property(target, p, 0, Value::Bool(true));
    }
    let mut args = trace_args();
    args[4] = Value::Bool(false);
    args.extend([Value::Object(None), Value::Int(0)]);
    assert_eq!(
        call_native(&mut vm, "Actor.Trace", shooter, &[false; 8], &mut args),
        NativeOutcome::Value(Value::Object(None))
    );
    args[7] = Value::Int(0x30);
    assert_eq!(
        call_native(&mut vm, "Actor.Trace", shooter, &[false; 8], &mut args),
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(target))))
    );
    // Additional category flags cannot bypass collision-hash membership.
    vm.set_property(target, "bCollideActors", 0, Value::Bool(false));
    assert_eq!(
        call_native(&mut vm, "Actor.Trace", shooter, &[false; 8], &mut args),
        NativeOutcome::Value(Value::Object(None))
    );
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
    assert_eq!(
        vm.get_property(p2, "NextPawn"),
        Some(&Value::Object(Some(ObjRef::Instance(p1))))
    );
    // Remove the head.
    call_native(&mut vm, "Engine.Pawn.RemovePawnFromList", p3, &[], &mut []);
    assert_eq!(
        vm.get_property(level, "PawnList"),
        Some(&Value::Object(Some(ObjRef::Instance(p1))))
    );
    assert_eq!(
        vm.get_property(p3, "NextPawn"),
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
    assert_eq!(
        vm.get_property(c2, "NextController"),
        Some(&Value::Object(Some(ObjRef::Instance(c1))))
    );
}

#[test]
fn self_removal_during_script_style_list_walk_preserves_remaining_nodes() {
    let set = list_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let level = vm.spawn(sg(&set, "Actor"), "Level").unwrap();

    // Mirrors the script pattern used by EndGame: P = Level.ControllerList; while (P != None)
    // { P.RemoveController(); P = P.NextController; }. Read NextController only after removal,
    // so clearing the removed node's link reproduces the engine traversal failure.
    let c1 = vm.spawn(sg(&set, "Controller"), "C1").unwrap();
    let c2 = vm.spawn(sg(&set, "Controller"), "C2").unwrap();
    let c3 = vm.spawn(sg(&set, "Controller"), "C3").unwrap();
    for c in [c1, c2, c3] {
        vm.set_property(c, "Level", 0, Value::Object(Some(ObjRef::Instance(level))));
        call_native(&mut vm, "Engine.Controller.AddController", c, &[], &mut []);
    }
    let mut visited_controllers = Vec::new();
    let mut current = Some(c3);
    while let Some(controller) = current {
        visited_controllers.push(controller);
        call_native(
            &mut vm,
            "Engine.Controller.RemoveController",
            controller,
            &[],
            &mut [],
        );
        current = match vm.get_property(controller, "NextController") {
            Some(Value::Object(Some(ObjRef::Instance(next)))) => Some(*next),
            _ => None,
        };
    }
    assert_eq!(visited_controllers, [c3, c2, c1]);
    assert_eq!(
        vm.get_property(level, "ControllerList"),
        Some(&Value::Object(None))
    );

    // The PawnList counterpart is traversed the same way by engine code. It must preserve the
    // removed pawn's NextPawn link too, not just the controller list's link.
    let p1 = vm.spawn(sg(&set, "Pawn"), "P1").unwrap();
    let p2 = vm.spawn(sg(&set, "Pawn"), "P2").unwrap();
    let p3 = vm.spawn(sg(&set, "Pawn"), "P3").unwrap();
    for pawn in [p1, p2, p3] {
        vm.set_property(
            pawn,
            "Level",
            0,
            Value::Object(Some(ObjRef::Instance(level))),
        );
        call_native(&mut vm, "Engine.Pawn.AddPawnToList", pawn, &[], &mut []);
    }
    let mut visited_pawns = Vec::new();
    let mut current = Some(p3);
    while let Some(pawn) = current {
        visited_pawns.push(pawn);
        call_native(
            &mut vm,
            "Engine.Pawn.RemovePawnFromList",
            pawn,
            &[],
            &mut [],
        );
        current = match vm.get_property(pawn, "NextPawn") {
            Some(Value::Object(Some(ObjRef::Instance(next)))) => Some(*next),
            _ => None,
        };
    }
    assert_eq!(visited_pawns, [p3, p2, p1]);
    assert_eq!(
        vm.get_property(level, "PawnList"),
        Some(&Value::Object(None))
    );
}

/// Synthetic package for a bytecode EndGame-style list walk (no proprietary data):
/// `Actor.WalkControllers`/`WalkPawns` = `P = List; while (P != None) { P.Destroy(); P = P.Next; }`
/// and `Controller.Destroyed` = `RemoveController()` (native 530), `Pawn.Destroyed` =
/// `RemovePawnFromList()` (name-bound native), the same shape as engine.u `Controller.Destroyed`
/// (+0x12 `RemoveController`) and XIIIGameInfo.EndGame (+0x02A8..+0x0336, where
/// `GotoState('GameEnded')` destroys every non-player controller before `P = P.nextController`).
fn list_walk_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let pawn = b.reserve(0, 0, "Pawn");
    let controller = b.reserve(0, 0, "Controller");
    let obj = compact(0);
    let ne = b.reserve(IMP_FUNCTION, object, "NotEqual_ObjectObject");
    let ne_a = b.reserve(IMP_OBJECTPROP, ne, "A");
    let ne_b = b.reserve(IMP_OBJECTPROP, ne, "B");
    let ne_r = b.reserve(IMP_OBJECTPROP, ne, "ReturnValue");
    b.prop_with(ne_a, ne_b, PARM, &obj);
    b.prop_with(ne_b, ne_r, PARM, &obj);
    b.prop_with(ne_r, 0, PARM | RETURN_PARM, &obj);
    b.func(ne, 0, ne_a, &[], 0, 119, FINAL | NATIVE | OPERATOR | STATIC);

    let level = b.reserve(IMP_OBJECTPROP, actor, "Level");
    let pawn_list = b.reserve(IMP_OBJECTPROP, actor, "PawnList");
    let next_pawn = b.reserve(IMP_OBJECTPROP, actor, "NextPawn");
    let controller_list = b.reserve(IMP_OBJECTPROP, actor, "ControllerList");
    let next_controller = b.reserve(IMP_OBJECTPROP, actor, "NextController");
    let destroy = b.reserve(IMP_FUNCTION, actor, "Destroy");
    let walk_c = b.reserve(IMP_FUNCTION, actor, "WalkControllers");
    let walk_c_p = b.reserve(IMP_OBJECTPROP, walk_c, "P");
    let walk_p = b.reserve(IMP_FUNCTION, actor, "WalkPawns");
    let walk_p_p = b.reserve(IMP_OBJECTPROP, walk_p, "P");
    let remove_c = b.reserve(IMP_FUNCTION, controller, "RemoveController");
    let c_destroyed = b.reserve(IMP_FUNCTION, controller, "Destroyed");
    let remove_p = b.reserve(IMP_FUNCTION, pawn, "RemovePawnFromList");
    let p_destroyed = b.reserve(IMP_FUNCTION, pawn, "Destroyed");
    b.prop_with(level, pawn_list, 0, &obj);
    b.prop_with(pawn_list, next_pawn, 0, &obj);
    b.prop_with(next_pawn, controller_list, 0, &obj);
    b.prop_with(controller_list, next_controller, 0, &obj);
    b.prop_with(next_controller, destroy, 0, &obj);
    // `Actor.Destroy` = native 279 (extended opcode 0x61 0x17).
    b.func(destroy, walk_c, 0, &[], 0, 279, FINAL | NATIVE);

    let walk = |list: i32, next: i32, local: i32| -> Vec<u8> {
        let (l, n, p) = (list as u8, next as u8, local as u8);
        #[rustfmt::skip]
        let code = vec![
            0x0F, 0x00, p, 0x01, l,                                   // 0000 P = self.List
            0x07, 0x39, 0x00, 0x77, 0x00, p, 0x2A, 0x16,             // 000B if !(P != None) goto 0x39
            0x19, 0x00, p, 0x03, 0x00, 0x00, 0x61, 0x17, 0x16,       // 0016 P.Destroy()
            0x0F, 0x00, p, 0x19, 0x00, p, 0x05, 0x00, 0x04, 0x01, n, // 0022 P = P.Next
            0x06, 0x0B, 0x00,                                         // 0036 goto 0x0B
            0x04, 0x0B,                                               // 0039 return
        ];
        code
    };
    b.prop_with(walk_c_p, 0, 0, &obj);
    b.prop_with(walk_p_p, 0, 0, &obj);
    let code = walk(controller_list, next_controller, walk_c_p);
    b.func(walk_c, walk_p, walk_c_p, &code, 0x3B, 0, DEFINED);
    let code = walk(pawn_list, next_pawn, walk_p_p);
    b.func(walk_p, 0, walk_p_p, &code, 0x3B, 0, DEFINED);

    // `Controller.RemoveController` = native 530 (extended opcode 0x62 0x12).
    b.func(remove_c, c_destroyed, 0, &[], 0, 530, FINAL | NATIVE);
    // `Destroyed() { RemoveController(); }`
    b.func(
        c_destroyed,
        0,
        0,
        &[0x62, 0x12, 0x16, 0x04, 0x0B],
        5,
        0,
        DEFINED | EVENT,
    );
    // `Pawn.RemovePawnFromList` is name-bound (native index 0): a FinalFunction call.
    b.func(remove_p, p_destroyed, 0, &[], 0, 0, FINAL | NATIVE);
    b.func(
        p_destroyed,
        0,
        0,
        &[0x1C, remove_p as u8, 0x16, 0x04, 0x0B],
        8,
        0,
        DEFINED | EVENT,
    );

    b.class(object, 0, ne, 0);
    b.class(actor, object, level, 0);
    b.class(pawn, actor, remove_p, 0);
    b.class(controller, actor, remove_c, 0);
    b.build()
}

fn list_walk_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        list_walk_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

fn obj_prop(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(ObjRef::Instance(i)))) => Some(*i),
        _ => None,
    }
}

/// Spawns `n` list members of `class`, links them with the add native and returns them in
/// list order (head first).
fn linked_members(
    vm: &mut Vm<'_>,
    set: &ScriptSet,
    level: ObjectId,
    class: &str,
    add_native: &str,
    n: usize,
) -> Vec<ObjectId> {
    let mut members = Vec::new();
    for i in 0..n {
        let m = vm.spawn(sg(set, class), &format!("{class}{i}")).unwrap();
        vm.set_active(m, true);
        vm.set_property(m, "Level", 0, Value::Object(Some(ObjRef::Instance(level))));
        call_native(vm, add_native, m, &[], &mut []);
        members.push(m);
    }
    members.reverse();
    members
}

/// Root cause (item19 Plage01 EndGame): `Vm::destroy` marks the actor deleted before running
/// `Destroyed`, whose `RemoveController()` then has to unlink a node the VM already treats as
/// None. The engine's list natives compare raw pointers (Engine.dll 0x10367ae2..0x10367b1a), so
/// a self-removal from `Destroyed` must unlink the head and a middle node.
#[test]
fn destroyed_event_removes_the_deleted_node_from_the_level_lists() {
    let set = list_walk_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let level = vm.spawn(sg(&set, "Actor"), "Level").unwrap();
    vm.set_active(level, true);
    let cs = linked_members(
        &mut vm,
        &set,
        level,
        "Controller",
        "Engine.Controller.AddController",
        3,
    );
    // Middle node: the predecessor skips it; the destroyed node keeps its successor.
    vm.destroy(cs[1]).unwrap();
    assert!(vm.objects[cs[1] as usize].deleted);
    assert_eq!(obj_prop(&vm, cs[0], "NextController"), Some(cs[2]));
    assert_eq!(obj_prop(&vm, cs[1], "NextController"), Some(cs[2]));
    // Head node.
    vm.destroy(cs[0]).unwrap();
    assert_eq!(obj_prop(&vm, level, "ControllerList"), Some(cs[2]));
    assert_eq!(obj_prop(&vm, cs[0], "NextController"), Some(cs[2]));

    let ps = linked_members(&mut vm, &set, level, "Pawn", "Engine.Pawn.AddPawnToList", 3);
    vm.destroy(ps[0]).unwrap();
    assert_eq!(obj_prop(&vm, level, "PawnList"), Some(ps[1]));
    vm.destroy(ps[2]).unwrap();
    assert_eq!(obj_prop(&vm, ps[1], "NextPawn"), None);
    assert_eq!(obj_prop(&vm, level, "PawnList"), Some(ps[1]));
}

/// Bytecode walk that destroys the current controller/pawn and then advances through it, as
/// XIIIGameInfo.EndGame does. Every node must be visited (destroyed) and the lists end empty;
/// with the old VM the walk stopped after the first node (unlink failed and `P.NextController`
/// on the destroyed `P` read as Accessed None), so the local player controller later in the list
/// never received `GameEnded`.
#[test]
fn script_list_walk_survives_self_destroying_nodes() {
    let set = list_walk_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let level = vm.spawn(sg(&set, "Actor"), "Level").unwrap();
    vm.set_active(level, true);
    let cs = linked_members(
        &mut vm,
        &set,
        level,
        "Controller",
        "Engine.Controller.AddController",
        3,
    );
    vm.call_function(sg(&set, "Actor.WalkControllers"), level, Vec::new())
        .unwrap();
    for &c in &cs {
        assert!(
            vm.objects[c as usize].deleted,
            "{} was not visited by the walk",
            vm.objects[c as usize].name
        );
    }
    assert_eq!(obj_prop(&vm, level, "ControllerList"), None);
    assert!(
        !vm.trace
            .iter()
            .any(|e| matches!(e.kind, TraceKind::AccessedNone { .. })),
        "reading NextController through a just-destroyed controller is not Accessed None in UE2"
    );

    let ps = linked_members(&mut vm, &set, level, "Pawn", "Engine.Pawn.AddPawnToList", 3);
    vm.call_function(sg(&set, "Actor.WalkPawns"), level, Vec::new())
        .unwrap();
    for &p in &ps {
        assert!(vm.objects[p as usize].deleted);
    }
    assert_eq!(obj_prop(&vm, level, "PawnList"), None);
}

/// The engine list natives dereference `Level` unconditionally; the VM must not silently edit
/// links when it is None (the old code cleared the node's own `NextController`).
#[test]
fn list_removal_without_level_leaves_links_and_records_a_note() {
    let set = list_walk_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Controller"), "A").unwrap();
    let b2 = vm.spawn(sg(&set, "Controller"), "B").unwrap();
    vm.set_property(
        a,
        "NextController",
        0,
        Value::Object(Some(ObjRef::Instance(b2))),
    );
    call_native(
        &mut vm,
        "Engine.Controller.RemoveController",
        a,
        &[],
        &mut [],
    );
    assert_eq!(obj_prop(&vm, a, "NextController"), Some(b2));
    assert!(vm.trace.iter().any(|e| matches!(
        &e.kind,
        TraceKind::Note(n) if n.contains("ControllerList") && n.contains("Level is None")
    )));
}

/// Synthetic package for the destroyed-actor access rules (item25). `Actor` has a `V` int, a
/// `Next` object property and functions exercising every access form through a context:
/// - `GetSelfV() -> int`      `return self.V`
/// - `SetV(int x)`            `self.V = x`
/// - `GetThrough(other) -> int`  `return other.V`        (plain variable read)
/// - `SetThrough(other, x)`   `other.V = x`               (plain variable write)
/// - `CallThrough(other) -> int` `return other.Bump()`    (call through a context)
/// - `Bump() -> int`          `self.V = self.V + 1; return self.V`
/// - `Destroyed() -> int`     the script `Destroyed` handler: `self.NestedDestroy(); return self.V`
///   (it returns a value only so a test can observe the actor's state during the event)
/// - `NestedDestroy()`        `self.Destroy()`            (nested destroy from `Destroyed`)
///
/// `Destroyed` is the script event handler (the VM ignores an event handler's return value).
fn destroyed_access_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let v = b.reserve(IMP_INTPROP, actor, "V");
    let seen = b.reserve(IMP_INTPROP, actor, "Seen");
    let next = b.reserve(IMP_OBJPROP, actor, "Next");
    let get_self_v = b.reserve(IMP_FUNCTION, actor, "GetSelfV");
    let set_v = b.reserve(IMP_FUNCTION, actor, "SetV");
    let get_through = b.reserve(IMP_FUNCTION, actor, "GetThrough");
    let set_through = b.reserve(IMP_FUNCTION, actor, "SetThrough");
    let call_through = b.reserve(IMP_FUNCTION, actor, "CallThrough");
    let bump = b.reserve(IMP_FUNCTION, actor, "Bump");
    let destroyed_v = b.reserve(IMP_FUNCTION, actor, "Destroyed");
    let nested = b.reserve(IMP_FUNCTION, actor, "NestedDestroy");
    let destroy = b.reserve(IMP_FUNCTION, actor, "Destroy");

    let set_v_x = b.reserve(IMP_INTPROP, set_v, "X");
    let get_through_other = b.reserve(IMP_OBJPROP, get_through, "Other");
    let set_through_other = b.reserve(IMP_OBJPROP, set_through, "Other");
    let set_through_x = b.reserve(IMP_INTPROP, set_through, "X");
    let call_through_other = b.reserve(IMP_OBJPROP, call_through, "Other");
    let bump_ret = b.reserve(IMP_INTPROP, bump, "ReturnValue");
    let destroyed_ret = b.reserve(IMP_INTPROP, destroyed_v, "ReturnValue");
    let nested_ret = b.reserve(IMP_BOOLPROP, nested, "ReturnValue");

    // The class child chain is linked through each export's `next` argument of `prop`/`func`
    // (separate calls do not accumulate): v -> next -> GetSelfV -> SetV -> GetThrough ->
    // SetThrough -> CallThrough -> Bump -> DestroyedV -> NestedDestroy -> Destroy.
    let null_obj = compact(0);
    b.prop(v, seen, 0);
    b.prop(seen, next, 0);
    b.prop_with(next, get_self_v, 0, &null_obj);

    // Function parameter chains, linked through each property's `next`.
    b.prop(set_v_x, 0, PARM);
    b.prop_with(set_through_other, set_through_x, PARM, &null_obj);
    b.prop(set_through_x, 0, PARM);
    b.prop_with(get_through_other, 0, PARM, &null_obj);
    b.prop_with(call_through_other, 0, PARM, &null_obj);
    b.prop(bump_ret, 0, RETURN_PARM);
    b.prop(destroyed_ret, 0, RETURN_PARM);
    b.prop(nested_ret, 0, RETURN_PARM);

    let rv = v as u8;
    let rseen = seen as u8;
    let rx = set_v_x as u8;
    let ro = get_through_other as u8;
    let co = call_through_other as u8;
    let so = set_through_other as u8;
    let sx = set_through_x as u8;

    // `return self.V` (GetSelfV).
    b.func(get_self_v, set_v, 0, &[0x04, 0x01, rv], 6, 0, DEFINED);

    // `self.V = X` (SetV).
    b.func(
        set_v,
        get_through,
        set_v_x,
        &[0x0F, 0x01, rv, 0x00, rx, 0x04, 0x0B],
        13,
        0,
        DEFINED,
    );

    // `return Other.V` (GetThrough).
    b.func(
        get_through,
        set_through,
        get_through_other,
        &[0x04, 0x19, 0x00, ro, 0xFF, 0xFF, 0x00, 0x01, rv],
        15,
        0,
        DEFINED,
    );

    // `Other.V = X; return` (SetThrough).
    b.func(
        set_through,
        call_through,
        set_through_other,
        &[
            0x0F, 0x19, 0x00, so, 0xFF, 0xFF, 0x00, 0x01, rv, 0x00, sx, 0x04, 0x0B,
        ],
        22,
        0,
        DEFINED,
    );

    // `return Other.Bump()` (CallThrough): `Return(Context Other -> VirtualFunction Bump())`.
    let bump_name = b.name("Bump");
    let call_code = [0x04, 0x19, 0x00, co, 0xFF, 0xFF, 0x00, 0x1B]
        .into_iter()
        .chain(compact(bump_name))
        .chain([0x16, 0x0B])
        .collect::<Vec<u8>>();
    b.func(
        call_through,
        bump,
        call_through_other,
        &call_code,
        17,
        0,
        DEFINED,
    );

    // `self.V = 3; return self.V` (Bump). A fixed mutation avoids a native operator dependency.
    let bump_code = vec![
        0x0F, 0x01, rv, 0x2C, 3, // 0000 V = 3
        0x04, 0x01, rv, // 0006 return V
    ];
    b.func(bump, destroyed_v, bump_ret, &bump_code, 14, 0, DEFINED);

    // `self.NestedDestroy(); return self.V` (Destroyed), the script event handler.
    // It exercises both the read-through-destroyed rule and the nested `Destroy` guard.
    let destroyed_code = [
        0x0F,
        0x01,
        rseen,
        0x01,
        rv, // Seen = V
        0x1C,
        nested as u8,
        0x16, // self.NestedDestroy()
        0x04,
        0x01,
        rv, // return V
    ]
    .to_vec();
    b.func(
        destroyed_v,
        nested,
        destroyed_ret,
        &destroyed_code,
        23,
        0,
        DEFINED | EVENT,
    );

    // `return self.Destroy()` (NestedDestroy): native 279, a FinalFunction call.
    b.func(
        nested,
        destroy,
        nested_ret,
        &[0x61, 0x17, 0x16, 0x04, 0x0B],
        5,
        0,
        DEFINED,
    );
    // `Actor.Destroy` native 279.
    b.func(destroy, 0, 0, &[], 0, 279, FINAL | NATIVE);

    b.class(object, 0, 0, 0);
    b.class(actor, object, v, 0);
    b.build()
}

fn destroyed_access_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        destroyed_access_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

/// item25: the engine runs `ULevel::DestroyActor` in the order state-clear, `Destroyed`, then
/// `bDeleteMe` (`Engine.dll` 0x103890e0, `Destroyed` at 0x103893f7-0x10389429, `orl $0x10000,
/// 0x2c` at 0x1038965a). A plain variable read through the actor still works **inside** its own
/// `Destroyed` (the engine has not set `bDeleteMe` yet).
#[test]
fn destroyed_event_can_still_read_the_actor() {
    let set = destroyed_access_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_property(a, "V", 0, Value::Int(42));
    vm.destroy(a).unwrap();
    // `Destroyed` ran: it copied `self.V` into `self.Seen` while readable (the engine has not set
    // `bDeleteMe` yet), then called the nested `Destroy` (a no-op).
    assert_eq!(
        vm.get_property(a, "Seen"),
        Some(&Value::Int(42)),
        "Destroyed must read self.V"
    );
    assert!(vm.objects[a as usize].deleted);
    // A nested `Destroy` from inside `Destroyed` is a no-op (the engine's `bDeleteMe` gate).
    assert!(
        !vm.trace.iter().any(|e| matches!(
            &e.kind,
            TraceKind::Note(n) if n.contains("skipped")
        )),
        "the Destroyed event must run, not be skipped"
    );
}

/// item25: through a just-destroyed actor (`bDeleteMe` set, not yet cleaned), a plain variable
/// read and a write still work; `execContext` has no `bDeleteMe` test (`Core.dll` 0x101173d3).
#[test]
fn destroyed_actor_plain_variable_read_and_write_still_work() {
    let set = destroyed_access_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let b = vm.spawn(sg(&set, "Actor"), "B").unwrap();
    vm.set_active(a, true);
    vm.set_property(b, "V", 0, Value::Int(7));
    vm.destroy(b).unwrap();

    let get = sg(&set, "Actor.GetThrough");
    let val = vm
        .call_function(get, a, vec![Value::Object(Some(ObjRef::Instance(b)))])
        .unwrap();
    assert_eq!(val, Value::Int(7), "stale V is still readable");

    let set_fn = sg(&set, "Actor.SetThrough");
    vm.call_function(
        set_fn,
        a,
        vec![Value::Object(Some(ObjRef::Instance(b))), Value::Int(9)],
    )
    .unwrap();
    assert_eq!(
        vm.get_property(b, "V"),
        Some(&Value::Int(9)),
        "a write through a just-destroyed actor lands"
    );
}

/// item25: a call through a just-destroyed actor runs (`execVirtualFunction` -> `CallFunction`
/// has no `bDeleteMe` gate), unlike an engine-delivered event (`ProcessEvent` skips it).
#[test]
fn destroyed_actor_call_through_context_runs() {
    let set = destroyed_access_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    let b = vm.spawn(sg(&set, "Actor"), "B").unwrap();
    vm.set_active(a, true);
    vm.set_active(b, true);
    vm.set_property(b, "V", 0, Value::Int(5));
    vm.destroy(b).unwrap();

    let call = sg(&set, "Actor.CallThrough");
    let val = vm
        .call_function(call, a, vec![Value::Object(Some(ObjRef::Instance(b)))])
        .unwrap();
    assert_eq!(val, Value::Int(3), "Bump() ran on the destroyed actor");
    assert_eq!(vm.get_property(b, "V"), Some(&Value::Int(3)));
}

/// item25: an engine-delivered event to a `bDeleteMe` actor is dropped (ProcessEvent), recorded
/// visibly, while a script call still runs (previous test).
#[test]
fn deleted_actor_event_is_dropped_and_traced() {
    let set = destroyed_access_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.destroy(a).unwrap();
    let before = vm.get_property(a, "V").cloned();
    // `Bump` is a script function, but deliver it as an event the way the engine would.
    let r = vm.send_event(a, "Bump", Vec::new()).unwrap();
    assert!(r.is_none(), "event to a bDeleteMe actor is not delivered");
    assert_eq!(vm.get_property(a, "V").cloned(), before);
    assert!(vm.trace.iter().any(|e| matches!(
        &e.kind,
        TraceKind::Note(n) if n.contains("bDeleteMe")
    )));
}

/// item25: `Disable('All')`/`bProbesDisabled` suppresses **all** events, including `Destroyed`
/// (the engine's ProcessEvent probe check), and `Enable('All')` restores them.
#[test]
fn probes_disabled_all_suppresses_destroyed_event() {
    let set = destroyed_access_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    call_native(
        &mut vm,
        "Object.Disable",
        a,
        &[],
        &mut [Value::Name("All".into())],
    );
    assert!(vm.probes_disabled(a));
    assert_eq!(
        vm.call_function(sg(&set, "Actor.Destroy"), a, Vec::new())
            .unwrap(),
        Value::Bool(true)
    );
    assert!(vm.objects[a as usize].deleted);
    // `Destroyed` was not delivered: the probe is disabled, so no `DestroyedV` event ran. The
    // fixture has no native `Destroyed`, so a delivered event would be an `EVENT`/`NO HANDLER`
    // record rather than a `Probe` one; assert the event function did not run.
    assert!(
        !vm.trace.iter().any(|e| matches!(
            &e.kind,
            TraceKind::Event { function, .. } if function.ends_with("Destroyed")
        )),
        "a probes-disabled actor must not run Destroyed"
    );
}

/// item25: a nested `Destroy` from inside `Destroyed` is a no-op: the engine has not returned from
/// `DestroyActor` yet but `bDeleteMe` is treated as set for re-entry, and the second call returns
/// 1 without re-running the event (idempotent).
#[test]
fn nested_destroy_during_destroyed_is_idempotent() {
    let set = destroyed_access_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    // NestedDestroy's `self.Destroy()` would recurse without the guard.
    vm.destroy(a).unwrap();
    assert!(vm.objects[a as usize].deleted);
    assert!(!vm.is_destroying(a));
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
    let decoration = b.reserve(0, 0, "Decoration");
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
    let proj_target = b.reserve(IMP_BOOLPROP, actor, "bProjTarget");
    let block_zero = b.reserve(IMP_BOOLPROP, actor, "bBlockZeroExtentTraces");
    let block_nonzero = b.reserve(IMP_BOOLPROP, actor, "bBlockNonZeroExtentTraces");
    let movable = b.reserve(IMP_BOOLPROP, actor, "bMovable");
    let bstatic = b.reserve(IMP_BOOLPROP, actor, "bStatic");
    let proj_target = b.reserve(IMP_BOOLPROP, actor, "bProjTarget");
    let hidden = b.reserve(IMP_BOOLPROP, actor, "bHidden");
    let world_geometry = b.reserve(IMP_BOOLPROP, actor, "bWorldGeometry");
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
    b.prop(block_players, proj_target, 0);
    b.prop(proj_target, block_zero, 0);
    b.prop(block_zero, block_nonzero, 0);
    b.prop(block_nonzero, proj_target, 0);
    b.prop(proj_target, hidden, 0);
    b.prop(hidden, world_geometry, 0);
    b.prop(world_geometry, movable, 0);
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
    b.class(decoration, actor, 0, 0);
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
    let physics = b.reserve(IMP_BYTEPROP, actor, "Physics");
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
    b.prop(height, physics, 0);
    b.prop_with(physics, 0, 0, &compact(0));
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

/// Redistributable item46 fixture: real property types, no installation bytes.
fn item46_set() -> ScriptSet {
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let pawn = b.reserve(0, 0, "Pawn");
    let controller = b.reserve(0, 0, "IAController");
    let controller_base = b.reserve(0, 0, "Controller");
    let move_toward = b.reserve(IMP_FUNCTION, controller_base, "MoveToward");
    let new_target = b.reserve(IMP_OBJECTPROP, move_toward, "NewTarget");
    let view_focus = b.reserve(IMP_OBJECTPROP, move_toward, "ViewFocus");
    let speed_arg = b.reserve(IMP_FLOATPROP, move_toward, "Speed");
    let next_target = b.reserve(IMP_OBJECTPROP, move_toward, "NextTarget");
    b.prop_with(new_target, view_focus, pf::PARM, &compact(actor));
    b.prop_with(
        view_focus,
        speed_arg,
        pf::PARM | pf::OPTIONAL_PARM,
        &compact(actor),
    );
    b.prop(speed_arg, next_target, pf::PARM | pf::OPTIONAL_PARM);
    b.prop_with(
        next_target,
        0,
        pf::PARM | pf::OPTIONAL_PARM,
        &compact(actor),
    );
    b.func(
        move_toward,
        0,
        new_target,
        &[],
        0,
        502,
        ff::FINAL | ff::NATIVE | ff::LATENT,
    );
    let moving = b.reserve(IMP_STATE, controller, "Moving");
    let level = b.reserve(0, 0, "LevelInfo");
    let node = b.reserve(0, 0, "NavigationPoint");
    let music_entry = b.reserve(IMP_STRUCT_CLASS, 0, "MusicEntry");
    let music_value = b.reserve(IMP_INTPROP, music_entry, "Value");
    b.prop(music_value, 0, 0);
    b.strukt(music_entry, music_value);
    let music_inner = b.reserve(IMP_STRUCTPROP, actor, "MusicInner");
    b.prop_with(music_inner, 0, 0, &compact(music_entry));
    let soldier_inner = b.reserve(IMP_OBJECTPROP, actor, "SoldierInner");
    b.prop_with(soldier_inner, 0, 0, &compact(pawn));
    let alliance_entry_ref = b.reserve(IMP_STRUCT_CLASS, 0, "AllianceEntry");
    let alliance_name_ref = b.reserve(IMP_NAMEPROP, alliance_entry_ref, "AllianceName");
    let alliance_level_ref = b.reserve(IMP_FLOATPROP, alliance_entry_ref, "AllianceLevel");
    b.prop(alliance_name_ref, alliance_level_ref, 0);
    b.prop(alliance_level_ref, 0, 0);
    b.strukt(alliance_entry_ref, alliance_name_ref);
    let mut fields = Vec::new();
    for name in [
        "Location",
        "Destination",
        "FocalPoint",
        "WeaponStartTrace",
        "LastSeenPos",
        "WeaponEndTrace",
        "Velocity",
        "Acceleration",
    ] {
        fields.push((b.reserve(IMP_STRUCTPROP, actor, name), compact(IMP_STRUCT)));
    }
    for name in [
        "CollisionRadius",
        "CollisionHeight",
        "BaseEyeHeight",
        "GroundSpeed",
        "WalkingPct",
        "CrouchingPct",
        "DesiredSpeed",
        "MaxDesiredSpeed",
        "AccelRate",
        "GroundFriction",
        "MoveTimer",
        "TacticalOffset",
    ] {
        fields.push((b.reserve(IMP_FLOATPROP, actor, name), vec![]));
    }
    for name in [
        "bCollideActors",
        "bCollideWorld",
        "bBlockZeroExtentTraces",
        "bIsDead",
        "bCanSeeThrough",
        "bInstantHit",
        "bCanShootThroughWithRayCastingWeapon",
        "bCanShootThroughWithProjectileWeapon",
        "bWalking",
        "bReducedSpeed",
        "bIsCrouched",
        "bAdjusting",
        "bAdvancedTactics",
        "bPreparingMove",
    ] {
        fields.push((b.reserve(IMP_BOOLPROP, actor, name), vec![]));
    }
    for name in [
        "Pawn",
        "Enemy",
        "Level",
        "BaseS",
        "XIII",
        "GenAlerte",
        "Pote",
        "Weapon",
        "AmmoType",
        "MoveTarget",
        "Focus",
        "PhysicsVolume",
        "NavigationPointList",
        "NextNavigationPoint",
        "NextMoveTarget",
    ] {
        fields.push((b.reserve(IMP_OBJECTPROP, actor, name), compact(actor)));
    }
    fields.push((b.reserve(IMP_BYTEPROP, actor, "Physics"), compact(0)));
    fields.push((b.reserve(IMP_NAMEPROP, actor, "Alliance"), vec![]));
    fields.push((
        b.reserve(IMP_ARRAYPROP, actor, "MusicVars"),
        compact(music_inner),
    ));
    fields.push((
        b.reserve(IMP_ARRAYPROP, actor, "SoldierInFightList"),
        compact(soldier_inner),
    ));
    let alliances = b.reserve(IMP_STRUCTPROP, actor, "InitialAlliances");
    for (i, (field, extra)) in fields.iter().enumerate() {
        b.prop_with(
            *field,
            fields.get(i + 1).map_or(alliances, |next| next.0),
            0,
            extra,
        );
    }
    b.prop_array_dim(alliances, 0, 0, alliance_entry_ref, 4);
    let target_ref = fields
        .iter()
        .find(|(id, _)| b.names[b.exports[*id as usize - 1].name as usize] == "MoveTarget")
        .unwrap()
        .0;
    let mut code = vec![0x61, 0xf6, 0x01];
    code.extend(compact(target_ref));
    code.extend([0x0b, 0x0b, 0x0b, 0x16, 0x08, 0x0c]);
    code.extend(compact(b.name("Begin")));
    code.extend(0u32.to_le_bytes());
    code.extend(compact(0));
    code.extend(0u32.to_le_bytes());
    b.state(moving, 0, &code, 29, 12);
    b.class(object, 0, 0, 0);
    b.class(actor, object, fields[0].0, 0);
    b.class(controller_base, actor, move_toward, 0);
    b.class(controller, controller_base, moving, 0);
    for class in [pawn, level, node] {
        b.class(class, actor, 0, 0);
    }
    let p = ScriptPackage::load(
        "Test",
        b.build(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("synthetic package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

fn item46_pair(vm: &mut Vm<'_>, set: &ScriptSet) -> (ObjectId, ObjectId) {
    let ctrl = vm.spawn(pg(set, "IAController"), "Controller").unwrap();
    let pawn = vm.spawn(pg(set, "Pawn"), "Soldier").unwrap();
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    vm.set_property(
        ctrl,
        "BaseS",
        0,
        Value::Object(Some(ObjRef::Instance(pawn))),
    );
    vm.set_property(pawn, "Location", 0, Value::Vector([0.0; 3]));
    vm.set_property(pawn, "CollisionRadius", 0, Value::Float(1.0));
    vm.set_property(pawn, "CollisionHeight", 0, Value::Float(40.0));
    (ctrl, pawn)
}

#[test]
fn item46_attack_counters_are_indexed_wrapping_and_reject_missing_records() {
    let set = item46_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let level = vm.spawn(pg(&set, "LevelInfo"), "Level").unwrap();
    let record = |n| Value::Struct(vec![("value".into(), Value::Int(n))]);
    vm.set_property(
        level,
        "MusicVars",
        0,
        Value::Array(vec![record(12), record(34), record(0)]),
    );
    for (native, expected) in [
        ("Engine.LevelInfo.DecAttaque", -1),
        ("Engine.LevelInfo.IncAttaque", 0),
    ] {
        call_native(&mut vm, native, level, &[], &mut []);
        assert_eq!(
            vm.get_property(level, "MusicVars"),
            Some(&Value::Array(vec![
                record(12),
                record(34),
                record(expected)
            ]))
        );
    }
    vm.set_property(
        level,
        "MusicVars",
        0,
        Value::Array(vec![record(12), record(34), record(i32::MAX)]),
    );
    call_native(&mut vm, "Engine.LevelInfo.IncAttaque", level, &[], &mut []);
    assert_eq!(
        vm.get_property(level, "MusicVars"),
        Some(&Value::Array(vec![
            record(12),
            record(34),
            record(i32::MIN)
        ]))
    );
    vm.set_property(level, "MusicVars", 0, Value::Array(vec![]));
    for native in ["Engine.LevelInfo.IncAttaque", "Engine.LevelInfo.DecAttaque"] {
        assert!(try_native(&mut vm, native, level, &[], &mut []).is_err());
    }
}

#[test]
fn item46_pseudo_steering_preserves_retail_equality_gate_and_symmetric_trace_correction() {
    let set = item46_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([50.0, -100.0, -100.0], [51.0, 100.0, 100.0]),
    ));
    let (ctrl, pawn) = item46_pair(&mut vm, &set);
    assert_eq!(
        call_native(&mut vm, "IAController.PseudoSteering", ctrl, &[], &mut []),
        NativeOutcome::Value(Value::Vector([0.0; 3]))
    );
    let group = vm.spawn(pg(&set, "Actor"), "Group").unwrap();
    vm.set_property(
        ctrl,
        "GenAlerte",
        0,
        Value::Object(Some(ObjRef::Instance(group))),
    );
    let left = vm.spawn(pg(&set, "Pawn"), "Left").unwrap();
    let right = vm.spawn(pg(&set, "Pawn"), "Right").unwrap();
    vm.set_property(left, "Location", 0, Value::Vector([-200.0, 0.0, 0.0]));
    vm.set_property(right, "Location", 0, Value::Vector([200.0, 0.0, 0.0]));
    let members = |ids: &[ObjectId]| {
        Value::Array(
            ids.iter()
                .map(|id| Value::Object(Some(ObjRef::Instance(*id))))
                .collect(),
        )
    };
    vm.set_property(group, "SoldierInFightList", 0, members(&[right]));
    assert_eq!(
        call_native(&mut vm, "IAController.PseudoSteering", ctrl, &[], &mut []),
        NativeOutcome::Value(Value::Vector([0.0; 3]))
    );
    vm.set_property(
        group,
        "SoldierInFightList",
        0,
        members(&[left, pawn, right]),
    );
    let NativeOutcome::Value(Value::Vector(result)) =
        call_native(&mut vm, "IAController.PseudoSteering", ctrl, &[], &mut [])
    else {
        panic!("vector");
    };
    for (actual, expected) in result.into_iter().zip([-62.5, 0.0, 37.5]) {
        assert!((actual - expected).abs() < 0.001, "{result:?}");
    }
    // Coincident member causes an unordered sum, rejected by the same equality gate.
    vm.set_property(right, "Location", 0, Value::Vector([0.0; 3]));
    vm.set_property(group, "SoldierInFightList", 0, members(&[right]));
    assert_eq!(
        call_native(&mut vm, "IAController.PseudoSteering", ctrl, &[], &mut []),
        NativeOutcome::Value(Value::Vector([0.0; 3]))
    );
    vm.set_property(group, "SoldierInFightList", 0, members(&[]));
    assert!(try_native(&mut vm, "IAController.PseudoSteering", ctrl, &[], &mut []).is_err());
}

#[test]
fn item46_stake_out_excludes_boundaries_preserves_failure_and_detects_cycles() {
    let set = item46_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let (ctrl, pawn) = item46_pair(&mut vm, &set);
    vm.set_property(ctrl, "LastSeenPos", 0, Value::Vector([7.0; 3]));
    vm.set_property(ctrl, "FocalPoint", 0, Value::Vector([9.0; 3]));
    call_native(
        &mut vm,
        "IAController.FindNewStakeOutDir",
        ctrl,
        &[],
        &mut [],
    );
    assert_eq!(vm.vector_prop(ctrl, "LastSeenPos"), Some([7.0; 3]));
    let enemy = vm.spawn(pg(&set, "Pawn"), "Enemy").unwrap();
    vm.set_property(enemy, "Location", 0, Value::Vector([1000.0, 0.0, 0.0]));
    vm.set_property(
        ctrl,
        "Enemy",
        0,
        Value::Object(Some(ObjRef::Instance(enemy))),
    );
    let level = vm.spawn(pg(&set, "LevelInfo"), "Level").unwrap();
    vm.set_property(
        ctrl,
        "Level",
        0,
        Value::Object(Some(ObjRef::Instance(level))),
    );
    let mut nodes = Vec::new();
    for (i, location) in [
        [100.0, 0.0, 0.0],
        [800.0, 0.0, 0.0],
        [200.0, 100.0, 0.0],
        [200.0, 0.0, 0.0],
        [300.0, 0.0, 0.0],
    ]
    .into_iter()
    .enumerate()
    {
        let node = vm
            .spawn(pg(&set, "NavigationPoint"), &format!("Node{i}"))
            .unwrap();
        vm.set_property(node, "Location", 0, Value::Vector(location));
        if let Some(previous) = nodes.last() {
            vm.set_property(
                *previous,
                "NextNavigationPoint",
                0,
                Value::Object(Some(ObjRef::Instance(node))),
            );
        }
        nodes.push(node);
    }
    vm.set_property(
        level,
        "NavigationPointList",
        0,
        Value::Object(Some(ObjRef::Instance(nodes[0]))),
    );
    call_native(
        &mut vm,
        "IAController.FindNewStakeOutDir",
        ctrl,
        &[],
        &mut [],
    );
    // First equally aligned eligible node wins; neither excluded endpoint wins.
    assert_eq!(
        vm.vector_prop(ctrl, "LastSeenPos"),
        Some([200.0, 0.0, 20.0])
    );
    vm.set_property(
        nodes[4],
        "NextNavigationPoint",
        0,
        Value::Object(Some(ObjRef::Instance(nodes[0]))),
    );
    assert!(
        try_native(
            &mut vm,
            "IAController.FindNewStakeOutDir",
            ctrl,
            &[],
            &mut []
        )
        .is_err()
    );
    vm.set_property(nodes[4], "NextNavigationPoint", 0, Value::Object(None));
    vm.set_property(pawn, "Location", 0, Value::Vector([5000.0, 0.0, 0.0]));
    call_native(
        &mut vm,
        "IAController.FindNewStakeOutDir",
        ctrl,
        &[],
        &mut [],
    );
    assert_eq!(
        vm.vector_prop(ctrl, "LastSeenPos"),
        Some([200.0, 0.0, 20.0])
    );
    assert_eq!(
        vm.vector_prop(ctrl, "FocalPoint"),
        Some([9.0; 3]),
        "stake-out changes remembered target, not FocalPoint"
    );
}

#[test]
fn item46_line_of_fire_classifies_ally_hostile_dead_and_shoot_through() {
    let set = item46_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let (ctrl, pawn) = item46_pair(&mut vm, &set);
    assert!(
        try_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        )
        .is_err()
    );
    let weapon = vm.spawn(pg(&set, "Actor"), "Weapon").unwrap();
    let ammo = vm.spawn(pg(&set, "Actor"), "Ammo").unwrap();
    vm.set_property(
        pawn,
        "Weapon",
        0,
        Value::Object(Some(ObjRef::Instance(weapon))),
    );
    vm.set_property(
        weapon,
        "AmmoType",
        0,
        Value::Object(Some(ObjRef::Instance(ammo))),
    );
    vm.set_property(ctrl, "WeaponEndTrace", 0, Value::Vector([200.0, 0.0, 0.0]));
    let other = vm.spawn(pg(&set, "Pawn"), "Other").unwrap();
    vm.set_property(other, "Location", 0, Value::Vector([100.0, 0.0, 0.0]));
    vm.set_property(other, "CollisionRadius", 0, Value::Float(10.0));
    vm.set_property(other, "CollisionHeight", 0, Value::Float(40.0));
    vm.set_property(other, "bBlockZeroExtentTraces", 0, Value::Bool(true));
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        ),
        NativeOutcome::Value(Value::Int(1))
    );
    assert_eq!(vm.obj_prop(ctrl, "Pote"), Some(other));
    vm.set_property(other, "bIsDead", 0, Value::Bool(true));
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        ),
        NativeOutcome::Value(Value::Int(0))
    );
    assert_eq!(vm.obj_prop(ctrl, "Pote"), Some(other)); // zeros do not clear Pote
    vm.set_property(other, "bIsDead", 0, Value::Bool(false));
    vm.set_property(other, "Alliance", 0, Value::Name("Hostile".into()));
    vm.set_property(pawn, "InitialAlliances", 0, alliance_entry("Hostile", -1.0));
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        ),
        NativeOutcome::Value(Value::Int(0))
    );
    vm.set_property(other, "bBlockZeroExtentTraces", 0, Value::Bool(false));
    let obstacle = vm.spawn(pg(&set, "Actor"), "Glass").unwrap();
    for name in ["Location", "CollisionRadius", "CollisionHeight"] {
        vm.set_property(
            obstacle,
            name,
            0,
            vm.get_property(other, name).unwrap().clone(),
        );
    }
    vm.set_property(obstacle, "bBlockZeroExtentTraces", 0, Value::Bool(true));
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        ),
        NativeOutcome::Value(Value::Int(2))
    );
    vm.set_property(obstacle, "bCanSeeThrough", 0, Value::Bool(true));
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        ),
        NativeOutcome::Value(Value::Int(0))
    );
    vm.set_property(obstacle, "bCanSeeThrough", 0, Value::Bool(false));
    vm.set_property(
        obstacle,
        "bCanShootThroughWithProjectileWeapon",
        0,
        Value::Bool(true),
    );
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        ),
        NativeOutcome::Value(Value::Int(0))
    );
    vm.set_property(ammo, "bInstantHit", 0, Value::Bool(true));
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        ),
        NativeOutcome::Value(Value::Int(2))
    );
    vm.set_property(
        obstacle,
        "bCanShootThroughWithRayCastingWeapon",
        0,
        Value::Bool(true),
    );
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.LineOfFireObstacle",
            ctrl,
            &[],
            &mut []
        ),
        NativeOutcome::Value(Value::Int(0))
    );
}

#[test]
fn item46_move_to_fractional_speed_accelerates_and_none_physics_does_not_teleport() {
    let set = item46_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let (ctrl, pawn) = item46_pair(&mut vm, &set);
    for (name, value) in [
        ("GroundSpeed", 100.0),
        ("AccelRate", 100.0),
        ("WalkingPct", 0.5),
        ("MaxDesiredSpeed", 1.0),
    ] {
        vm.set_property(pawn, name, 0, Value::Float(value));
    }
    let destination = [300.0, 0.0, 0.0];
    vm.set_property(pawn, "Physics", 0, Value::Byte(1));
    let mut args = [
        Value::Vector(destination),
        Value::Object(None),
        Value::Float(0.25),
    ];
    call_native_stateful(
        &mut vm,
        "Engine.Controller.MoveTo",
        ctrl,
        &[false; 3],
        &mut args,
    );
    assert_eq!(vm.get_property(pawn, "bWalking"), Some(&Value::Bool(true)));
    assert_eq!(vm.f32_prop(pawn, "DesiredSpeed"), 0.5);
    assert_eq!(vm.f32_prop(ctrl, "MoveTimer"), 13.0);
    vm.set_property(pawn, "Physics", 0, Value::Byte(0));
    assert!(
        !vm.controller_move_step(ctrl, pawn, destination, 0.1)
            .unwrap()
    );
    assert_eq!(vm.vector_prop(pawn, "Location"), Some([0.0; 3]));
    assert_eq!(
        vm.vector_prop(pawn, "Acceleration"),
        Some([100.0, 0.0, 0.0])
    );
    vm.set_property(pawn, "Physics", 0, Value::Byte(1));
    vm.controller_move_step(ctrl, pawn, destination, 0.1)
        .unwrap();
    assert_eq!(vm.vector_prop(pawn, "Location"), Some([0.5, 0.0, 0.0]));
    assert_eq!(vm.vector_prop(pawn, "Acceleration"), Some([50.0, 0.0, 0.0]));
    for _ in 0..20 {
        vm.controller_move_step(ctrl, pawn, destination, 0.1)
            .unwrap();
    }
    assert_eq!(vm.vector_prop(pawn, "Velocity"), Some([25.0, 0.0, 0.0]));
    // Explicit zero is not an omitted Speed; it has a finite half-second timer.
    args[2] = Value::Float(0.0);
    call_native_stateful(
        &mut vm,
        "Engine.Controller.MoveTo",
        ctrl,
        &[false; 3],
        &mut args,
    );
    assert_eq!(vm.f32_prop(pawn, "DesiredSpeed"), 0.0);
    assert_eq!(vm.f32_prop(ctrl, "MoveTimer"), 0.5);
    args[2] = Value::Float(0.5);
    call_native_stateful(
        &mut vm,
        "Engine.Controller.MoveTo",
        ctrl,
        &[false; 3],
        &mut args,
    );
    assert_eq!(
        vm.get_property(pawn, "bWalking"),
        Some(&Value::Bool(true)),
        "Speed == WalkingPct is walking in the DLL"
    );
    assert_eq!(vm.f32_prop(pawn, "DesiredSpeed"), 1.0);
    assert!(
        vm.start_move(ctrl, destination, f32::NAN, "Controller.MoveTo", true)
            .is_err()
    );
    vm.set_property(pawn, "WalkingPct", 0, Value::Float(0.0));
    assert!(
        vm.start_move(ctrl, destination, 0.0, "Controller.MoveTo", true)
            .is_err()
    );
}

#[test]
fn item46_move_toward_polls_moving_target_and_cancels_when_target_destroyed() {
    let set = item46_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let (ctrl, pawn) = item46_pair(&mut vm, &set);
    for (name, value) in [
        ("GroundSpeed", 100.0),
        ("AccelRate", 100.0),
        ("WalkingPct", 0.5),
        ("MaxDesiredSpeed", 1.0),
    ] {
        vm.set_property(pawn, name, 0, Value::Float(value));
    }
    vm.set_property(pawn, "Physics", 0, Value::Byte(1));
    let target = vm.spawn(pg(&set, "Pawn"), "Target").unwrap();
    vm.set_property(target, "Location", 0, Value::Vector([300.0, 0.0, 0.0]));
    vm.set_property(
        ctrl,
        "MoveTarget",
        0,
        Value::Object(Some(ObjRef::Instance(target))),
    );
    vm.set_active(ctrl, true);
    vm.goto_state(ctrl, "Moving", None).unwrap();
    vm.tick(0.1).unwrap();
    assert_eq!(vm.f32_prop(ctrl, "MoveTimer"), 1.2, "{:?}", vm.trace);
    assert_eq!(vm.obj_prop(ctrl, "Focus"), Some(target));
    vm.set_property(target, "Location", 0, Value::Vector([0.0, 300.0, 0.0]));
    vm.tick(0.1).unwrap();
    assert_eq!(vm.vector_prop(ctrl, "Destination"), Some([0.0, 300.0, 0.0]));
    assert_eq!(vm.vector_prop(pawn, "Location"), Some([0.0, 1.0, 0.0]));
    vm.destroy(target).unwrap();
    vm.tick(0.1).unwrap();
    assert!(vm.trace.iter().any(
        |event| matches!(&event.kind, TraceKind::StateStop { actor } if actor == "Controller")
    ));
    // Explicit None view-focus differs from an omitted argument.
    let replacement = vm.spawn(pg(&set, "Actor"), "Replacement").unwrap();
    vm.set_property(replacement, "Location", 0, Value::Vector([500.0, 0.0, 0.0]));
    let mut args = [
        Value::Object(Some(ObjRef::Instance(replacement))),
        Value::Object(None),
    ];
    call_native_stateful(
        &mut vm,
        "Engine.Controller.MoveToward",
        ctrl,
        &[false, false, true, true],
        &mut args,
    );
    assert_eq!(vm.obj_prop(ctrl, "Focus"), None);
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

#[test]
fn trace_actors_orders_hits_filters_class_and_returns_all_outs() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let caller = phys_actor(&mut vm, &set, "Caller", [0.0; 3]);
    let far = vm.spawn(pg(&set, "Child"), "Far").unwrap();
    vm.set_property(far, "Location", 0, Value::Vector([80.0, 0.0, 0.0]));
    let near = vm.spawn(pg(&set, "Child"), "Near").unwrap();
    vm.set_property(near, "Location", 0, Value::Vector([40.0, 0.0, 0.0]));
    set_collision_fields(&mut vm, far, true, true);
    set_collision_fields(&mut vm, near, true, true);
    // Measured game shape (Plage01): a trace-blocking actor is a projectile target.
    for id in [far, near] {
        vm.set_property(id, "bProjTarget", 0, Value::Bool(true));
    }
    let rows = vm
        .vm_trace_actors(
            caller,
            Some(pg(&set, "Child")),
            [0.0; 3],
            [120.0, 0.0, 0.0],
            [0.0; 3],
        )
        .unwrap();
    assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), [near, far]);
    assert!(rows[0].1[0] < rows[1].1[0]);
    assert!(rows.iter().all(|r| r.2[0] < 0.0));
    assert!(
        vm.vm_trace_actors(
            caller,
            Some(pg(&set, "Child")),
            [0.0; 3],
            [5.0, 0.0, 0.0],
            [0.0; 3]
        )
        .unwrap()
        .is_empty()
    );
}

#[test]
fn item49_view_target_requires_aim_range_and_clear_world_segment() {
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let caller = phys_actor(&mut vm, &set, "Viewer", [0.0, 0.0, 40.0]);
    let target = phys_actor(&mut vm, &set, "Pickup", [100.0, 0.0, 40.0]);
    set_collision_fields(&mut vm, target, true, true);
    let start = [0.0, 0.0, 40.0];
    let trace = |vm: &mut Vm<'_>, end| {
        vm.vm_trace_actors(caller, None, start, end, [0.0; 3])
            .unwrap()
    };
    assert_eq!(trace(&mut vm, [160.0, 0.0, 40.0])[0].0, target);
    assert!(
        trace(&mut vm, [-160.0, 0.0, 40.0]).is_empty(),
        "looking away cannot target a named pickup"
    );
    vm.set_property(target, "Location", 0, Value::Vector([200.0, 0.0, 40.0]));
    assert!(
        trace(&mut vm, [160.0, 0.0, 40.0]).is_empty(),
        "out-of-reach pickup"
    );
    // An explicit synthetic floor occludes the pickup below it.
    vm.set_physics(Box::new(
        MockWorld::new().with_wall([-100.0, -100.0, -1.0], [100.0, 100.0, 1.0]),
    ));
    vm.set_property(target, "Location", 0, Value::Vector([0.0, 0.0, -40.0]));
    assert!(
        trace(&mut vm, [0.0, 0.0, -120.0]).is_empty(),
        "world geometry occludes interaction"
    );
}

/// Item47b: the decoded engine actor-trace filter (`trace_admits_actor`). Each case maps to a
/// measured Engine.dll behavior cited in the function's documentation.
#[test]
fn trace_filter_parked_actor_with_extent_flag_is_not_hit() {
    // `IAController.faction.BeginState` parks a pawn with `SetCollision(false,false,false)` while
    // its `bBlockZeroExtentTraces` stays true (measured on Plage01 `BaseSoldier6`). Hash
    // membership (`bCollideActors`) gates candidacy — the old "extent OR bCollideActors"
    // heuristic wrongly let parked soldiers block bullets.
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, true);
    let parked = phys_actor(&mut vm, &set, "Parked", [40.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, parked, false, true);
    let (hit, location, _) = vm
        .vm_trace(tracer, [0.0, 0.0, 0.0], [200.0, 0.0, 0.0], true, [0.0; 3])
        .expect("trace provider installed");
    assert_eq!(
        None, hit,
        "a parked (bCollideActors=false) actor must not block"
    );
    assert!(
        (location[0] - 200.0).abs() < 0.01,
        "the ray must pass through: {location:?}"
    );
}

#[test]
fn trace_filter_projtarget_actor_is_hit_without_block_flags() {
    // `AActor::ShouldTrace` (VA 0x10354640): with the script-trace flag word, an in-hash,
    // extent-blocking actor is admitted when `bProjTarget` is set even without block flags.
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, true);
    let target = phys_actor(&mut vm, &set, "Target", [40.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, target, true, false);
    vm.set_property(target, "bProjTarget", 0, Value::Bool(true));
    vm.set_property(target, "bBlockZeroExtentTraces", 0, Value::Bool(true));
    let (hit, _, _) = vm
        .vm_trace(tracer, [0.0, 0.0, 0.0], [200.0, 0.0, 0.0], true, [0.0; 3])
        .expect("trace provider installed");
    assert_eq!(Some(target), hit);
}

#[test]
fn trace_filter_membership_and_extent_without_shouldtrace_passes_through() {
    // Membership plus the extent prefilter without `ShouldTrace` admission (no `bProjTarget`,
    // no `bBlockActors && bBlockPlayers`) is not enough — the third decoded stage rejects.
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, true);
    let ghost = phys_actor(&mut vm, &set, "Ghost", [40.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, ghost, true, false);
    let (hit, _, _) = vm
        .vm_trace(tracer, [0.0, 0.0, 0.0], [200.0, 0.0, 0.0], true, [0.0; 3])
        .expect("trace provider installed");
    assert_eq!(None, hit);
}

#[test]
fn trace_filter_hidden_colliding_actor_still_blocks() {
    // `bHidden` is tested nowhere in the decoded trace path (hash insert/remove, hash walk,
    // every `ShouldTrace` override). A colliding hidden actor blocks — the carried first-person
    // weapon passes only because its class clears `bCollideActors` (`xiii.Fists`, measured).
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, true);
    let hidden = phys_actor(&mut vm, &set, "Hidden", [40.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, hidden, true, true);
    vm.set_property(hidden, "bHidden", 0, Value::Bool(true));
    let (hit, _, _) = vm
        .vm_trace(tracer, [0.0, 0.0, 0.0], [200.0, 0.0, 0.0], true, [0.0; 3])
        .expect("trace provider installed");
    assert_eq!(Some(hidden), hit);
}

#[test]
fn trace_filter_decoration_chain_admits_without_block_flags() {
    // `AMover`/`ADecoration::ShouldTrace` (shared VA 0x10306c70) returns `TraceFlags & 2`, which
    // the script-trace flag word always sets: an in-hash decoration is admitted regardless of its
    // block flags.
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, true);
    let prop = vm.spawn(pg(&set, "Decoration"), "Prop").unwrap();
    vm.set_property(prop, "Location", 0, Value::Vector([40.0, 0.0, 0.0]));
    set_collision_fields(&mut vm, prop, true, false);
    vm.set_property(prop, "bBlockZeroExtentTraces", 0, Value::Bool(true));
    let (hit, _, _) = vm
        .vm_trace(tracer, [0.0, 0.0, 0.0], [200.0, 0.0, 0.0], true, [0.0; 3])
        .expect("trace provider installed");
    assert_eq!(Some(prop), hit);
}

#[test]
fn trace_filter_world_geometry_actor_is_hit_without_block_flags() {
    // World-geometry actors are admitted by the `TraceFlags & 0x80` branch of
    // `AActor::ShouldTrace` (`IsBlockedBy` bit 30 of `+0x2c`).
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, true);
    let blocker = phys_actor(&mut vm, &set, "Blocker", [40.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, blocker, true, false);
    vm.set_property(blocker, "bWorldGeometry", 0, Value::Bool(true));
    vm.set_property(blocker, "bBlockZeroExtentTraces", 0, Value::Bool(true));
    let (hit, _, _) = vm
        .vm_trace(tracer, [0.0, 0.0, 0.0], [200.0, 0.0, 0.0], true, [0.0; 3])
        .expect("trace provider installed");
    assert_eq!(Some(blocker), hit);
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
fn trace_extent_flag_needs_bprojtarget() {
    // Measured on Plage01 (item30c): `BaseSoldier6` blocks hitscan traces with
    // `bCollideActors=false` + `bBlockZeroExtentTraces=true` + `bProjTarget=true`, while the
    // muzzle-flash `MuzzleLight` (spawned at the muzzle by `MuzzleAttach` once the spawn fix
    // gives the weapon attachment an Instigator) has the same extent flag but
    // `bCollideActors=false` + `bProjTarget=false` and must not stop the bullet. The extent
    // flag is the gate; `bProjTarget` - the game's own shootability marker, set on every
    // shootable actor and clear on triggers and effects - qualifies it.
    let set = set_of(trace_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let light = vm.spawn(g(&set, "Actor"), "MuzzleLight").unwrap();
    let pawn = vm.spawn(g(&set, "Actor"), "Pawn").unwrap();
    // The tracer is a third actor off the ray, spawned last like the sibling tests.
    let shooter = vm.spawn(g(&set, "Actor"), "Shooter").unwrap();
    vm.set_active(shooter, true);
    for (id, x) in [(light, 50.0), (pawn, 100.0)] {
        vm.set_property(id, "Location", 0, Value::Vector([x, 0.0, 0.0]));
        vm.set_property(id, "CollisionRadius", 0, Value::Float(24.0));
        vm.set_property(id, "CollisionHeight", 0, Value::Float(24.0));
        vm.set_property(id, "bBlockZeroExtentTraces", 0, Value::Bool(true));
        vm.set_property(id, "bProjTarget", 0, Value::Bool(id == pawn));
        vm.set_active(id, true);
    }
    // The muzzle light: no collision role -> not a trace candidate; the pawn wins.
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
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(pawn)))),
        "a non-colliding non-proj-target actor must not block the bullet trace"
    );
    // The measured `TouchTrigger7` shape (merged-tree Plage01): `bCollideActors=true` with a
    // map-sized radius but `bProjTarget=false` - it must not stop the bullet either.
    vm.set_property(light, "bCollideActors", 0, Value::Bool(true));
    vm.set_property(light, "CollisionRadius", 0, Value::Float(2000.0));
    vm.set_property(light, "CollisionHeight", 0, Value::Float(2000.0));
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
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(pawn)))),
        "a colliding non-proj-target trigger must not block the bullet trace"
    );
    // Give the light the pawn's role and it becomes the nearer hit.
    vm.set_property(light, "bProjTarget", 0, Value::Bool(true));
    vm.set_property(light, "CollisionRadius", 0, Value::Float(24.0));
    vm.set_property(light, "CollisionHeight", 0, Value::Float(24.0));
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
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(light)))),
        "the same actor with a collision role blocks the trace again"
    );
}

#[test]
fn trace_skips_weapon_owned_first_person_muzzle_flash() {
    // item18 B11: `M60.TraceFire` could hit its own `StarFPMF` attachment before the pawn. The
    // attachment has `bCollideActors=false` but `bBlockZeroExtentTraces=true`; UE2's owner-chain
    // filter must skip a candidate whose Owner chain contains the tracer (not just the reverse).
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let weapon = phys_actor(&mut vm, &set, "Weapon", [0.0, 0.0, 0.0]);
    let flash = phys_actor(&mut vm, &set, "StarFPMF", [50.0, 0.0, 0.0]);
    let soldier = phys_actor(&mut vm, &set, "Soldier", [100.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, weapon, true, false);
    set_collision_fields(&mut vm, flash, false, true);
    set_collision_fields(&mut vm, soldier, true, true);
    // Measured game shape (Plage01): the shootable soldier is a projectile target; the
    // muzzle-flash attachment is not (`bProjTarget=false`).
    vm.set_property(soldier, "bProjTarget", 0, Value::Bool(true));
    vm.set_property(
        flash,
        "Owner",
        0,
        Value::Object(Some(ObjRef::Instance(weapon))),
    );

    let (hit, location, _) = vm
        .vm_trace(weapon, [0.0, 0.0, 0.0], [200.0, 0.0, 0.0], true, [0.0; 3])
        .expect("trace provider installed");
    assert_eq!(
        Some(soldier),
        hit,
        "weapon-owned flash must not intercept Trace"
    );
    assert!(
        location[0] > 80.0,
        "trace should reach the soldier: {location:?}"
    );
}

#[test]
fn trace_ignores_actors_outside_the_collision_hash() {
    // item40e: `Engine.dll` only puts `bCollideActors` actors in the collision hash
    // (`FCollisionHash::AddActor` asserts it; `AActor::SetCollision` removes/re-adds on change),
    // so an actor with the `Actor` default `bBlockZeroExtentTraces=true` but
    // `bCollideActors=false` (a hidden `TriggerLight` in front of an Amos01 grille) must not stop
    // a weapon trace. The colliding actor behind it is hit.
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    let light = phys_actor(&mut vm, &set, "TriggerLight2", [50.0, 0.0, 0.0]);
    let target = phys_actor(&mut vm, &set, "Target", [120.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, false);
    set_collision_fields(&mut vm, light, false, true);
    set_collision_fields(&mut vm, target, true, true);
    let (hit, _, _) = vm
        .vm_trace(tracer, [0.0; 3], [200.0, 0.0, 0.0], true, [0.0; 3])
        .expect("trace provider installed");
    assert_eq!(
        hit,
        Some(target),
        "a non-colliding actor must not block Trace"
    );

    // Collision-hash membership alone is not enough: the extent flag still selects line traces.
    vm.set_property(target, "bBlockZeroExtentTraces", 0, Value::Bool(false));
    let (hit, location, _) = vm
        .vm_trace(tracer, [0.0; 3], [200.0, 0.0, 0.0], true, [0.0; 3])
        .unwrap();
    assert_eq!(hit, None, "no zero-extent blocker left on the line");
    assert_eq!(location, [200.0, 0.0, 0.0]);
    // ...and a box trace uses the non-zero-extent flag, which is still set.
    let (hit, _, _) = vm
        .vm_trace(tracer, [0.0; 3], [200.0, 0.0, 0.0], true, [2.0, 2.0, 2.0])
        .unwrap();
    assert_eq!(hit, Some(target));
}

/// [`MockWorld`] whose world hits name an actor, like the map adapter's per-source label.
struct NamedHitWorld {
    inner: MockWorld,
    actor: String,
}

impl WorldPhysics for NamedHitWorld {
    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        self.inner.trace(start, end, extent)
    }

    fn trace_with_mover(
        &mut self,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> (Option<WorldHit>, Option<String>) {
        let hit = self.inner.trace(start, end, extent);
        let actor = hit.map(|_| self.actor.clone());
        (hit, actor)
    }

    fn move_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        self.inner.move_box(start, delta, extent)
    }

    fn point_free(&mut self, location: [f32; 3], extent: [f32; 3]) -> bool {
        self.inner.point_free(location, extent)
    }
}

#[test]
fn world_hit_on_a_non_mover_actor_source_still_returns_the_level() {
    // item40e: only a mover's geometry turns a world hit into an actor hit (UE2 movers are
    // collision-hash actors); geometry labelled with any other actor (a placed static mesh)
    // keeps the existing level result, so scripts testing `Other == Level` are unchanged.
    let set = phys_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(NamedHitWorld {
        inner: MockWorld::new().with_wall([100.0, -50.0, -50.0], [110.0, 50.0, 50.0]),
        actor: "Prop".to_owned(),
    }));
    let level = vm.spawn(pg(&set, "LevelInfo"), "LevelInfo0").unwrap();
    let tracer = phys_actor(&mut vm, &set, "Tracer", [0.0, 0.0, 0.0]);
    let prop = phys_actor(&mut vm, &set, "Prop", [105.0, 0.0, 0.0]);
    set_collision_fields(&mut vm, tracer, true, false);
    set_collision_fields(&mut vm, prop, true, true);
    // Keep the prop's own cylinder off the line so only the world hit can report it.
    vm.set_property(prop, "Location", 0, Value::Vector([105.0, 500.0, 0.0]));
    let (hit, location, _) = vm
        .vm_trace(tracer, [0.0; 3], [200.0, 0.0, 0.0], true, [0.0; 3])
        .unwrap();
    assert_eq!(hit, Some(level));
    assert!((location[0] - 100.0).abs() < 1e-3, "{location:?}");
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
    // Blocking shape as measured on the game's shootable actors: the extent flag opts the actor
    // into traces and `bProjTarget=true` is the trace filter's collision role.
    set_collision_fields(&mut vm, b, true, true);
    vm.set_property(b, "bProjTarget", 0, Value::Bool(true));
    // item40e: a zero-extent actor check needs `bCollideActors` (collision hash) and
    // `bBlockZeroExtentTraces` (Engine.dll `FCollisionHash::ActorLineCheck` tests bit 0x400 of
    // the actor flags at 0x10349E90); a non-blocking `B` would not be hit.
    set_collision_fields(&mut vm, b, true, true);

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

    // The decoded Plage01 wake-up script disables `bCollideWorld` before setting the pawn's bed
    // pose, which overlaps the closed `Fenetre3`/nearby window actor cylinders. UE2's scripted
    // FarMoveActor path permits that collisionless cutscene placement; restoring world collision
    // restores normal encroachment rejection.
    vm.set_property(a, "bCollideWorld", 0, Value::Bool(false));
    let mut args = [Value::Vector([500.0, 0.0, 0.0])];
    let ok = match try_native(&mut vm, "Engine.Actor.SetLocation", a, &[false], &mut args).unwrap()
    {
        NativeOutcome::Value(Value::Bool(b)) => b,
        other => panic!("{other:?}"),
    };
    assert!(
        ok,
        "collisionless scripted placement must not be blocked by an actor"
    );
    assert_eq!(
        vm.get_property(a, "Location"),
        Some(&Value::Vector([500.0, 0.0, 0.0]))
    );
    vm.set_property(a, "bCollideWorld", 0, Value::Bool(true));

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
    // The earlier collisionless move still generated its overlap touch with B; the later move
    // adds the touch with C rather than resetting the actor's accumulated touch counter.
    assert_eq!(vm.get_property(a, "Touches"), Some(&Value::Int(2)));
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
    assert_eq!(items[0][0], Value::Object(Some(ObjRef::Instance(b))));
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
    let attachment_bone = b.reserve(IMP_NAMEPROP, actor, "AttachmentBone");
    let base = b.reserve(IMP_OBJECTPROP, actor, "Base");
    let static_mesh = b.reserve(IMP_OBJECTPROP, actor, "StaticMesh");

    let selected = b.reserve(IMP_OBJECTPROP, actor, "Selected");
    let pending = b.reserve(IMP_OBJECTPROP, actor, "Pending");
    let switching = b.reserve(IMP_STATE, actor, "Switching");
    let begin_switch = b.reserve(IMP_FUNCTION, switching, "BeginState");
    let end_switch = b.reserve(IMP_FUNCTION, switching, "AnimEnd");
    let end_channel = b.reserve(IMP_INTPROP, end_switch, "Channel");

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
    b.prop(counter, attachment_bone, 0);
    b.prop(attachment_bone, base, 0);
    b.prop_with(base, static_mesh, 0, &object_extra);
    b.prop_with(static_mesh, selected, 0, &object_extra);
    b.prop_with(selected, pending, 0, &object_extra);
    b.prop_with(pending, link, 0, &object_extra);

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
    b.state(animating, switching, &anim_code, 0x24, 0x13);

    // Authored synthetic example: start a finite Down clip on state entry; selection changes
    // only in the state-aware animation callback. No retail bytecode is embedded here.
    let down = b.name("Down") as u8;
    let mut select_code = vec![0x61, 0x03, 0x21, down, 0x1E];
    select_code.extend(1.0f32.to_le_bytes());
    select_code.push(0x1E);
    select_code.extend(0.0f32.to_le_bytes());
    select_code.extend([0x25, 0x16, 0x04, 0x0B]);
    b.func(
        begin_switch,
        end_switch,
        0,
        &select_code,
        21,
        0,
        EVENT | DEFINED,
    );
    b.prop(end_channel, 0, PARM);
    let end_code = [
        0x0F,
        0x01,
        selected as u8,
        0x01,
        pending as u8,
        0x0F,
        0x01,
        counter as u8,
        0x26,
        0x04,
        0x0B,
    ];
    b.func(
        end_switch,
        0,
        end_channel,
        &end_code,
        20,
        0,
        EVENT | DEFINED,
    );
    b.state_children(switching, 0, begin_switch, &[0x08], 1, 0xFFFF);

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

    // Four frames indexed 0..3 at 1 fps: completion after 3 seconds (DLL end=1-1/N).
    for _ in 0..5 {
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
    assert_eq!(vm.get_property(a, "AnimFrame"), Some(&Value::Float(0.75)));
    // Further ticks do not fire it again.
    vm.tick(0.5).unwrap();
    assert_eq!(anim_end_count(&vm), 1);
}

#[test]
fn item49_deferred_selection_waits_for_clip_end_and_uses_latest_pending() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(11, 10.0)));
    let weapon = vm.spawn(sg(&set, "Actor"), "Weapon").unwrap();
    let old = vm.spawn(sg(&set, "Actor"), "Old").unwrap();
    let next = vm.spawn(sg(&set, "Actor"), "Next").unwrap();
    let latest = vm.spawn(sg(&set, "Actor"), "Latest").unwrap();
    let reference = |id| Value::Object(Some(ObjRef::Instance(id)));
    vm.set_property(weapon, "Selected", 0, reference(old));
    vm.set_property(weapon, "Pending", 0, reference(next));
    vm.set_active(weapon, true);
    vm.goto_state(weapon, "Switching", None).unwrap();
    // Repeated small steps must not switch early, and retargeting an in-flight selection
    // must use the latest pending value when the callback actually runs.
    for _ in 0..9 {
        vm.tick(0.1).unwrap();
        assert_eq!(vm.get_property(weapon, "Selected"), Some(&reference(old)));
    }
    vm.set_property(weapon, "Pending", 0, reference(latest));
    vm.tick(0.11).unwrap();
    assert_eq!(
        vm.get_property(weapon, "Selected"),
        Some(&reference(latest))
    );
    assert_eq!(vm.get_property(weapon, "Counter"), Some(&Value::Int(1)));
    assert_eq!(anim_end_count(&vm), 1);
    vm.set_property(weapon, "Counter", 0, Value::Int(0));
    vm.tick(2.0).unwrap();
    assert_eq!(vm.get_property(weapon, "Counter"), Some(&Value::Int(0)));
    assert_eq!(
        anim_end_count(&vm),
        1,
        "completed clip cannot deliver twice"
    );
}

#[test]
fn item49_interrupted_down_clip_cannot_complete_old_selection() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(11, 10.0)));
    let weapon = vm.spawn(sg(&set, "Actor"), "Weapon").unwrap();
    vm.set_active(weapon, true);
    vm.goto_state(weapon, "Switching", None).unwrap();
    vm.tick(0.8).unwrap();
    play_anim(&mut vm, weapon, "Replacement", 1.0, 0);
    vm.tick(0.3).unwrap();
    assert_eq!(
        anim_end_count(&vm),
        0,
        "the interrupted Down end must be cancelled"
    );
    vm.tick(0.71).unwrap();
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
fn loop_anim_reports_end_at_last_frame_and_keeps_animating() {
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
    assert_eq!(
        anim_end_count(&vm),
        2,
        "DLL sends loop AnimEnd at frames 3 and 7 before wraps at 4 and 8"
    );
    assert!(vm.anim_channel_active(a, 0));

    let mut args = [Value::Int(0)];
    let r = try_native(&mut vm, "Engine.Actor.IsAnimating", a, &[false], &mut args).unwrap();
    assert_eq!(r, NativeOutcome::Value(Value::Bool(true)));
}

#[test]
fn anim_is_in_group_answers_false_and_leaves_a_visible_note() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(4, 1.0)));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    let mut args = [Value::Int(0), Value::Name("Standing".into())];
    let r = try_native(
        &mut vm,
        "Engine.Actor.AnimIsInGroup",
        a,
        &[false, false],
        &mut args,
    )
    .unwrap();
    // The decoded SeqInfo carries no sequence group, so the VM answers the engine's "not in
    // that group" and never silently claims membership.
    assert_eq!(r, NativeOutcome::Value(Value::Bool(false)));
    assert!(
        vm.trace.iter().any(|e| matches!(&e.kind, TraceKind::Note(text) if text.contains("AnimIsInGroup(0, 'Standing')") && text.contains("no group data"))),
        "AnimIsInGroup must leave a visible note: {:?}",
        vm.trace.iter().map(|e| format!("{:?}", e.kind)).collect::<Vec<_>>()
    );
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
fn levelinfo_dec_attaque_records_partial_counter_decrement() {
    let set = item46_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let level = vm.spawn(pg(&set, "LevelInfo"), "LevelInfoFixture").unwrap();
    let record = Value::Struct(vec![("value".into(), Value::Int(0))]);
    vm.set_property(level, "MusicVars", 0, Value::Array(vec![record.clone(); 3]));
    let mut args = [];
    assert_eq!(
        try_native(
            &mut vm,
            "Engine.LevelInfo.DecAttaque",
            level,
            &[],
            &mut args
        )
        .unwrap(),
        NativeOutcome::Value(Value::Void)
    );
    assert!(vm.trace.iter().any(|event| matches!(
        &event.kind,
        TraceKind::Note(note) if note.contains("MusicVars") && note.contains("audio-device")
    )));
    assert!(
        crate::registry::Registry::builtin()
            .get("levelinfo.decattaque")
            .is_some_and(|def| matches!(def.status, crate::registry::NativeStatus::Partial(_)))
    );
    assert!(!vm.missing_natives.contains_key("LevelInfo.DecAttaque"));
}

#[test]
fn item46_find_best_path_sets_target_on_success_and_preserves_it_on_failure() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav0", [0.0, 0.0, 0.0]);
    let first = spawn_at(&mut vm, &set, "Actor", "Nav1", [500.0, 0.0, 0.0]);
    let _ = spawn_at(&mut vm, &set, "Actor", "Nav2", [1000.0, 0.0, 0.0]);
    let (ctrl, _) = nav_actor_pair(&mut vm, &set, 40.0, 80.0, 100.0);
    vm.set_navigation(Box::new(MockNav::line()));
    let mut args = [Value::Vector([1000.0, 0.0, 0.0])];
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.FindBestPathTo",
            ctrl,
            &[false],
            &mut args
        ),
        NativeOutcome::Value(Value::Bool(true))
    );
    assert_eq!(vm.obj_prop(ctrl, "MoveTarget"), Some(first));
    assert_eq!(vm.vector_prop(ctrl, "Destination"), Some([500.0, 0.0, 0.0]));
    vm.set_navigation(Box::new(crate::navigation::EmptyNavigation));
    assert_eq!(
        call_native(
            &mut vm,
            "IAController.FindBestPathTo",
            ctrl,
            &[false],
            &mut args
        ),
        NativeOutcome::Value(Value::Bool(false))
    );
    assert_eq!(vm.obj_prop(ctrl, "MoveTarget"), Some(first));
    assert_eq!(vm.vector_prop(ctrl, "Destination"), Some([500.0, 0.0, 0.0]));
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

/// Synthetic weapon-attachment cast fixture (item14c): `Object` <- `Actor` <-
/// `InventoryAttachment` <- `WeaponAttachment`, plus a `Caller` with a `ThirdPersonActor` object
/// link and a `Fire` function whose only statement is
/// `WeaponAttachment(ThirdPersonActor).ThirdPersonEffects()`. `ThirdPersonEffects` sets
/// `self.Fired = true`. This is the VM shape of the retail `Weapon.IncrementFlashCount` ->
/// `WeaponAttachment(ThirdPersonActor).ThirdPersonEffects()` chain.
fn weapon_attachment_fixture() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let inv_attach = b.reserve(0, 0, "InventoryAttachment");
    let weapon_attach = b.reserve(0, 0, "WeaponAttachment");
    let caller = b.reserve(0, 0, "Caller");

    let tpa = b.reserve(IMP_OBJPROP, caller, "ThirdPersonActor");
    let fire = b.reserve(IMP_FUNCTION, caller, "Fire");
    let fired = b.reserve(B_BOOLPROP, weapon_attach, "Fired");
    let effects = b.reserve(IMP_FUNCTION, weapon_attach, "ThirdPersonEffects");
    let effects_name = b.name("ThirdPersonEffects");

    b.prop_with(tpa, fire, 0, &compact(0));
    b.prop(fired, effects, 0);

    // Caller.Fire(): WeaponAttachment(self.ThirdPersonActor).ThirdPersonEffects().
    let fire_code = vec![
        0x19, // Context
        0x2E, // DynamicCast
    ]
    .into_iter()
    .chain(compact(weapon_attach))
    .chain([0x01])
    .chain(compact(tpa))
    .chain([
        0x00, 0x00, 0x00, // Context skip u16 + size u8
        0x1B, // VirtualFunction
    ])
    .chain(compact(effects_name))
    .chain([
        0x16, // EndFunctionParms
        0x04, // Return
        0x0B, // Nothing
    ])
    .collect::<Vec<u8>>();
    // `mem` is the decoded memory size (object refs/names count 4 bytes, not their file length).
    b.func(fire, 0, 0, &fire_code, 22, 0, ff::DEFINED);

    // WeaponAttachment.ThirdPersonEffects(): self.Fired = true.
    let effects_code = vec![
        0x0F, // Let
        0x01, // InstanceVariable
    ]
    .into_iter()
    .chain(compact(fired))
    .chain([0x27, 0x04, 0x0B]) // True, Return, Nothing
    .collect::<Vec<u8>>();
    b.func(effects, 0, 0, &effects_code, 9, 0, ff::DEFINED);

    b.class(object, 0, 0);
    b.class(actor, object, 0);
    b.class(inv_attach, actor, 0);
    b.class(weapon_attach, inv_attach, fired);
    b.class(caller, actor, tpa);
    b.build()
}

#[test]
fn weapon_attachment_cast_reaches_third_person_effects() {
    let set = set_of(weapon_attachment_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let caller = vm.spawn(g(&set, "Caller"), "Weapon").unwrap();
    let attach = vm.spawn(g(&set, "WeaponAttachment"), "Attach").unwrap();
    vm.set_active(caller, true);
    vm.set_active(attach, true);
    vm.set_property(
        caller,
        "ThirdPersonActor",
        0,
        Value::Object(Some(ObjRef::Instance(attach))),
    );
    vm.call_function(g(&set, "Caller.Fire"), caller, vec![])
        .expect("the cast + call must run");
    assert_eq!(
        vm.get_property(attach, "Fired"),
        Some(&Value::Bool(true)),
        "the DynamicCast must reach the WeaponAttachment's ThirdPersonEffects"
    );
    // A plain Actor fails the `WeaponAttachment(...)` cast: the chained call is a no-op
    // (`Accessed None`), never a crash, and it must not set anything on the non-attachment.
    let other = vm.spawn(g(&set, "Actor"), "Other").unwrap();
    vm.set_property(
        caller,
        "ThirdPersonActor",
        0,
        Value::Object(Some(ObjRef::Instance(other))),
    );
    vm.call_function(g(&set, "Caller.Fire"), caller, vec![])
        .expect("a failed cast must be a no-op, not an error");
    assert_eq!(vm.get_property(other, "Fired"), None);
}

#[test]
fn diagnostic_scope_suspended_calls_are_counted_and_traced() {
    let set = set_of(weapon_attachment_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_diagnostic_call_scope(true);
    let caller = vm.spawn(g(&set, "Caller"), "Weapon").unwrap();
    let attach = vm.spawn(g(&set, "WeaponAttachment"), "Attach").unwrap();
    vm.set_active(caller, true);
    vm.set_active(attach, true);
    vm.set_property(
        caller,
        "ThirdPersonActor",
        0,
        Value::Object(Some(ObjRef::Instance(attach))),
    );
    // Suspend the attachment as `suspend_for_error` would (a script error cleared `active`).
    vm.objects[attach as usize].active = false;
    vm.objects[attach as usize].suspended = true;
    assert_eq!(vm.suspended_deferred_calls(), 0);
    // The inner `WeaponAttachment(ThirdPersonActor).ThirdPersonEffects()` call is dropped: it
    // must be counted and traced, not silently no-op'd.
    vm.call_function(g(&set, "Caller.Fire"), caller, vec![])
        .expect("the caller itself runs");
    assert_eq!(
        vm.suspended_deferred_calls(),
        1,
        "the dropped call on the suspended attachment must be counted"
    );
    assert!(
        vm.trace.iter().any(|e| matches!(
            &e.kind,
            TraceKind::Note(s) if s.contains("suspended actor Attach")
        )),
        "the dropped call must be recorded in the trace"
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

/// Finite angled wall used to prove that a VM-driven PHYS_Walking pawn receives provider sliding.
/// The wall lies on `normal·p = offset` and is bounded along its tangent, so its far end is
/// traversable. Non-walking `move_box` deliberately stops at the first contact.
struct AngledWallPhysics {
    normal: [f32; 2],
    tangent: [f32; 2],
    offset: f32,
    half_length: f32,
}

impl AngledWallPhysics {
    fn hit(&self, start: [f32; 3], delta: [f32; 3], radius: f32) -> Option<(f32, [f32; 3])> {
        let signed = start[0] * self.normal[0] + start[1] * self.normal[1];
        let into = delta[0] * self.normal[0] + delta[1] * self.normal[1];
        if signed > self.offset - radius || into <= 0.0 {
            return None;
        }
        let time = (self.offset - radius - signed) / into;
        if !(0.0..=1.0).contains(&time) {
            return None;
        }
        let point = [start[0] + delta[0] * time, start[1] + delta[1] * time];
        let along = point[0] * self.tangent[0] + point[1] * self.tangent[1];
        (along.abs() <= self.half_length + radius)
            .then_some((time, [self.normal[0], self.normal[1], 0.0]))
    }

    fn outcome(
        &self,
        start: [f32; 3],
        delta: [f32; 3],
        end: [f32; 3],
        time: f32,
        normal: [f32; 3],
    ) -> MoveOutcome {
        MoveOutcome {
            end,
            hit: Some(WorldHit {
                location: [
                    start[0] + delta[0] * time,
                    start[1] + delta[1] * time,
                    start[2] + delta[2] * time,
                ],
                normal,
                time,
            }),
        }
    }
}

impl WorldPhysics for AngledWallPhysics {
    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        let delta = [end[0] - start[0], end[1] - start[1], end[2] - start[2]];
        let (time, normal) = self.hit(start, delta, extent[0].max(extent[1]))?;
        Some(WorldHit {
            location: [
                start[0] + delta[0] * time,
                start[1] + delta[1] * time,
                start[2] + delta[2] * time,
            ],
            normal,
            time,
        })
    }

    fn move_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        let full_end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        match self.hit(start, delta, extent[0].max(extent[1])) {
            Some((t, n)) => self.outcome(
                start,
                delta,
                [
                    start[0] + delta[0] * t,
                    start[1] + delta[1] * t,
                    full_end[2],
                ],
                t,
                n,
            ),
            None => MoveOutcome {
                end: full_end,
                hit: None,
            },
        }
    }

    fn walk_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        let radius = extent[0].max(extent[1]);
        let full_end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        let Some((time, normal)) = self.hit(start, delta, radius) else {
            return MoveOutcome {
                end: full_end,
                hit: None,
            };
        };
        let contact = [
            start[0] + delta[0] * time,
            start[1] + delta[1] * time,
            full_end[2],
        ];
        let left = [(1.0 - time) * delta[0], (1.0 - time) * delta[1]];
        let into = left[0] * normal[0] + left[1] * normal[1];
        let slide = [left[0] - into * normal[0], left[1] - into * normal[1]];
        self.outcome(
            start,
            delta,
            [contact[0] + slide[0], contact[1] + slide[1], full_end[2]],
            time,
            normal,
        )
    }

    fn point_free(&mut self, _location: [f32; 3], _extent: [f32; 3]) -> bool {
        true
    }
}

#[test]
fn vm_walking_pawn_slides_along_finite_angled_wall_and_reaches_target() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(AngledWallPhysics {
        normal: [0.9486833, 0.31622776],
        tangent: [-0.31622776, 0.9486833],
        offset: 50.0,
        half_length: 30.0,
    }));
    let (ctrl, pawn) = nav_actor_pair(&mut vm, &set, 1.0, 2.0, 100.0);
    vm.set_property(pawn, "Physics", 0, Value::Byte(1)); // PHYS_Walking
    assert_eq!(vm.get_property(pawn, "Physics"), Some(&Value::Byte(1)));
    let mut args = [Value::Vector([100.0, 0.0, 0.0])];
    let _ = call_native_stateful(
        &mut vm,
        "Engine.Controller.MoveTo",
        ctrl,
        &[false, true, true],
        &mut args,
    );

    let mut arrived = false;
    for _ in 0..500 {
        arrived = vm
            .move_pawn_step(pawn, [100.0, 0.0, 0.0], 0.0, 0.1)
            .unwrap();
        if arrived {
            break;
        }
    }
    assert!(
        arrived,
        "VM walking pawn failed to route around the angled wall: {:?}",
        vm.vector_prop(pawn, "Location")
    );
    let final_pos = vm.vector_prop(pawn, "Location").unwrap();
    assert!((final_pos[0] - 100.0).abs() <= 1.0, "{final_pos:?}");
    assert!(final_pos[1].abs() <= 1.0, "{final_pos:?}");
}

#[test]
fn vm_nonwalking_pawn_keeps_direct_swept_move() {
    let set = nav_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(AngledWallPhysics {
        normal: [0.9486833, 0.31622776],
        tangent: [-0.31622776, 0.9486833],
        offset: 50.0,
        half_length: 30.0,
    }));
    let (_ctrl, pawn) = nav_actor_pair(&mut vm, &set, 1.0, 2.0, 100.0);
    vm.set_property(pawn, "Physics", 0, Value::Byte(0)); // PHYS_None
    assert_eq!(vm.get_property(pawn, "Physics"), Some(&Value::Byte(0)));

    let mut arrived = false;
    for _ in 0..100 {
        arrived = vm
            .move_pawn_step(pawn, [100.0, 0.0, 0.0], 0.0, 0.1)
            .unwrap();
        if arrived {
            break;
        }
    }
    assert!(
        !arrived,
        "PHYS_None must not receive the PHYS_Walking slide"
    );
    let pos = vm.vector_prop(pawn, "Location").unwrap();
    assert!(
        pos[0] < 55.0,
        "direct swept move should stop at the wall: {pos:?}"
    );
    assert!(
        pos[1].abs() < 1.0,
        "direct move must not slide along the wall: {pos:?}"
    );
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

// ---------------------------------------------------------------------------------------
// item27m: the decoded Engine.dll LineOfSightTo (VA 0x1036ac70) and SeePawn/CanSee
// (VA 0x1036dc40) semantics. Fixture: Controller with Pawn/Enemy/ViewTarget, Pawn with
// BaseEyeHeight/SightRadius/PeripheralVision, Actor with CollisionHeight and a Visibility byte.

fn los_fixture() -> Vec<u8> {
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let pawn = b.reserve(0, 0, "Pawn");
    let controller = b.reserve(0, 0, "Controller");
    let player_controller = b.reserve(0, 0, "PlayerController");

    let object_extra = compact(0);
    let vector_extra = compact(IMP_STRUCT);

    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let collision_height = b.reserve(IMP_FLOATPROP, actor, "CollisionHeight");
    let visibility = b.reserve(IMP_BYTEPROP, actor, "Visibility");

    let eye = b.reserve(IMP_FLOATPROP, pawn, "BaseEyeHeight");
    let sight_radius = b.reserve(IMP_FLOATPROP, pawn, "SightRadius");
    let peripheral = b.reserve(IMP_FLOATPROP, pawn, "PeripheralVision");

    let c_pawn = b.reserve(IMP_OBJECTPROP, controller, "Pawn");
    let c_enemy = b.reserve(IMP_OBJECTPROP, controller, "Enemy");
    let c_view_target = b.reserve(IMP_OBJECTPROP, controller, "ViewTarget");

    b.prop_with(location, collision_height, 0, &vector_extra);
    b.prop(collision_height, visibility, 0);
    b.prop_with(visibility, 0, 0, &object_extra);
    b.prop(eye, sight_radius, 0);
    b.prop(sight_radius, peripheral, 0);
    b.prop(peripheral, 0, 0);
    b.prop_with(c_pawn, c_enemy, 0, &object_extra);
    b.prop_with(c_enemy, c_view_target, 0, &object_extra);
    b.prop_with(c_view_target, 0, 0, &object_extra);

    b.class(object, 0, 0, 0);
    b.class(actor, object, location, 0);
    b.class(pawn, actor, eye, 0);
    b.class(controller, actor, c_pawn, 0);
    b.class(player_controller, controller, 0, 0);
    b.build()
}

fn los_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        los_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

/// A controller at the origin with a pawn whose eye is 100 above the base, plus an `other`
/// actor at `loc` with the given BaseEyeHeight/CollisionHeight.
fn los_pair(
    vm: &mut Vm<'_>,
    set: &ScriptSet,
    other_class: &str,
    loc: [f32; 3],
    other_eye: f32,
    other_height: f32,
) -> (ObjectId, ObjectId, ObjectId) {
    let pawn = spawn_at(vm, set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(pawn, "BaseEyeHeight", 0, Value::Float(100.0));
    let ctrl = vm.spawn(pg(set, "Controller"), "C").unwrap();
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    let other = spawn_at(vm, set, other_class, "O", loc);
    vm.set_property(other, "BaseEyeHeight", 0, Value::Float(other_eye));
    vm.set_property(other, "CollisionHeight", 0, Value::Float(other_height));
    // The Engine.u Pawn default (the fixture leaves the byte at 0).
    vm.set_property(other, "Visibility", 0, Value::Int(128));
    (ctrl, pawn, other)
}

/// The base line (view point -> target base) is blocked by a low wall; the decoded retry for a
/// non-Enemy target goes to `Location.Z + 0.8*CollisionHeight` (Engine.dll 0x1036b020) and clears
/// above the wall: LineOfSightTo is true. A lower target top is blocked by the same wall: false.
/// The old single eye-to-eye trace answered the opposite for the first case (the eye line clears
/// above the wall).
#[test]
fn line_of_sight_to_tries_the_base_then_the_top_point() {
    let set = los_set();
    // Wall ("counter") x in [50,51], z in [40,80]: the descending base line (eye z=100 -> base
    // z=0) is blocked at z=50; the eye-to-eye line at z=100 is NOT blocked.
    let wall = ([50.0, -10.0, 40.0], [51.0, 10.0, 80.0]);

    // Target top 0.8*100 = 80: the retry line (z 100 -> 80) passes above the wall (z=90 there).
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new().with_wall(wall.0, wall.1)));
    let (ctrl, _pawn, other) = los_pair(&mut vm, &set, "Pawn", [100.0, 0.0, 0.0], 100.0, 100.0);
    let mut args = [Value::Object(Some(ObjRef::Instance(other)))];
    let out = call_native(
        &mut vm,
        "Engine.Controller.LineOfSightTo",
        ctrl,
        &[false],
        &mut args,
    );
    assert_eq!(out, NativeOutcome::Value(Value::Bool(true)));

    // Target top 0.8*25 = 20: the retry line (z 100 -> 20) crosses the wall at z=60 -> blocked.
    let mut vm2 = Vm::new(&set, VmLimits::default());
    vm2.set_physics(Box::new(MockWorld::new().with_wall(wall.0, wall.1)));
    let (ctrl2, _pawn2, other2) = los_pair(&mut vm2, &set, "Pawn", [100.0, 0.0, 0.0], 100.0, 25.0);
    let mut args2 = [Value::Object(Some(ObjRef::Instance(other2)))];
    let out2 = call_native(
        &mut vm2,
        "Engine.Controller.LineOfSightTo",
        ctrl2,
        &[false],
        &mut args2,
    );
    assert_eq!(out2, NativeOutcome::Value(Value::Bool(false)));
}

/// The 8000^2 (0x1047c878) and 2000^2 (0x1047c874) limits gate only the RETRY: a clear base line
/// is visible at any distance, while a blocked base line falls back to the retry only inside the
/// limits. The `Enemy` branch (0x1036ad39) has no limits and retries at the eye point.
#[test]
fn line_of_sight_to_distance_limits_gate_only_the_retry() {
    let set = los_set();
    // Wall between viewer and target blocking the descending base line (z at the wall is 73)
    // but not the higher retry lines. Per distance, the wall sits at 0.27*dist.
    for (dist, expected) in [(1500.0, true), (2500.0, false), (3000.0, false)] {
        let wx = 0.27 * dist;
        let mut vm = Vm::new(&set, VmLimits::default());
        vm.set_physics(Box::new(
            MockWorld::new().with_wall([wx, -10.0, 40.0], [wx + 1.0, 10.0, 80.0]),
        ));
        let (ctrl, _pawn, other) = los_pair(&mut vm, &set, "Pawn", [dist, 0.0, 0.0], 100.0, 100.0);
        let mut args = [Value::Object(Some(ObjRef::Instance(other)))];
        let out = call_native(
            &mut vm,
            "Engine.Controller.LineOfSightTo",
            ctrl,
            &[false],
            &mut args,
        );
        assert_eq!(
            out,
            NativeOutcome::Value(Value::Bool(expected)),
            "dist {dist}"
        );
    }

    // The same far, wall-blocked geometry as Enemy: retry at the eye point (z=100, above the
    // wall), no distance limits -> visible even at 9e6 squared distance.
    let mut vm2 = Vm::new(&set, VmLimits::default());
    vm2.set_physics(Box::new(
        MockWorld::new().with_wall([810.0, -10.0, 40.0], [811.0, 10.0, 80.0]),
    ));
    let (ctrl2, _pawn2, other2) =
        los_pair(&mut vm2, &set, "Pawn", [3000.0, 0.0, 0.0], 100.0, 100.0);
    vm2.set_property(
        ctrl2,
        "Enemy",
        0,
        Value::Object(Some(ObjRef::Instance(other2))),
    );
    let mut args2 = [Value::Object(Some(ObjRef::Instance(other2)))];
    let out2 = call_native(
        &mut vm2,
        "Engine.Controller.LineOfSightTo",
        ctrl2,
        &[false],
        &mut args2,
    );
    assert_eq!(out2, NativeOutcome::Value(Value::Bool(true)));

    // Enemy whose BaseEyeHeight does not raise the retry over the wall: the eye retry is the
    // blocked base line -> false (the branch's second line is what clears, not the branch).
    let mut vm3 = Vm::new(&set, VmLimits::default());
    vm3.set_physics(Box::new(
        MockWorld::new().with_wall([810.0, -10.0, 40.0], [811.0, 10.0, 80.0]),
    ));
    let (ctrl3, _pawn3, other3) = los_pair(&mut vm3, &set, "Pawn", [3000.0, 0.0, 0.0], 0.0, 100.0);
    vm3.set_property(
        ctrl3,
        "Enemy",
        0,
        Value::Object(Some(ObjRef::Instance(other3))),
    );
    let mut args3 = [Value::Object(Some(ObjRef::Instance(other3)))];
    let out3 = call_native(
        &mut vm3,
        "Engine.Controller.LineOfSightTo",
        ctrl3,
        &[false],
        &mut args3,
    );
    assert_eq!(out3, NativeOutcome::Value(Value::Bool(false)));
}

/// A PlayerController traces from its `ViewTarget` (Engine.dll 0x10368040), raising the view
/// point by BaseEyeHeight only when the view target is its own pawn (0x1036ace6).
#[test]
fn line_of_sight_to_uses_the_player_controller_view_target() {
    let set = los_set();
    // Wall from the ground up to z=100: a trace at z=0 is blocked, traces descending from the
    // pawn eye (z=100) are blocked on the base line, retries can clear above.
    let wall = ([50.0, -10.0, -10.0], [51.0, 10.0, 100.0]);
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new().with_wall(wall.0, wall.1)));
    let pc = vm.spawn(pg(&set, "PlayerController"), "PC").unwrap();
    let pawn = spawn_at(&mut vm, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(pawn, "BaseEyeHeight", 0, Value::Float(100.0));
    vm.set_property(pc, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    // ViewTarget = a camera at z=0 (not the pawn): no eye raise -> the base line to a target
    // base at z=0 runs horizontally through the wall -> blocked, and the top retry
    // (0.8*40=32) also descends into the wall -> false.
    let camera = spawn_at(&mut vm, &set, "Actor", "Cam", [0.0, 0.0, 0.0]);
    vm.set_property(
        pc,
        "ViewTarget",
        0,
        Value::Object(Some(ObjRef::Instance(camera))),
    );
    let other = spawn_at(&mut vm, &set, "Pawn", "O", [100.0, 0.0, 0.0]);
    let mut args = [Value::Object(Some(ObjRef::Instance(other)))];
    let out = call_native(
        &mut vm,
        "Engine.Controller.LineOfSightTo",
        pc,
        &[false],
        &mut args,
    );
    assert_eq!(out, NativeOutcome::Value(Value::Bool(false)));

    // ViewTarget = the pawn itself: the eye raise applies (z=100), the base line descends into
    // the wall, and the top retry to 0.8*200=160 climbs above it (z=130 at the wall) -> true.
    vm.set_property(
        pc,
        "ViewTarget",
        0,
        Value::Object(Some(ObjRef::Instance(pawn))),
    );
    vm.set_property(other, "CollisionHeight", 0, Value::Float(200.0));
    let mut args2 = [Value::Object(Some(ObjRef::Instance(other)))];
    let out2 = call_native(
        &mut vm,
        "Engine.Controller.LineOfSightTo",
        pc,
        &[false],
        &mut args2,
    );
    assert_eq!(out2, NativeOutcome::Value(Value::Bool(true)));
}

/// CanSee (SeePawn, Engine.dll 0x1036dc40) adds the SightRadius/Visibility range gate and the
/// decoded `|delta| > PeripheralVision` check; LineOfSightTo has neither.
#[test]
fn can_see_range_and_peripheral_gates_differ_from_line_of_sight_to() {
    let set = los_set();

    // Clear world: LineOfSightTo ignores SightRadius, CanSee enforces it.
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let pawn = spawn_at(&mut vm, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm.set_property(pawn, "SightRadius", 0, Value::Float(500.0));
    let ctrl = vm.spawn(pg(&set, "Controller"), "C").unwrap();
    vm.set_property(ctrl, "Pawn", 0, Value::Object(Some(ObjRef::Instance(pawn))));
    let other = spawn_at(&mut vm, &set, "Pawn", "O", [600.0, 0.0, 0.0]);
    vm.set_property(other, "Visibility", 0, Value::Int(128));
    let mut args = [Value::Object(Some(ObjRef::Instance(other)))];
    let los = call_native(
        &mut vm,
        "Engine.Controller.LineOfSightTo",
        ctrl,
        &[false],
        &mut args,
    );
    assert_eq!(los, NativeOutcome::Value(Value::Bool(true)));
    let mut args2 = [Value::Object(Some(ObjRef::Instance(other)))];
    let see = call_native(
        &mut vm,
        "Engine.Controller.CanSee",
        ctrl,
        &[false],
        &mut args2,
    );
    assert_eq!(see, NativeOutcome::Value(Value::Bool(false)));

    // Within SightRadius: visible. Visibility scales the range: 64 halves it (250).
    let closer = spawn_at(&mut vm, &set, "Pawn", "O2", [400.0, 0.0, 0.0]);
    vm.set_property(closer, "Visibility", 0, Value::Int(128));
    let mut args3 = [Value::Object(Some(ObjRef::Instance(closer)))];
    let see2 = call_native(
        &mut vm,
        "Engine.Controller.CanSee",
        ctrl,
        &[false],
        &mut args3,
    );
    assert_eq!(see2, NativeOutcome::Value(Value::Bool(true)));
    vm.set_property(closer, "Visibility", 0, Value::Int(64));
    let mut args4 = [Value::Object(Some(ObjRef::Instance(closer)))];
    let see3 = call_native(
        &mut vm,
        "Engine.Controller.CanSee",
        ctrl,
        &[false],
        &mut args4,
    );
    assert_eq!(see3, NativeOutcome::Value(Value::Bool(false)));
    let nearest = spawn_at(&mut vm, &set, "Pawn", "O3", [200.0, 0.0, 0.0]);
    vm.set_property(nearest, "Visibility", 0, Value::Int(64));
    let _ = &nearest;
    let mut args5 = [Value::Object(Some(ObjRef::Instance(nearest)))];
    let see4 = call_native(
        &mut vm,
        "Engine.Controller.CanSee",
        ctrl,
        &[false],
        &mut args5,
    );
    assert_eq!(see4, NativeOutcome::Value(Value::Bool(true)));

    // PeripheralVision is compared as a distance in the decoded binary (dot(delta,
    // SafeNormal(delta)) = |delta|): 360 rejects inside 360 UU; the Engine.u default 0 and
    // the XIII -1 convention pass.
    for (pv, dist, expected) in [
        (360.0, 100.0, false),
        (0.0, 100.0, true),
        (-1.0, 100.0, true),
    ] {
        let mut vm2 = Vm::new(&set, VmLimits::default());
        vm2.set_physics(Box::new(MockWorld::new()));
        let pawn2 = spawn_at(&mut vm2, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
        vm2.set_property(pawn2, "PeripheralVision", 0, Value::Float(pv));
        let ctrl2 = vm2.spawn(pg(&set, "Controller"), "C").unwrap();
        vm2.set_property(
            ctrl2,
            "Pawn",
            0,
            Value::Object(Some(ObjRef::Instance(pawn2))),
        );
        let other2 = spawn_at(&mut vm2, &set, "Pawn", "O", [dist, 0.0, 0.0]);
        vm2.set_property(other2, "Visibility", 0, Value::Int(128));
        let mut args6 = [Value::Object(Some(ObjRef::Instance(other2)))];
        let out = call_native(
            &mut vm2,
            "Engine.Controller.CanSee",
            ctrl2,
            &[false],
            &mut args6,
        );
        assert_eq!(out, NativeOutcome::Value(Value::Bool(expected)), "pv {pv}");
    }

    // CanSee on the controller's Enemy is exactly LineOfSightTo: no range gate.
    let mut vm3 = Vm::new(&set, VmLimits::default());
    vm3.set_physics(Box::new(MockWorld::new()));
    let pawn3 = spawn_at(&mut vm3, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    vm3.set_property(pawn3, "SightRadius", 0, Value::Float(500.0));
    let ctrl3 = vm3.spawn(pg(&set, "Controller"), "C").unwrap();
    vm3.set_property(
        ctrl3,
        "Pawn",
        0,
        Value::Object(Some(ObjRef::Instance(pawn3))),
    );
    let far = spawn_at(&mut vm3, &set, "Pawn", "O", [3000.0, 0.0, 0.0]);
    vm3.set_property(far, "Visibility", 0, Value::Int(128));
    vm3.set_property(
        ctrl3,
        "Enemy",
        0,
        Value::Object(Some(ObjRef::Instance(far))),
    );
    let mut args7 = [Value::Object(Some(ObjRef::Instance(far)))];
    let see5 = call_native(
        &mut vm3,
        "Engine.Controller.CanSee",
        ctrl3,
        &[false],
        &mut args7,
    );
    assert_eq!(see5, NativeOutcome::Value(Value::Bool(true)));
}

/// Degenerate inputs: no pawn, missing view/other locations, zero distance.
#[test]
fn can_see_and_line_of_sight_to_degenerate_inputs() {
    let set = los_set();
    // No pawn on the controller: CanSee is false (SeePawn's first gate), LineOfSightTo still
    // traces from the controller itself (AController::GetViewTarget returns `this`).
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(MockWorld::new()));
    let ctrl = vm.spawn(pg(&set, "Controller"), "C").unwrap();
    let other = spawn_at(&mut vm, &set, "Pawn", "O", [100.0, 0.0, 0.0]);
    let mut args = [Value::Object(Some(ObjRef::Instance(other)))];
    let see = call_native(
        &mut vm,
        "Engine.Controller.CanSee",
        ctrl,
        &[false],
        &mut args,
    );
    assert_eq!(see, NativeOutcome::Value(Value::Bool(false)));
    let mut args2 = [Value::Object(Some(ObjRef::Instance(other)))];
    let los = call_native(
        &mut vm,
        "Engine.Controller.LineOfSightTo",
        ctrl,
        &[false],
        &mut args2,
    );
    assert_eq!(los, NativeOutcome::Value(Value::Bool(true)));

    // Controller and target exactly overlapping: the decoded PeripheralVision check (pass
    // requires |delta| > PeripheralVision strictly) rejects CanSee at distance 0.
    let mut vm2 = Vm::new(&set, VmLimits::default());
    vm2.set_physics(Box::new(MockWorld::new()));
    let pawn2 = spawn_at(&mut vm2, &set, "Pawn", "P", [0.0, 0.0, 0.0]);
    let ctrl2 = vm2.spawn(pg(&set, "Controller"), "C").unwrap();
    vm2.set_property(
        ctrl2,
        "Pawn",
        0,
        Value::Object(Some(ObjRef::Instance(pawn2))),
    );
    let same = spawn_at(&mut vm2, &set, "Pawn", "O", [0.0, 0.0, 0.0]);
    vm2.set_property(same, "Visibility", 0, Value::Int(128));
    let mut args3 = [Value::Object(Some(ObjRef::Instance(same)))];
    let see2 = call_native(
        &mut vm2,
        "Engine.Controller.CanSee",
        ctrl2,
        &[false],
        &mut args3,
    );
    assert_eq!(see2, NativeOutcome::Value(Value::Bool(false)));
    let mut args4 = [Value::Object(Some(ObjRef::Instance(same)))];
    let los2 = call_native(
        &mut vm2,
        "Engine.Controller.LineOfSightTo",
        ctrl2,
        &[false],
        &mut args4,
    );
    assert_eq!(los2, NativeOutcome::Value(Value::Bool(true)));
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
    let ammo = b.reserve(0, 0, "Ammo");
    let fists_ammo = b.reserve(0, 0, "FistsAmmo");
    let nine_mm_ammo = b.reserve(0, 0, "NineMmAmmo");
    let nine_mm_child = b.reserve(0, 0, "NineMmAmmoChild");
    let pawn = b.reserve(0, 0, "Pawn");
    // `Inventory` properties (inherited by `Pawn`): the link and the owner.
    let inv_inv = b.reserve(IMP_OBJPROP, inventory, "Inventory");
    let inv_owner = b.reserve(IMP_OBJPROP, inventory, "Owner");
    b.prop_with(inv_inv, inv_owner, 0, &compact(0));
    let give = b.reserve(IMP_FUNCTION, inventory, "GiveTo");
    let destroyed = b.reserve(IMP_FUNCTION, inventory, "Destroyed");
    b.prop_with(inv_owner, give, 0, &compact(0));
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
    let delete = b.reserve(IMP_FUNCTION, pawn, "DeleteInventory");
    b.func(add, delete, newitem, &code, 102, 0, ff::DEFINED);
    let other = b.reserve(IMP_OBJPROP, give, "Other");
    let give_ret = b.reserve(IMP_INTPROP, give, "ReturnValue");
    b.prop_with(other, give_ret, pf::PARM, &compact(0));
    b.prop(give_ret, 0, pf::PARM | pf::RETURN_PARM);
    let add_name = b.name("AddInventory");
    // Owner = Other; return Other.AddInventory(self). Exercises a return-valued nested
    // script call on an unticked/suspended owner, rather than a host call_function on it.
    let give_code = [
        0x0F,
        0x01,
        inv_owner as u8,
        0x00,
        other as u8,
        0x04,
        0x19,
        0x00,
        other as u8,
        0xFF,
        0xFF,
        0,
        0x1B,
    ]
    .into_iter()
    .chain(compact(add_name))
    .chain([0x17, 0x16])
    .collect::<Vec<_>>();
    b.func(give, destroyed, other, &give_code, 28, 0, ff::DEFINED);
    let delete_name = b.name("DeleteInventory");
    // Destroyed -> Owner.DeleteInventory(self). No native inventory repair is allowed.
    let destroyed_code = [0x19, 0x01, inv_owner as u8, 0xFF, 0xFF, 0, 0x1B]
        .into_iter()
        .chain(compact(delete_name))
        .chain([0x17, 0x16, 0x04, 0x0B])
        .collect::<Vec<_>>();
    b.func(
        destroyed,
        0,
        0,
        &destroyed_code,
        18,
        0,
        ff::DEFINED | ff::EVENT,
    );
    let item = b.reserve(IMP_OBJPROP, delete, "Item");
    b.prop_with(item, 0, pf::PARM, &compact(0));
    // Minimal head unlink authored for this fixture: Inventory = Item.Inventory;
    // Item.Inventory = None; Item.Owner = None. The tests delete the current head.
    let delete_code = vec![
        0x0F,
        0x01,
        pi,
        0x19,
        0x00,
        item as u8,
        0xFF,
        0xFF,
        0,
        0x01,
        pi,
        0x0F,
        0x19,
        0x00,
        item as u8,
        0xFF,
        0xFF,
        0,
        0x01,
        pi,
        0x2A,
        0x0F,
        0x19,
        0x00,
        item as u8,
        0xFF,
        0xFF,
        0,
        0x01,
        inv_owner as u8,
        0x2A,
        0x04,
        0x0B,
    ];
    b.func(delete, 0, item, &delete_code, 54, 0, ff::DEFINED);
    b.class(object, 0, neq);
    b.class(inventory, object, inv_inv);
    b.class(ammo, inventory, 0);
    b.class(fists_ammo, ammo, 0);
    b.class(nine_mm_ammo, ammo, 0);
    b.class(nine_mm_child, nine_mm_ammo, 0);
    b.class(pawn, inventory, add);
    b.build()
}

#[test]
fn find_inventory_type_does_not_match_a_subclass_of_the_requested_ammo() {
    let set = set_of(inventory_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    let pawn = vm.spawn(g(&set, "Pawn"), "P").unwrap();
    let ammo = vm.spawn(g(&set, "NineMmAmmoChild"), "PickedAmmo").unwrap();
    vm.set_active(pawn, true);
    vm.set_active(ammo, true);
    vm.set_property(
        pawn,
        "Inventory",
        0,
        Value::Object(Some(ObjRef::Instance(ammo))),
    );

    let mut query = [Value::Object(Some(ObjRef::Static(g(&set, "Ammo"))))];
    assert_eq!(
        call_native(
            &mut vm,
            "Engine.Pawn.FindInventoryType",
            pawn,
            &[],
            &mut query
        ),
        NativeOutcome::Value(Value::Object(None)),
        "a 9 mm subclass must not be returned for an Ammo/base-class query"
    );

    let mut exact_query = [Value::Object(Some(ObjRef::Static(g(
        &set,
        "NineMmAmmoChild",
    ))))];
    assert_eq!(
        call_native(
            &mut vm,
            "Engine.Pawn.FindInventoryType",
            pawn,
            &[],
            &mut exact_query
        ),
        NativeOutcome::Value(Value::Object(Some(ObjRef::Instance(ammo)))),
        "the same linked item must be found by its concrete class"
    );
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

/// Tick suspension must not interrupt synchronous GiveTo/AddInventory or the Destroyed
/// callback to the owner. Duplicate items, an inactive owner and disabled Destroyed probes
/// expose the old call gate and the native unlink workaround independently.
#[test]
fn item49b_inventory_callbacks_run_on_inactive_and_suspended_owners() {
    let set = set_of(inventory_package());
    for suspended in [false, true] {
        let mut vm = Vm::new(&set, VmLimits::default());
        let owner = vm.spawn(g(&set, "Pawn"), "Corpse").unwrap();
        vm.objects[owner as usize].suspended = suspended;
        let first = vm.spawn(g(&set, "Inventory"), "First").unwrap();
        let key = vm.spawn(g(&set, "Inventory"), "Key").unwrap();
        let give = g(&set, "Inventory.GiveTo");
        for item in [first, key] {
            let result = vm
                .call_function(
                    give,
                    item,
                    vec![Value::Object(Some(ObjRef::Instance(owner)))],
                )
                .unwrap();
            assert_eq!(result, Value::Bool(true));
        }
        assert_eq!(obj_prop(&vm, owner, "Inventory"), Some(first));
        assert_eq!(obj_prop(&vm, first, "Inventory"), Some(key));
        assert_eq!(
            vm.call_function(
                give,
                key,
                vec![Value::Object(Some(ObjRef::Instance(owner)))]
            )
            .unwrap(),
            Value::Bool(false)
        );
        assert_eq!(
            obj_prop(&vm, key, "Inventory"),
            None,
            "duplicate must not form a cycle"
        );
        vm.destroy(first).unwrap();
        assert_eq!(
            obj_prop(&vm, owner, "Inventory"),
            Some(key),
            "Destroyed must call unticked owner"
        );
        assert_eq!(obj_prop(&vm, first, "Owner"), None);
        vm.destroy(key).unwrap();
        assert_eq!(obj_prop(&vm, owner, "Inventory"), None);
        assert!(
            !vm.objects[owner as usize].active,
            "a direct call must not enable scheduled ticking"
        );
        assert_eq!(vm.objects[owner as usize].suspended, suspended);
        assert_eq!(vm.suspended_deferred_calls(), 0);
        assert!(
            !vm.trace
                .iter()
                .any(|e| matches!(e.kind, TraceKind::Deferred { .. }))
        );
    }
}

#[test]
fn item49b_destroy_without_inventory_callback_does_not_repair_chain() {
    let set = set_of(inventory_package());
    let mut vm = Vm::new(&set, VmLimits::default());
    let owner = vm.spawn(g(&set, "Pawn"), "Owner").unwrap();
    let item = vm.spawn(g(&set, "Inventory"), "Item").unwrap();
    vm.call_function(
        g(&set, "Inventory.GiveTo"),
        item,
        vec![Value::Object(Some(ObjRef::Instance(owner)))],
    )
    .unwrap();
    // Disabling Destroyed intentionally prevents the script unlink. DestroyActor must not
    // silently synthesize inventory semantics when the callback is absent.
    vm.disable_probe(item, "All", true);
    vm.destroy(item).unwrap();
    assert_eq!(obj_prop(&vm, owner, "Inventory"), Some(item));
    assert_eq!(obj_prop(&vm, item, "Owner"), Some(owner));
}

/// Synthetic package with a `Pickup` class carrying `Location` (`Core.Struct` `Vector`),
/// `CollisionHeight` (`float`), `bCollideWorld` (`bool`) and `Physics` (`byte`).
fn pickup_physics_fixture() -> Vec<u8> {
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
    let physics = b.reserve(B_BYTEPROP, collide, "Physics");
    b.prop_with(loc, height, 0, &compact(vector));
    b.prop(height, collide, 0);
    b.prop(collide, physics, 0);
    b.prop_with(physics, 0, 0, &compact(0));
    b.class(object, 0, 0);
    b.class(actor, object, 0);
    b.class(pickup, actor, loc);
    b.build()
}

#[test]
fn physics_none_pickup_keeps_its_authored_location_above_floor() {
    let set = set_of(pickup_physics_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_physics(Box::new(crate::physics::FlatPhysics::new(100.0)));
    let p = vm.spawn(g(&set, "Pickup"), "Key").unwrap();
    // PHYS_None does not floor-snap a map-placed actor, even when world queries are available.
    vm.set_property(p, "Location", 0, Value::Vector([10.0, 20.0, 140.0]));
    vm.set_property(p, "CollisionHeight", 0, Value::Float(8.0));
    vm.set_property(p, "bCollideWorld", 0, Value::Bool(true));
    vm.set_property(p, "Physics", 0, Value::Byte(0)); // PHYS_None
    vm.begin_play(&[p]).unwrap();
    assert_eq!(
        vm.vector_prop(p, "Location").unwrap(),
        [10.0, 20.0, 140.0],
        "PHYS_None preserves the authored position above the floor"
    );
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
    let proj_target = b.reserve(B_BOOLPROP, actor, "bProjTarget");
    let bzero = b.reserve(B_BOOLPROP, actor, "bBlockZeroExtentTraces");
    let bnz = b.reserve(B_BOOLPROP, actor, "bBlockNonZeroExtentTraces");
    let bblocka = b.reserve(B_BOOLPROP, actor, "bBlockActors");
    let bblockp = b.reserve(B_BOOLPROP, actor, "bBlockPlayers");
    b.prop_with(loc, rot, 0, &compact(vector));
    b.prop_with(rot, radius, 0, &compact(rotator));
    b.prop(radius, height, 0);
    b.prop(height, collide, 0);
    b.prop(collide, proj_target, 0);
    b.prop(proj_target, bzero, 0);
    b.prop(bzero, bnz, 0);
    b.prop(bnz, bblocka, 0);
    b.prop(bblocka, bblockp, 0);
    b.prop(bblockp, 0, 0);
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
        // Decoded engine filter: hash membership (`bCollideActors`) plus the extent prefilter
        // (`bBlockZeroExtentTraces`) plus `ShouldTrace` admission (block flags).
        vm.set_property(id, "bCollideActors", 0, Value::Bool(true));
        vm.set_property(id, "bBlockActors", 0, Value::Bool(true));
        vm.set_property(id, "bBlockPlayers", 0, Value::Bool(true));
        vm.set_property(id, "bBlockZeroExtentTraces", 0, Value::Bool(true));
        // Measured Plage01 pawn shape: `bCollideActors=false` with `bProjTarget=true`.
        vm.set_property(id, "bProjTarget", 0, Value::Bool(true));
        // item40e: only collision-hash actors (`bCollideActors`) can be hit.
        vm.set_property(id, "bCollideActors", 0, Value::Bool(true));
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
fn return_valued_call_on_inactive_placed_actor_runs_synchronously() {
    let set = set_of(list_fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let caller = vm.spawn(sg(&set, "Actor"), "Caller").unwrap();
    vm.set_active(caller, true);
    let target = vm.spawn(sg(&set, "Actor"), "Target").unwrap();
    // The target is a placed actor outside the executed scope: a call that must return a value
    // can never be deferred (the engine's `execVirtualFunction -> CallFunction` runs the callee
    // frame synchronously). Measured caller: `XIIIBulletsAmmo.ProcessTraceHit` (xiii.u 0x029D)
    // reads `XIIIPawn(Other).GetDamageLocation(...)` on a parked, non-active soldier; the old
    // deferral error aborted the bullet chain before `Other.TakeDamage`.
    let value = vm
        .call_function(sg(&set, "Actor.Echo"), target, vec![Value::Int(41)])
        .expect("a return-valued call on a non-active placed actor must run synchronously");
    assert_eq!(value, Value::Int(41));
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

#[test]
fn percent_float_float_uses_fmod_for_negative_and_zero_divisors() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let object = vm.spawn(g(&set, "Object"), "O").unwrap();

    let mut args = [Value::Float(5.5), Value::Float(2.0)];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.Percent_FloatFloat",
            object,
            &[false, false],
            &mut args,
        ),
        NativeOutcome::Value(Value::Float(1.5))
    );
    // C/UE2 fmod retains the dividend's sign; this is not Euclidean modulo.
    let mut args = [Value::Float(-5.5), Value::Float(2.0)];
    assert_eq!(
        call_native(
            &mut vm,
            "Object.Percent_FloatFloat",
            object,
            &[false, false],
            &mut args,
        ),
        NativeOutcome::Value(Value::Float(-1.5))
    );
    // The native mirrors fmod's floating-point zero-divisor result (NaN), not an integer-style
    // DivisionByZero error or a silent zero fallback.
    let mut args = [Value::Float(1.0), Value::Float(0.0)];
    assert!(matches!(
        call_native(
            &mut vm,
            "Object.Percent_FloatFloat",
            object,
            &[false, false],
            &mut args,
        ),
        NativeOutcome::Value(Value::Float(v)) if v.is_nan()
    ));
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
            tween_duration: 0.0,
            tween_only: false,
            loop_end_sent: false,
            tween_source: None,
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
fn particle_spawn_native_queues_a_vm_owned_emitter_request_once() {
    let set = spawn_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let emitter = vm.spawn(sg(&set, "Actor"), "EmitterSubobject").unwrap();
    let mut a = vec![Value::Int(3)];
    let out = call_native(
        &mut vm,
        "ParticleEmitter.SpawnParticle",
        emitter,
        &[false],
        &mut a,
    );
    assert!(matches!(out, NativeOutcome::Value(Value::Void)));
    assert_eq!(vm.drain_particle_spawns(), vec![(emitter, 3)]);
    assert!(vm.drain_particle_spawns().is_empty(), "requests drain once");

    let mut a = vec![Value::Int(-1)];
    call_native(
        &mut vm,
        "ParticleEmitter.SpawnParticle",
        emitter,
        &[false],
        &mut a,
    );
    assert!(
        vm.drain_particle_spawns().is_empty(),
        "negative amount is inert"
    );
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

/// item21: a controllable host playback provider for the VM-state tests. The flags are shared
/// through an `Rc` so a test can flip them after the VM owns the boxed provider.
#[derive(Default)]
struct FakeVideoHostState {
    open_ok: std::cell::Cell<bool>,
    finished: std::cell::Cell<bool>,
    errored: std::cell::Cell<bool>,
    plays: std::cell::Cell<u32>,
    stops: std::cell::Cell<u32>,
    last_open: std::cell::RefCell<Option<String>>,
}

#[derive(Clone, Default)]
struct FakeVideoHost {
    s: std::rc::Rc<FakeVideoHostState>,
}

impl crate::vm::VideoPlayerHost for FakeVideoHost {
    fn open(&mut self, name: &str) -> Option<f32> {
        *self.s.last_open.borrow_mut() = Some(name.to_owned());
        self.s.open_ok.get().then_some(12.5)
    }
    fn play(&mut self) {
        self.s.plays.set(self.s.plays.get() + 1);
    }
    fn stop(&mut self) {
        self.s.stops.set(self.s.stops.get() + 1);
    }
    fn finished(&self) -> bool {
        self.s.finished.get()
    }
    fn errored(&self) -> bool {
        self.s.errored.get()
    }
}

/// item21: with a host provider installed, `Open` decodes through the host, `GetStatus` reports
/// `1` while the host playback runs, `0` at its actual end and `2` on playback failure; the
/// registered duration is ignored for host-decoded clips.
#[test]
fn host_playback_drives_video_status() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let host = FakeVideoHost::default();
    host.s.open_ok.set(true);
    vm.set_video_host(Box::new(host.clone()));
    vm.set_video_duration("cine01", 100.0); // must not matter for a host-decoded clip
    assert!(vm.has_video_host());
    assert!(vm.video_open("Cine01.bik"));
    assert_eq!(host.s.last_open.borrow().as_deref(), Some("cine01"));
    assert_eq!(vm.video_timing(), Some(crate::vm::VideoTiming::Host));
    assert_eq!(vm.video_duration(), Some(12.5));
    // Not started: status 0 even though the clip is open.
    assert_eq!(vm.video_status(), 0);
    vm.video_play();
    assert_eq!(host.s.plays.get(), 1);
    assert_eq!(vm.video_status(), 1);
    // VM time passing the (irrelevant) registered duration changes nothing while playing.
    vm.tick(200.0).unwrap();
    assert_eq!(
        vm.video_status(),
        1,
        "host playback, not VM time, ends the clip"
    );
    // The host playback ends: status 0 at the host's actual end.
    host.s.finished.set(true);
    assert_eq!(vm.video_status(), 0);
    // Playback failure is the game's error status ("Error playing video").
    host.s.finished.set(false);
    host.s.errored.set(true);
    assert_eq!(vm.video_status(), 2);
    // Stop clears the clip and stops the host playback.
    host.s.errored.set(false);
    vm.video_stop();
    assert_eq!(host.s.stops.get(), 1);
    assert_eq!(vm.video_status(), 0);
    assert_eq!(vm.video_name(), None);
}

/// item21: when the host cannot decode the file (`open` -> `None`) the registered Bink-header
/// duration times the clip (labelled fallback); with neither, the clip is untimed and reports
/// finished. A second `Open` stops the clip the host is still playing.
#[test]
fn host_open_failure_falls_back_and_reopen_stops() {
    let set = set_of(fixture());
    let mut vm = Vm::new(&set, VmLimits::default());
    let host = FakeVideoHost::default();
    host.s.open_ok.set(true);
    vm.set_video_host(Box::new(host.clone()));
    vm.set_video_duration("other", 5.0);
    // Host decode failure + registered duration: duration fallback, timed from Play.
    host.s.open_ok.set(false);
    assert!(vm.video_open("other"));
    assert_eq!(vm.video_timing(), Some(crate::vm::VideoTiming::Duration));
    vm.video_play();
    assert_eq!(vm.video_status(), 1);
    vm.tick(4.9).unwrap();
    assert_eq!(vm.video_status(), 1);
    vm.tick(0.2).unwrap();
    assert_eq!(vm.video_status(), 0, "4.9 + 0.2 s >= 5 s");
    // Neither: untimed, finished immediately, `Open` still returns false.
    assert!(!vm.video_open("unknown"));
    assert_eq!(vm.video_timing(), Some(crate::vm::VideoTiming::Untimed));
    vm.video_play();
    assert_eq!(vm.video_status(), 0);
    // While the second clip is open, a third `Open` stops the host playback of the previous one.
    host.s.open_ok.set(true);
    assert!(vm.video_open("cine01"));
    vm.video_play();
    assert_eq!(
        host.s.plays.get(),
        1,
        "only the host-decoded clip reaches host play (the fallback clip does not)"
    );
    assert!(vm.video_open("cine02"));
    assert_eq!(host.s.stops.get(), 1, "re-opening must stop the prior clip");
    // Play without an open clip never reaches the host.
    vm.video_stop();
    vm.video_play();
    assert_eq!(host.s.plays.get(), 1);
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

#[test]
fn tween_only_holds_first_frame_and_captures_interrupted_source() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(4, 2.0)));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    play_anim(&mut vm, a, "Walk", 1.0, 0);
    vm.tick(0.5).unwrap();
    let mut args = [Value::Name("Idle".into()), Value::Float(1.0), Value::Int(0)];
    try_native(&mut vm, "Engine.Actor.TweenAnim", a, &[false; 3], &mut args).unwrap();
    vm.tick(0.25).unwrap();
    let view = vm.actor_animation(a).unwrap();
    let c = &view.channels[0];
    assert_eq!(c.frame, 0.0);
    assert_eq!(c.rate, 0.0);
    assert_eq!(c.tween_remaining, 0.75);
    assert_eq!(c.tween_source.as_ref().unwrap().frame, 1.0);
    assert_eq!(c.tween_source.as_ref().unwrap().sequence, "Walk");
    args[0] = Value::Name("Run".into());
    try_native(&mut vm, "Engine.Actor.TweenAnim", a, &[false; 3], &mut args).unwrap();
    let c = vm.actor_animation(a).unwrap().channels.remove(0);
    assert_eq!(c.tween_source.as_ref().unwrap().tween_remaining, 0.75);
    vm.tick(1.25).unwrap();
    assert_eq!(anim_end_count(&vm), 1);
    vm.tick(2.0).unwrap();
    let c = vm.actor_animation(a).unwrap().channels.remove(0);
    assert_eq!(c.frame, 0.0);
    assert!(!c.active);
    assert!(c.tween_source.is_none());
}

#[test]
fn blend_to_alpha_preserves_subtree_and_is_independent_of_step_size() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(100, 1.0)));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    vm.anim_blend_params(a, 1, 0.2, 0.5, 0.25, Some("Arm".into()));
    vm.anim_blend_to_alpha(a, 1, 0.8, 1.0);
    for _ in 0..4 {
        vm.tick(0.125).unwrap();
    }
    let p = vm.objects[a as usize].anim.blend_params.get(&1).unwrap();
    assert!((p.blend_alpha - 0.5).abs() < 1e-6);
    assert_eq!(p.bone_name.as_deref(), Some("Arm"));
    assert_eq!(p.in_time, 0.5);
    vm.tick(1.0).unwrap();
    assert!((vm.objects[a as usize].anim.blend_params[&1].blend_alpha - 0.8).abs() < 1e-6);
    vm.anim_blend_to_alpha(a, 1, 0.0, 0.0);
    assert_eq!(
        vm.objects[a as usize].anim.blend_params[&1].blend_alpha,
        0.0
    );
    vm.anim_blend_params(a, 0, 0.0, 0.0, 0.0, None);
    assert!(!vm.objects[a as usize].anim.blend_params.contains_key(&0));
}

#[test]
fn channel_params_return_requested_sequence_and_loop_notifies_survive_multiple_wraps() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(ScriptedAnim {
        frames: 4,
        rate: 1.0,
        notifies: vec![(0.25, "Foot".into()), (0.75, "Foot".into())],
    }));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    play_anim(&mut vm, a, "Walk", 1.0, 0);
    play_anim(&mut vm, a, "Aim", 1.0, 1);
    let mut args = [
        Value::Int(0),
        Value::Name("None".into()),
        Value::Float(0.0),
        Value::Float(0.0),
    ];
    try_native(
        &mut vm,
        "Engine.Actor.GetAnimParams",
        a,
        &[false; 4],
        &mut args,
    )
    .unwrap();
    assert_eq!(args[1], Value::Name("Walk".into()));
    assert_eq!(args[3], Value::Float(0.25));
    assert_eq!(
        vm.get_property(a, "AnimSequence"),
        Some(&Value::Name("Walk".into()))
    );
    let st = vm.objects[a as usize].anim.channels.get_mut(&0).unwrap();
    st.looping = true;
    vm.tick(9.5).unwrap();
    assert_eq!(vm.objects[a as usize].anim.channels[&0].frame, 1.5);
    let count = vm
        .trace
        .iter()
        .filter(|e| matches!(e.kind, TraceKind::AnimNotify { channel: 0, .. }))
        .count();
    assert_eq!(count, 5);
    assert_eq!(vm.anim_channel_params(a, 0), Some((0.375, 0.25)));
    assert_eq!(
        vm.get_property(a, "bAnimFinished"),
        Some(&Value::Bool(false))
    );
}

#[test]
fn repeated_bone_controls_replace_previous_request() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    for yaw in 0..1000 {
        vm.add_bone_rotation(a, "Arm".into(), [0, yaw, 0], 0, 1.0);
    }
    let bs = vm.bone_state(a).unwrap();
    assert_eq!(bs.rotations.len(), 1);
    assert_eq!(bs.rotations[0].turn[1], 999);
}

#[test]
fn animation_rejects_nonfinite_and_interruptions_stay_bounded() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    vm.set_animation_data(Box::new(crate::animation::FixedAnimation::new(4, 2.0)));
    let a = vm.spawn(sg(&set, "Actor"), "A").unwrap();
    vm.set_active(a, true);
    assert!(
        vm.start_animation(a, "Walk", f32::NAN, 0.0, 0, false)
            .is_err()
    );
    assert!(
        vm.start_animation(a, "Walk", 1.0, f32::INFINITY, 0, false)
            .is_err()
    );
    // Authored cine scripts re-run `LoopAnim` on the same channel every tick (measured:
    // Plage01 `CineController2` -> `Cine2.CineInit.PlayMoving`). The tween source freezes
    // one level instead of growing an unbounded interruption chain, so repeated tweened
    // starts on one actor keep working and sampling stays bounded.
    for _ in 0..200 {
        vm.start_animation(a, "Walk", 1.0, 1.0, 0, false).unwrap();
    }
    vm.tick(1.0).unwrap();
    vm.start_animation(a, "Walk", 1.0, 0.0, 0, true).unwrap();
    assert!(vm.tick(10000.0).is_err());
}

#[test]
fn attach_to_bone_writes_the_reflected_attachment_bone_field() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let parent = vm.spawn(sg(&set, "Actor"), "Parent").unwrap();
    let child = vm.spawn(sg(&set, "Actor"), "Child").unwrap();
    let mut args = [
        Value::Object(Some(ObjRef::Instance(child))),
        Value::Name("Arm".into()),
    ];
    let out = try_native(
        &mut vm,
        "Engine.Actor.AttachToBone",
        parent,
        &[false; 2],
        &mut args,
    )
    .unwrap();
    assert!(matches!(out, NativeOutcome::Value(Value::Bool(true))));
    assert_eq!(
        vm.get_property(child, "AttachmentBone"),
        Some(&Value::Name("Arm".into()))
    );
    assert_eq!(
        vm.get_property(child, "Base"),
        Some(&Value::Object(Some(ObjRef::Instance(parent))))
    );
    args[0] = Value::Object(None);
    assert!(matches!(
        try_native(
            &mut vm,
            "Engine.Actor.AttachToBone",
            parent,
            &[false; 2],
            &mut args
        )
        .unwrap(),
        NativeOutcome::Value(Value::Bool(false))
    ));
}

/// item49b regression: Banque01's ending flow stalled because `CineMalletteSM.Trigger`
/// (0x00E3) calls `Actor.DetachFromBone` (native 403), which used to be unimplemented; the
/// error suspended the mallette actor and the escape controller's action never advanced. The
/// detach must clear exactly the link `AttachToBone` recorded, refuse actors based elsewhere,
/// and survive a repeated detach on an already world-based actor.
#[test]
fn detach_from_bone_clears_the_attach_link_and_refuses_foreign_bases() {
    let set = anim_set();
    let mut vm = Vm::new(&set, VmLimits::default());
    let parent = vm.spawn(sg(&set, "Actor"), "Parent").unwrap();
    let child = vm.spawn(sg(&set, "Actor"), "Child").unwrap();
    let other = vm.spawn(sg(&set, "Actor"), "Other").unwrap();
    let mut args = [
        Value::Object(Some(ObjRef::Instance(child))),
        Value::Name("Arm".into()),
    ];
    assert!(matches!(
        try_native(
            &mut vm,
            "Engine.Actor.AttachToBone",
            parent,
            &[false; 2],
            &mut args
        )
        .unwrap(),
        NativeOutcome::Value(Value::Bool(true))
    ));
    // Detaching from a different actor must refuse and leave the link untouched.
    assert!(matches!(
        try_native(
            &mut vm,
            "Engine.Actor.DetachFromBone",
            other,
            &[false; 1],
            &mut args
        )
        .unwrap(),
        NativeOutcome::Value(Value::Bool(false))
    ));
    assert_eq!(
        vm.get_property(child, "Base"),
        Some(&Value::Object(Some(ObjRef::Instance(parent))))
    );
    // The real detach: clears Base and the recorded bone.
    assert!(matches!(
        try_native(
            &mut vm,
            "Engine.Actor.DetachFromBone",
            parent,
            &[false; 1],
            &mut args
        )
        .unwrap(),
        NativeOutcome::Value(Value::Bool(true))
    ));
    assert_eq!(vm.get_property(child, "Base"), Some(&Value::Object(None)));
    assert_eq!(
        vm.get_property(child, "AttachmentBone"),
        Some(&Value::Name("None".into()))
    );
    // A second detach is a no-op refusal (the engine has nothing to undo).
    assert!(matches!(
        try_native(
            &mut vm,
            "Engine.Actor.DetachFromBone",
            parent,
            &[false; 1],
            &mut args
        )
        .unwrap(),
        NativeOutcome::Value(Value::Bool(false))
    ));
    // A None attachment is refused, not a silent success.
    args[0] = Value::Object(None);
    assert!(matches!(
        try_native(
            &mut vm,
            "Engine.Actor.DetachFromBone",
            parent,
            &[false; 1],
            &mut args
        )
        .unwrap(),
        NativeOutcome::Value(Value::Bool(false))
    ));
}

/// Synthetic recursion fixture: `Actor.Run()` calls itself virtually and never returns, so only
/// the interpreter's call-depth guard can stop it. This is the shape of the Amos01
/// campaign-start recursion (`xiii.u XIIIPlayerController.SwitchWeapon` 0x00FC -> 0x00FC).
/// The guard limit itself follows the engine: Core.dll `UObject::ProcessInternal` compares
/// the runaway counter against 250 (`cmp $0xfa` at VA 0x101166e0) and logs "Infinite script
/// recursion (%i calls) detected" (string VA 0x10178cd8) past it.
fn recursion_package() -> Vec<u8> {
    let mut b = B::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let run = b.reserve(IMP_FUNCTION, actor, "Run");
    // Run(): Run(); return;  — a virtual self-call with no arguments. A name operand is 1 byte
    // on disk but NAME_MEMORY_SIZE (4) in memory, so the code is 8 memory bytes.
    let mut code = Vec::new();
    code.push(0x1B); // VirtualFunction
    code.extend(compact(b.name("Run")));
    code.push(0x16); // EndFunctionParms
    code.push(0x04); // Return
    code.push(0x0B); // Nothing
    b.func(run, 0, 0, &code, 8, 0, ff::DEFINED);
    b.class(object, 0, 0);
    b.class(actor, object, run);
    b.build()
}

/// The unbounded-recursion property at the engine's own limit: at `VmLimits::default()` (250,
/// the Core.dll `ProcessInternal` constant) the guard aborts with `CallDepthExceeded` **inside
/// a 2 MiB thread stack** — the libtest default (`RUST_MIN_STACK`), i.e. the budget every test
/// already runs on, with no wrapper needed. The measured interpreter cost is ~4.4 KiB per
/// interpreted frame (debug build), so 250 frames need ~1.1 MiB: the guard fires at roughly
/// half the budget. If the default limit is ever raised past what that budget supports, this
/// thread dies with a stack overflow and the test (loudly) fails. The shipped host entry
/// points run their VM-driving code on an explicit 64 MiB stack instead (see xiii-app
/// `vmstack`, which exists for the binary's 1 MiB main thread).
#[test]
fn recursion_guard_fits_a_2mib_stack() {
    let package = recursion_package();
    let limit = VmLimits::default().max_call_depth;
    let outcome = std::thread::Builder::new()
        .stack_size(2 * 1024 * 1024)
        .spawn(move || {
            let set = set_of(package);
            let mut vm = Vm::new(&set, VmLimits::default());
            let actor = vm.spawn(g(&set, "Actor"), "Rec").unwrap();
            vm.set_active(actor, true);
            match vm.call_function(g(&set, "Actor.Run"), actor, vec![]) {
                Err(e) => format!("{:?}", e.kind),
                Ok(_) => "no error".to_owned(),
            }
        })
        .expect("spawn the 2 MiB probe thread")
        .join()
        .expect("the recursion must abort inside a 2 MiB stack, not overflow it");
    assert_eq!(
        outcome,
        format!("CallDepthExceeded {{ limit: {limit} }}"),
        "the engine-limit call-depth guard (250) must fire before a 2 MiB stack is exhausted"
    );
}

// ---------------------------------------------------------------------------------------
// item41c: Actor.MakeNoise -> CheckNoiseHearing -> CanHear -> HearNoise (Engine.dll decoded)

/// `HearNoise` probe bit (EName 342 - 300).
const HEAR_BIT: u64 = 1 << 42;

impl SpawnB {
    /// A `Core.Class` with explicit stored `ProbeMask` / `IgnoreMask`.
    fn class_masks(&mut self, r: i32, sup: i32, children: i32, probe: u64, ignore: u64) {
        let friendly = self.exports[(r - 1) as usize].name;
        let system = self.name("System");
        let mut p = self.header(sup, 0, children, friendly, &[], 0);
        p.extend(probe.to_le_bytes());
        p.extend(ignore.to_le_bytes());
        p.extend(0xFFFFu16.to_le_bytes());
        p.extend(0u16.to_le_bytes());
        p.extend(0u16.to_le_bytes());
        p.extend([0u8; 16]);
        p.extend(compact(0));
        p.extend(compact(0));
        p.extend(compact(0));
        p.extend(compact(system));
        p.extend(compact(0));
        p.extend(compact(0));
        self.set(r, p);
    }

    /// A code-less `Core.State` with explicit stored `ProbeMask` / `IgnoreMask`.
    fn state_masks(&mut self, r: i32, next: i32, probe: u64, ignore: u64) {
        let friendly = self.exports[(r - 1) as usize].name;
        let mut p = compact(0);
        p.extend(self.header(0, next, 0, friendly, &[], 0));
        p.extend(probe.to_le_bytes());
        p.extend(ignore.to_le_bytes());
        p.extend(0xFFFFu16.to_le_bytes());
        p.extend(0u16.to_le_bytes());
        self.set(r, p);
    }
}

/// Synthetic Engine-shaped hierarchy: `Actor` (Level, Tag, Location, Instigator), `LevelInfo`
/// (NetMode, NavigationPointList, ControllerList), `Pawn` (hearing fields and noise slots),
/// `Controller` (no HearNoise body, probe mask 0), `AIController` (a `HearNoise` event body,
/// class probe bit 42, and a `Deaf` state that ignores it) and `NavigationPoint`.
fn hearing_fixture() -> Vec<u8> {
    use ff::*;
    use pf::*;
    let mut b = SpawnB::new();
    let object = b.reserve(0, 0, "Object");
    let actor = b.reserve(0, 0, "Actor");
    let level_info = b.reserve(0, 0, "LevelInfo");
    let pawn = b.reserve(0, 0, "Pawn");
    let controller = b.reserve(0, 0, "Controller");
    let ai = b.reserve(0, 0, "AIController");
    let nav = b.reserve(0, 0, "NavigationPoint");
    let obj = compact(0);
    let vec = compact(IMP_STRUCT);

    let level = b.reserve(IMP_OBJECTPROP, actor, "Level");
    let tag = b.reserve(IMP_NAMEPROP, actor, "Tag");
    let location = b.reserve(IMP_STRUCTPROP, actor, "Location");
    let instigator = b.reserve(IMP_OBJECTPROP, actor, "Instigator");
    b.prop_with(level, tag, 0, &obj);
    b.prop(tag, location, 0);
    b.prop_with(location, instigator, 0, &vec);
    b.prop_with(instigator, 0, 0, &obj);

    let net_mode = b.reserve(IMP_BYTEPROP, level_info, "NetMode");
    let nav_list = b.reserve(IMP_OBJECTPROP, level_info, "NavigationPointList");
    let ctrl_list = b.reserve(IMP_OBJECTPROP, level_info, "ControllerList");
    b.prop_with(net_mode, nav_list, 0, &obj);
    b.prop_with(nav_list, ctrl_list, 0, &obj);
    b.prop_with(ctrl_list, 0, 0, &obj);

    // Pawn fields, chained in declaration order.
    let pawn_fields: Vec<(i32, &str, Vec<u8>)> = vec![
        (IMP_OBJECTPROP, "Controller", obj.clone()),
        (IMP_BOOLPROP, "bLOSHearing", Vec::new()),
        (IMP_BOOLPROP, "bSameZoneHearing", Vec::new()),
        (IMP_BOOLPROP, "bAdjacentZoneHearing", Vec::new()),
        (IMP_BOOLPROP, "bMuffledHearing", Vec::new()),
        (IMP_BOOLPROP, "bAroundCornerHearing", Vec::new()),
        (IMP_FLOATPROP, "HearingThreshold", Vec::new()),
        (IMP_FLOATPROP, "Alertness", Vec::new()),
        (IMP_FLOATPROP, "BaseEyeHeight", Vec::new()),
        (IMP_STRUCTPROP, "noise1spot", vec.clone()),
        (IMP_FLOATPROP, "noise1time", Vec::new()),
        (IMP_FLOATPROP, "noise1loudness", Vec::new()),
        (IMP_STRUCTPROP, "noise2spot", vec.clone()),
        (IMP_FLOATPROP, "noise2time", Vec::new()),
        (IMP_FLOATPROP, "noise2loudness", Vec::new()),
    ];
    let refs: Vec<i32> = pawn_fields
        .iter()
        .map(|(class, name, _)| b.reserve(*class, pawn, name))
        .collect();
    for (i, (_, _, extra)) in pawn_fields.iter().enumerate() {
        b.prop_with(refs[i], refs.get(i + 1).copied().unwrap_or(0), 0, extra);
    }

    let c_pawn = b.reserve(IMP_OBJECTPROP, controller, "Pawn");
    let is_player = b.reserve(IMP_BOOLPROP, controller, "bIsPlayer");
    let next_controller = b.reserve(IMP_OBJECTPROP, controller, "NextController");
    let enemy = b.reserve(IMP_OBJECTPROP, controller, "Enemy");
    b.prop_with(c_pawn, is_player, 0, &obj);
    b.prop(is_player, next_controller, 0);
    b.prop_with(next_controller, enemy, 0, &obj);
    b.prop_with(enemy, 0, 0, &obj);

    let hear = b.reserve(IMP_FUNCTION, ai, "HearNoise");
    let deaf = b.reserve(IMP_STATE, ai, "Deaf");
    let hear_l = b.reserve(IMP_FLOATPROP, hear, "Loudness");
    let hear_m = b.reserve(IMP_OBJECTPROP, hear, "NoiseMaker");
    b.prop(hear_l, hear_m, PARM);
    b.prop_with(hear_m, 0, PARM, &obj);
    // `return;` (Return + Nothing).
    b.func(hear, deaf, hear_l, &[0x04, 0x0B], 2, 0, DEFINED | EVENT);
    b.state_masks(deaf, 0, 0, !HEAR_BIT);

    let next_nav = b.reserve(IMP_OBJECTPROP, nav, "nextNavigationPoint");
    let propagates = b.reserve(IMP_BOOLPROP, nav, "bPropagatesSound");
    b.prop_with(next_nav, propagates, 0, &obj);
    b.prop(propagates, 0, 0);

    b.class(object, 0, 0, 0);
    b.class(actor, object, level, 0);
    b.class(level_info, actor, net_mode, 0);
    b.class(pawn, actor, refs[0], 0);
    b.class(controller, actor, c_pawn, 0);
    b.class_masks(ai, controller, hear, HEAR_BIT, u64::MAX);
    b.class(nav, actor, next_nav, 0);
    b.build()
}

fn hearing_set() -> ScriptSet {
    let p = ScriptPackage::load(
        "Test",
        hearing_fixture(),
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("package");
    assert!(p.errors.is_empty(), "{:?}", p.errors);
    let mut set = ScriptSet::new();
    set.add(p);
    set
}

fn inst(id: ObjectId) -> Value {
    Value::Object(Some(ObjRef::Instance(id)))
}

/// A level with an optional world, plus helpers to place pawns/controllers on its lists.
struct HearWorld<'s> {
    vm: Vm<'s>,
    set: &'s ScriptSet,
    level: ObjectId,
}

impl<'s> HearWorld<'s> {
    fn new(set: &'s ScriptSet, world: Option<MockWorld>) -> Self {
        let mut vm = Vm::new(set, VmLimits::default());
        if let Some(w) = world {
            vm.set_physics(Box::new(w));
        }
        let level = vm.spawn(sg(set, "LevelInfo"), "Level").unwrap();
        Self { vm, set, level }
    }

    /// A pawn at `at` with `HearingThreshold`, LOS hearing on, eye height 0, Instigator = self.
    fn pawn(&mut self, name: &str, at: [f32; 3], threshold: f32) -> ObjectId {
        let p = self.vm.spawn(sg(self.set, "Pawn"), name).unwrap();
        let vm = &mut self.vm;
        vm.set_property(p, "Level", 0, inst(self.level));
        vm.set_property(p, "Location", 0, Value::Vector(at));
        vm.set_property(p, "Instigator", 0, inst(p));
        vm.set_property(p, "HearingThreshold", 0, Value::Float(threshold));
        vm.set_property(p, "bLOSHearing", 0, Value::Bool(true));
        p
    }

    /// A controller of `class` possessing `pawn`, pushed at the head of `ControllerList`.
    fn controller(&mut self, class: &str, name: &str, pawn: Option<ObjectId>) -> ObjectId {
        let c = self.vm.spawn(sg(self.set, class), name).unwrap();
        let vm = &mut self.vm;
        vm.set_property(c, "Level", 0, inst(self.level));
        if let Some(p) = pawn {
            vm.set_property(c, "Pawn", 0, inst(p));
            vm.set_property(p, "Controller", 0, inst(c));
        }
        let head = vm.get_property(self.level, "ControllerList").cloned();
        if let Some(head) = head {
            vm.set_property(c, "NextController", 0, head);
        }
        vm.set_property(self.level, "ControllerList", 0, inst(c));
        c
    }

    /// A pawn + controller pair; returns the pawn.
    fn listener(&mut self, class: &str, name: &str, at: [f32; 3], threshold: f32) -> ObjectId {
        let p = self.pawn(&format!("{name}Pawn"), at, threshold);
        self.controller(class, name, Some(p));
        p
    }

    /// The player: a `Controller` (no HearNoise probe) with `bIsPlayer`; returns the pawn.
    fn player(&mut self, at: [f32; 3]) -> ObjectId {
        let p = self.pawn("PlayerPawn", at, 1000.0);
        let c = self.controller("Controller", "Player", Some(p));
        self.vm.set_property(c, "bIsPlayer", 0, Value::Bool(true));
        p
    }

    fn noise(&mut self, source: ObjectId, loudness: f32) {
        self.vm.trace.clear();
        call_native(
            &mut self.vm,
            "Engine.Actor.MakeNoise",
            source,
            &[false],
            &mut [Value::Float(loudness)],
        );
    }

    /// `(controller name, args)` of every `HearNoise` delivered by the last noise.
    fn heard(&self) -> Vec<(String, Vec<String>)> {
        self.vm
            .trace
            .iter()
            .filter_map(|e| match &e.kind {
                TraceKind::Event {
                    target,
                    function,
                    args,
                } if function.ends_with("HearNoise") => Some((target.clone(), args.clone())),
                _ => None,
            })
            .collect()
    }

    fn heard_by(&self) -> Vec<String> {
        let mut names: Vec<String> = self.heard().into_iter().map(|(t, _)| t).collect();
        names.sort();
        names
    }

    fn notes(&self, needle: &str) -> usize {
        self.vm
            .trace
            .iter()
            .filter(|e| matches!(&e.kind, TraceKind::Note(n) if n.contains(needle)))
            .count()
    }

    fn f(&self, id: ObjectId, name: &str) -> f32 {
        match self.vm.get_property(id, name) {
            Some(Value::Float(v)) => *v,
            other => panic!("{name}: {other:?}"),
        }
    }

    fn v(&self, id: ObjectId, name: &str) -> [f32; 3] {
        match self.vm.get_property(id, name) {
            Some(Value::Vector(v)) => *v,
            other => panic!("{name}: {other:?}"),
        }
    }
}

/// Range: `HearingThreshold^2 * Loudness * max(0, Alertness + 1) >= DistSq` (inclusive, Z
/// included), and the event carries `(Loudness, NoiseMaker)`.
#[test]
fn make_noise_range_is_threshold_squared_times_loudness_and_alertness() {
    let set = hearing_set();
    let mut w = HearWorld::new(&set, Some(MockWorld::new()));
    let player = w.player([0.0, 0.0, 0.0]);
    let at = w.listener("AIController", "At", [1000.0, 0.0, 0.0], 1000.0);
    w.listener("AIController", "Beyond", [0.0, 1000.5, 0.0], 1000.0);
    // Z counts: (600, 0, 800) is exactly 1000 away.
    w.listener("AIController", "Diag", [600.0, 0.0, 800.0], 1000.0);
    w.noise(player, 1.0);
    assert_eq!(
        w.heard_by(),
        ["At", "Diag"],
        "the boundary is inclusive, 1000.5 is out"
    );
    let (_, args) = &w.heard()[0];
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], Value::Float(1.0).to_string());
    assert_eq!(args[1], w.vm.value_text(&inst(player)));

    // Loudness scales the squared range linearly: 1000.5^2 needs Loudness >= 1.001.
    w.vm.time += 1.0;
    w.noise(player, 1.002);
    assert_eq!(w.heard_by(), ["At", "Beyond", "Diag"]);

    // Alertness -1 zeroes perception; +1 doubles it.
    w.vm.set_property(at, "Alertness", 0, Value::Float(-1.0));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert_eq!(w.heard_by(), ["Diag"]);
    w.vm.set_property(at, "Alertness", 0, Value::Float(1.0));
    w.vm.set_property(at, "Location", 0, Value::Vector([1414.0, 0.0, 0.0]));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert_eq!(w.heard_by(), ["At", "Diag"], "2e6 >= 1414^2");
}

/// The line check runs from the listener's eye (`Location + BaseEyeHeight`) to the noise; a wall
/// that only crosses the eye line blocks hearing. Without a physics provider the native fails
/// explicitly once the line check is needed.
#[test]
fn make_noise_is_occluded_on_the_eye_line_and_needs_a_provider() {
    let set = hearing_set();
    // The eye line z = 100 from x 0 to 500 crosses the wall; the pawn-centre line to the noise
    // at (500, 0, 100) is at z = 50 at x = 250, below it.
    let wall = MockWorld::new().with_wall([240.0, -50.0, 80.0], [260.0, 50.0, 120.0]);
    let mut w = HearWorld::new(&set, Some(wall));
    let player = w.player([500.0, 0.0, 100.0]);
    let ear = w.listener("AIController", "Ear", [0.0, 0.0, 0.0], 1000.0);
    w.noise(player, 1.0);
    assert_eq!(
        w.heard_by(),
        ["Ear"],
        "eye height 0: the centre line is clear"
    );
    w.vm.set_property(ear, "BaseEyeHeight", 0, Value::Float(100.0));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert!(w.heard().is_empty(), "the eye line is blocked");
    w.vm.set_property(ear, "bLOSHearing", 0, Value::Bool(false));
    w.vm.set_property(ear, "BaseEyeHeight", 0, Value::Float(0.0));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert!(
        w.heard().is_empty(),
        "no bLOSHearing and no other mode: deaf"
    );

    let mut bare = HearWorld::new(&set, None);
    let player = bare.player([0.0, 0.0, 0.0]);
    bare.listener("AIController", "Ear", [100.0, 0.0, 0.0], 1000.0);
    let def = native("Engine.Actor.MakeNoise");
    let err = (def.f)(
        &mut bare.vm,
        &ctx(player, &[false], "Engine.Actor.MakeNoise"),
        &mut [Value::Float(1.0)],
    )
    .expect_err("no provider");
    assert!(
        matches!(err.kind, VmErrorKind::NoPhysicsProvider { .. }),
        "{err:?}"
    );
}

/// Who can hear: never the instigator's own controller, nor a controller that does not probe
/// `HearNoise` (no body, a state that ignores it, `Disable`), nor one without a pawn; no
/// instigator, an instigator without a controller and client net mode deliver nothing.
#[test]
fn make_noise_listener_filters() {
    let set = hearing_set();
    let mut w = HearWorld::new(&set, Some(MockWorld::new()));
    let player = w.player([0.0, 0.0, 0.0]);
    w.listener("AIController", "Ear", [100.0, 0.0, 0.0], 1000.0);
    w.listener("Controller", "Plain", [100.0, 0.0, 0.0], 1000.0);
    let deaf_pawn = w.listener("AIController", "Deaf", [100.0, 0.0, 0.0], 1000.0);
    let deaf = w.vm.obj_prop(deaf_pawn, "Controller").unwrap();
    w.vm.goto_state(deaf, "Deaf", None).unwrap();
    let disabled_pawn = w.listener("AIController", "Disabled", [100.0, 0.0, 0.0], 1000.0);
    let disabled = w.vm.obj_prop(disabled_pawn, "Controller").unwrap();
    w.vm.disable_probe(disabled, "HearNoise", true);
    w.controller("AIController", "NoPawn", None);
    w.noise(player, 1.0);
    assert_eq!(w.heard_by(), ["Ear"]);

    // Leaving the ignoring state restores the class probe.
    w.vm.goto_state(deaf, "None", None).unwrap();
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert_eq!(w.heard_by(), ["Deaf", "Ear"]);

    // The noise maker's own controller never hears it, even when it probes.
    let selfish = w.listener("AIController", "Selfish", [0.0, 0.0, 0.0], 1000.0);
    w.vm.time += 1.0;
    w.noise(selfish, 1.0);
    assert_eq!(w.heard_by(), ["Deaf", "Ear"]);

    // No instigator / an instigator without a controller: nothing, and no slot is written.
    let rock = w.pawn("Rock", [0.0, 0.0, 0.0], 0.0);
    w.vm.set_property(rock, "Instigator", 0, Value::Object(None));
    w.vm.time += 1.0;
    w.noise(rock, 1.0);
    assert!(w.heard().is_empty());
    w.vm.set_property(rock, "Instigator", 0, inst(rock));
    w.noise(rock, 1.0);
    assert!(w.heard().is_empty());
    assert_eq!(
        w.f(rock, "noise1time"),
        0.0,
        "no controller: returns before the slots"
    );

    // NM_Client (3): MakeNoise does nothing.
    w.vm.set_property(w.level, "NetMode", 0, Value::Byte(3));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert!(w.heard().is_empty());
    assert_eq!(
        w.f(player, "noise1time"),
        1.0,
        "the client-mode noise left no slot"
    );
}

/// A noise whose instigator is not a player and whose controller's enemy is not a player only
/// reaches controllers with the noise maker's Tag or a player pawn; a player enemy makes it
/// reach everyone.
#[test]
fn make_noise_non_player_noise_is_limited_to_tag_or_player() {
    let set = hearing_set();
    let mut w = HearWorld::new(&set, Some(MockWorld::new()));
    let soldier = w.listener("AIController", "Soldier", [0.0, 0.0, 0.0], 1000.0);
    let soldier_c = w.vm.obj_prop(soldier, "Controller").unwrap();
    // The pawn carries the tag here, its controller does not.
    let other = w.listener("AIController", "OtherTag", [100.0, 0.0, 0.0], 1000.0);
    let same = w.listener("AIController", "SameTag", [100.0, 0.0, 0.0], 1000.0);
    let same_c = w.vm.obj_prop(same, "Controller").unwrap();
    // A probing controller flagged bIsPlayer counts as a player listener.
    let human = w.listener("AIController", "Human", [100.0, 0.0, 0.0], 1000.0);
    let human_c = w.vm.obj_prop(human, "Controller").unwrap();
    w.vm.set_property(human_c, "bIsPlayer", 0, Value::Bool(true));
    for id in [soldier, same_c, other] {
        w.vm.set_property(id, "Tag", 0, Value::Name("Squad".into()));
    }
    w.noise(soldier, 1.0);
    assert_eq!(
        w.heard_by(),
        ["Human", "SameTag"],
        "the listener controller's Tag is compared with the noise maker's"
    );

    let player = w.player([5000.0, 0.0, 0.0]);
    w.vm.set_property(soldier_c, "Enemy", 0, inst(player));
    w.vm.time += 1.0;
    w.noise(soldier, 1.0);
    assert_eq!(w.heard_by(), ["Human", "OtherTag", "SameTag"]);
}

/// The two noise slots: a repeat within 0.2 s, within 50 uu and not more than 1/0.9 louder is
/// dropped entirely; otherwise the noise is stored (slot 1 when older than 0.18 s, else slot 2,
/// else the louder-than rules, the last of which writes slot 1).
#[test]
fn make_noise_slots_suppress_repeats_and_store_like_the_engine() {
    let set = hearing_set();
    let mut w = HearWorld::new(&set, Some(MockWorld::new()));
    let player = w.player([0.0, 0.0, 0.0]);
    w.listener("AIController", "Ear", [100.0, 0.0, 0.0], 1000.0);
    w.vm.time = 10.0;
    w.noise(player, 1.0);
    assert_eq!(w.heard().len(), 1);
    assert_eq!(w.f(player, "noise1time"), 10.0);
    assert_eq!(w.f(player, "noise1loudness"), 1.0);
    assert_eq!(w.v(player, "noise1spot"), [0.0, 0.0, 0.0]);

    // 0.1 s later, 49 uu away, 1.11x louder (0.9 * 1.11 <= 1): suppressed by slot 1.
    w.vm.time = 10.1;
    w.vm.set_property(player, "Location", 0, Value::Vector([49.0, 0.0, 0.0]));
    w.noise(player, 1.11);
    assert!(w.heard().is_empty(), "suppressed by slot 1");
    // 1.12x louder (0.9 * 1.12 > 1): heard and stored in slot 2 (slot 1 is younger than 0.18 s).
    w.noise(player, 1.12);
    assert_eq!(w.heard().len(), 1);
    assert_eq!(w.f(player, "noise1time"), 10.0);
    assert_eq!(w.f(player, "noise2time"), 10.1f64 as f32);
    assert_eq!(w.f(player, "noise2loudness"), 1.12);
    // 51 uu from slot 1 and 2 uu from slot 2: slot 2 suppresses it.
    w.vm.set_property(player, "Location", 0, Value::Vector([51.0, 0.0, 0.0]));
    w.noise(player, 1.0);
    assert!(w.heard().is_empty(), "suppressed by slot 2");

    // Both slots fresh and a far noise: slot 1 is not near, slot 2 is not louder than this one
    // (1.12 <= 1.5), so the engine overwrites slot 1, not slot 2.
    w.vm.time = 10.15;
    w.vm.set_property(player, "Location", 0, Value::Vector([500.0, 0.0, 0.0]));
    w.noise(player, 1.5);
    assert_eq!(w.heard().len(), 1);
    assert_eq!(w.v(player, "noise1spot"), [500.0, 0.0, 0.0]);
    assert_eq!(w.f(player, "noise1loudness"), 1.5);
    assert_eq!(w.f(player, "noise2loudness"), 1.12, "slot 2 untouched");

    // Both fresh, far and quieter than both: delivered (0.7e6 >= 800^2), nothing stored.
    w.vm.time = 10.2;
    w.vm.set_property(player, "Location", 0, Value::Vector([900.0, 0.0, 0.0]));
    w.noise(player, 0.7);
    assert_eq!(w.heard().len(), 1);
    assert_eq!(w.v(player, "noise1spot"), [500.0, 0.0, 0.0]);
    assert_eq!(w.f(player, "noise1loudness"), 1.5);

    // The window is strict: 0.25 s after slot 1, the same noise at the same spot is heard.
    w.vm.time = 10.4;
    w.vm.set_property(player, "Location", 0, Value::Vector([500.0, 0.0, 0.0]));
    w.noise(player, 1.5);
    assert_eq!(w.heard().len(), 1, "slot 1 is older than 0.2 s");
}

/// bMuffledHearing: through a wall the noise is still heard when
/// `Perceived > W*W + 4*DistSq`, W being the squared span between the two wall hits.
#[test]
fn make_noise_muffled_hearing_through_a_thin_wall() {
    let set = hearing_set();
    // A 10 uu wall: W = 100, W^2 = 1e4. DistSq = 300^2 = 9e4, 4*DistSq = 3.6e5.
    let thin = MockWorld::new().with_wall([145.0, -50.0, -50.0], [155.0, 50.0, 50.0]);
    let mut w = HearWorld::new(&set, Some(thin));
    let player = w.player([300.0, 0.0, 0.0]);
    let ear = w.listener("AIController", "Ear", [0.0, 0.0, 0.0], 700.0);
    w.noise(player, 1.0);
    assert!(w.heard().is_empty(), "occluded, no muffled hearing");
    w.vm.set_property(ear, "bMuffledHearing", 0, Value::Bool(true));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert_eq!(w.heard().len(), 1, "4.9e5 > 1e4 + 3.6e5");
    assert_eq!(
        w.notes("bMuffledHearing"),
        1,
        "the BSP-only stand-in is reported"
    );
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert_eq!(w.heard().len(), 1);
    assert_eq!(w.notes("bMuffledHearing"), 0, "reported once per VM");
    // HearingThreshold 610: 3.721e5 > 3.7e5: heard; 608: 3.69664e5 < 3.7e5: not heard.
    w.vm.set_property(ear, "HearingThreshold", 0, Value::Float(610.0));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert_eq!(w.heard().len(), 1);
    w.vm.set_property(ear, "HearingThreshold", 0, Value::Float(608.0));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert!(w.heard().is_empty());

    // A 20 uu wall: W = 400, W^2 + 4*DistSq = 1.6e5 + 3.6e5 = 5.2e5 > 4.9e5: not heard.
    let thick = MockWorld::new().with_wall([140.0, -50.0, -50.0], [160.0, 50.0, 50.0]);
    let mut w = HearWorld::new(&set, Some(thick));
    let player = w.player([300.0, 0.0, 0.0]);
    let ear = w.listener("AIController", "Ear", [0.0, 0.0, 0.0], 700.0);
    w.vm.set_property(ear, "bMuffledHearing", 0, Value::Bool(true));
    w.noise(player, 1.0);
    assert!(w.heard().is_empty());
}

/// bAroundCornerHearing: a sound-propagating navigation point within `Perceived / 8` (squared)
/// of both ends, with clear lines to the noise and to the eye, carries the noise around a wall.
#[test]
fn make_noise_around_corner_via_navigation_points() {
    let set = hearing_set();
    // Wall between the ear (0,0,0) and the noise (300,0,0); a nav point at (150,200,0) sees both.
    let wall = MockWorld::new().with_wall([140.0, -100.0, -50.0], [160.0, 100.0, 50.0]);
    let mut w = HearWorld::new(&set, Some(wall));
    let player = w.player([300.0, 0.0, 0.0]);
    let ear = w.listener("AIController", "Ear", [0.0, 0.0, 0.0], 1000.0);
    w.vm.set_property(ear, "bAroundCornerHearing", 0, Value::Bool(true));
    let nav = w.vm.spawn(sg(&set, "NavigationPoint"), "Nav").unwrap();
    w.vm.set_property(nav, "Location", 0, Value::Vector([150.0, 200.0, 0.0]));
    w.noise(player, 1.0);
    assert!(
        w.heard().is_empty(),
        "the nav point is not on the level list"
    );

    w.vm.set_property(w.level, "NavigationPointList", 0, inst(nav));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert!(w.heard().is_empty(), "bPropagatesSound is false");

    w.vm.set_property(nav, "bPropagatesSound", 0, Value::Bool(true));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert_eq!(w.heard().len(), 1);
    assert_eq!(w.notes("bAroundCornerHearing"), 1);

    // Each leg is 150^2 + 200^2 = 62500; Perceived / 8 must exceed it: HearingThreshold 707
    // gives 499849 / 8 = 62481.1: not heard.
    w.vm.set_property(ear, "HearingThreshold", 0, Value::Float(707.0));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert!(w.heard().is_empty());
    // A blocked leg (the nav point inside the wall's span) also fails.
    w.vm.set_property(ear, "HearingThreshold", 0, Value::Float(1000.0));
    w.vm.set_property(nav, "Location", 0, Value::Vector([150.0, 90.0, 0.0]));
    w.vm.time += 1.0;
    w.noise(player, 1.0);
    assert!(w.heard().is_empty());
}

/// Zone hearing flags are decoded but the VM has no zone model: the zone test is reported once
/// and the LOS branch still decides.
#[test]
fn make_noise_zone_hearing_is_reported_partial() {
    let set = hearing_set();
    let wall = MockWorld::new().with_wall([45.0, -50.0, -50.0], [55.0, 50.0, 50.0]);
    let mut w = HearWorld::new(&set, Some(wall));
    let player = w.player([100.0, 0.0, 0.0]);
    let ear = w.listener("AIController", "Ear", [0.0, 0.0, 0.0], 1000.0);
    w.vm.set_property(ear, "bSameZoneHearing", 0, Value::Bool(true));
    w.noise(player, 1.0);
    assert!(w.heard().is_empty(), "zone test unavailable, LOS blocked");
    assert_eq!(w.notes("zone hearing"), 1);
}

/// A NaN loudness makes `Perceived` NaN, which passes the engine's `Perceived < DistSq` reject
/// test (x87 unordered compare); the decoded behaviour is kept rather than filtered. A negative
/// loudness is never heard at a distance.
#[test]
fn make_noise_nan_and_negative_loudness_follow_the_x87_compares() {
    let set = hearing_set();
    let mut w = HearWorld::new(&set, Some(MockWorld::new()));
    let player = w.player([0.0, 0.0, 0.0]);
    w.listener("AIController", "Ear", [900.0, 0.0, 0.0], 1.0);
    w.noise(player, f32::NAN);
    assert_eq!(w.heard_by(), ["Ear"]);
    w.vm.time += 1.0;
    w.noise(player, -1.0);
    assert!(w.heard().is_empty());
}
