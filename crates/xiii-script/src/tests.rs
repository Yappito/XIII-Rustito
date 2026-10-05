//! Synthetic tests: token families, limits and errors, and generated version-100 packages
//! holding UStruct/UFunction/UState/UClass/UProperty payloads. No proprietary data.

use xiii_package::{Limits, ObjectRef, Package};

use crate::bytecode::{ScriptLimits, TokenKind, decode_script};
use crate::error::ScriptErrorKind;
use crate::reader::Tables;
use crate::reflect::{
    PropertyKind, ScriptObject, class_defaults, function_flags, read_script_object,
};
use crate::{ScriptPackage, ScriptSet};

// ---------------------------------------------------------------------------------------
// Encoding helpers

pub(crate) fn compact(v: i32) -> Vec<u8> {
    let neg = v < 0;
    let mut m = i64::from(v).unsigned_abs();
    let mut out = Vec::new();
    let mut first = (m & 0x3f) as u8;
    if neg {
        first |= 0x80;
    }
    m >>= 6;
    if m != 0 {
        first |= 0x40;
    }
    out.push(first);
    while m != 0 {
        let mut b = (m & 0x7f) as u8;
        m >>= 7;
        if m != 0 {
            b |= 0x80;
        }
        out.push(b);
    }
    out
}

const TABLES: Tables = Tables {
    names: 8,
    imports: 2,
    exports: 200,
};

/// Name 0 is `None` in these token tests.
fn decode(bytes: &[u8], size: u32) -> crate::Result<crate::Script> {
    decode_script(
        bytes,
        0,
        bytes.len(),
        size,
        TABLES,
        &|n| n == 0,
        &ScriptLimits::default(),
    )
}

fn decode_with(bytes: &[u8], size: u32, limits: ScriptLimits) -> crate::Result<crate::Script> {
    decode_script(bytes, 0, bytes.len(), size, TABLES, &|n| n == 0, &limits)
}

// ---------------------------------------------------------------------------------------
// Token families

#[test]
fn variables_and_assignment_track_memory_and_file_sizes() {
    // Let(Local(export 0), Instance(export 99 -> two-byte compact)).
    let mut b = vec![0x0F, 0x00, 0x01, 0x01];
    b.extend(compact(100));
    assert_eq!(b.len(), 6);
    let s = decode(&b, 11).unwrap();
    assert_eq!(s.statements.len(), 1);
    assert_eq!(s.memory_size, 11);
    assert_eq!(s.file_span.len(), 6);
    assert_eq!(s.token_count, 3);
    match &s.statements[0].kind {
        TokenKind::Let { lhs, rhs } => {
            assert_eq!(lhs.kind, TokenKind::LocalVariable(ObjectRef::Export(0)));
            assert_eq!(rhs.kind, TokenKind::InstanceVariable(ObjectRef::Export(99)));
            assert_eq!(rhs.offset, 6);
            assert_eq!(rhs.file_offset, 3);
        }
        k => panic!("{k:?}"),
    }
}

#[test]
fn constants() {
    let mut b = Vec::new();
    let mut mem = 0u32;
    // Return(IntConst 7)
    b.extend([0x04, 0x1D, 7, 0, 0, 0]);
    mem += 6;
    // Return(FloatConst 1.5)
    b.extend([0x04, 0x1E]);
    b.extend(1.5f32.to_le_bytes());
    mem += 6;
    // Return(StringConst "hi")
    b.extend([0x04, 0x1F, b'h', b'i', 0]);
    mem += 5;
    // Return(NameConst 3)
    b.extend([0x04, 0x21, 3]);
    mem += 6;
    // Return(RotationConst), Return(VectorConst)
    b.extend([0x04, 0x22]);
    b.extend([0u8; 12]);
    b.extend([0x04, 0x23]);
    b.extend([0u8; 12]);
    mem += 28;
    // Byte/IntConstByte/zero/one/true/false/None/self/unicode
    b.extend([0x04, 0x24, 9, 0x04, 0x2C, 200, 0x04, 0x25, 0x04, 0x26]);
    mem += 10;
    b.extend([0x04, 0x27, 0x04, 0x28, 0x04, 0x2A, 0x04, 0x17]);
    mem += 8;
    b.extend([0x04, 0x34, b'A', 0, 0, 0]);
    mem += 6;
    // ObjectConst(import 0)
    b.extend([0x04, 0x20]);
    b.extend(compact(-1));
    mem += 6;
    let s = decode(&b, mem).unwrap();
    let kinds: Vec<&TokenKind> = s
        .statements
        .iter()
        .map(|t| match &t.kind {
            TokenKind::Return(e) => &e.kind,
            k => k,
        })
        .collect();
    assert_eq!(kinds[0], &TokenKind::IntConst(7));
    assert_eq!(kinds[1], &TokenKind::FloatConst(1.5));
    assert_eq!(kinds[2], &TokenKind::StringConst(b"hi".to_vec()));
    assert_eq!(kinds[3], &TokenKind::NameConst(3));
    assert_eq!(kinds[4], &TokenKind::RotationConst([0, 0, 0]));
    assert_eq!(kinds[6], &TokenKind::ByteConst(9));
    assert_eq!(kinds[7], &TokenKind::IntConstByte(200));
    assert_eq!(kinds[13], &TokenKind::SelfRef);
    assert_eq!(
        kinds[14],
        &TokenKind::UnicodeStringConst(vec![u16::from(b'A')])
    );
    assert_eq!(kinds[15], &TokenKind::ObjectConst(ObjectRef::Import(0)));
}

#[test]
fn control_flow_tokens() {
    let mut b = Vec::new();
    // 0000 Switch[4](Local 0)          1+1+5 = 7
    b.extend([0x05, 4, 0x00, 0x01]);
    // 0007 Case 0x0010 (IntZero)       1+2+1 = 4
    b.extend([0x0A, 0x10, 0x00, 0x25]);
    // 000B Jump 0x0020                 3
    b.extend([0x06, 0x20, 0x00]);
    // 000E Case default                3
    b.extend([0x0A, 0xFF, 0xFF]);
    // 0011 JumpIfNot 0x0000 (True)     4
    b.extend([0x07, 0x00, 0x00, 0x27]);
    // 0015 Assert line 5 (False)       4
    b.extend([0x09, 5, 0, 0x28]);
    // 0019 Iterator(native 0x70 (EndParms)) end 0x0030: 1 + (1+1) + 2 = 5
    b.extend([0x2F, 0x70, 0x16, 0x30, 0x00]);
    // 001E IteratorNext, IteratorPop   2
    b.extend([0x31, 0x30]);
    // 0020 Skip 3 (IntOne)             4
    b.extend([0x18, 3, 0, 0x26]);
    // 0024 Conditional(True, 1, IntOne, 1, IntZero) 1+1+2+1+2+1 = 8
    b.extend([0x48, 0x27, 1, 0, 0x26, 1, 0, 0x25]);
    // 002C Stop, Nothing, GotoLabel(NameConst 1)  1+1+1+5 = 8
    b.extend([0x08, 0x0B, 0x0D, 0x21, 1]);
    // 0034 LabelTable [name 2 @0x2C, None]   1 + 8 + 8 = 17
    b.extend([0x0C, 2, 0x2C, 0, 0, 0, 0, 0, 0, 0, 0]);
    let size = 0x34 + 17;
    let s = decode(&b, size).unwrap();
    let ops: Vec<u8> = s.statements.iter().map(|t| t.opcode).collect();
    assert_eq!(
        ops,
        [
            0x05, 0x0A, 0x06, 0x0A, 0x07, 0x09, 0x2F, 0x31, 0x30, 0x18, 0x48, 0x08, 0x0B, 0x0D,
            0x0C
        ]
    );
    let offsets: Vec<u32> = s.statements.iter().map(|t| t.offset).collect();
    assert_eq!(offsets[..5], [0x00, 0x07, 0x0B, 0x0E, 0x11]);
    assert_eq!(s.statements[14].offset, 0x34);
    let labels = s.labels();
    assert_eq!(labels.len(), 1);
    assert_eq!((labels[0].name, labels[0].offset), (2, 0x2C));
    assert!(matches!(
        s.statements[3].kind,
        TokenKind::Case {
            target: 0xFFFF,
            value: None
        }
    ));
    assert!(matches!(
        s.statements[6].kind,
        TokenKind::Iterator { end: 0x30, .. }
    ));
}

#[test]
fn call_tokens_and_native_indices() {
    let mut b = Vec::new();
    let mut mem = 0;
    // Virtual call name 4 with (IntOne, Nothing) args: 1 + 4 + 1 + 1 + 1 = 8
    b.extend([0x1B, 4, 0x26, 0x0B, 0x16]);
    mem += 8;
    // Final call export 5: 1 + 4 + 1 = 6
    b.extend([0x1C, 6, 0x16]);
    mem += 6;
    // Global call name 2: 6
    b.extend([0x38, 2, 0x16]);
    mem += 6;
    // Native 0x81 (two args): 1 + 1 + 1 + 1 = 4
    b.extend([0x81, 0x25, 0x26, 0x16]);
    mem += 4;
    // Extended native 0x61 0x29 -> 0x129: 2 + 1 = 3
    b.extend([0x61, 0x29, 0x16]);
    mem += 3;
    // Delegate call: property export 1, name 3: 1 + 4 + 4 + 1 = 10
    b.extend([0x43, 2, 3, 0x16]);
    mem += 10;
    // Native 0x70 followed by debug info version 100: 2 + 1 + 12 + 1 = 16
    b.extend([0x70, 0x16, 0x42]);
    b.extend(100i32.to_le_bytes());
    b.extend(12i32.to_le_bytes());
    b.extend(0i32.to_le_bytes());
    b.push(1);
    mem += 2 + 14;
    let s = decode(&b, mem).unwrap();
    let mut natives = Vec::new();
    s.walk(&mut |t| {
        if let TokenKind::NativeCall { index, .. } = t.kind {
            natives.push(index);
        }
    });
    assert_eq!(natives, [0x81, 0x129, 0x70]);
    match &s.statements[0].kind {
        TokenKind::VirtualFunction { name, call } => {
            assert_eq!(*name, 4);
            assert_eq!(call.args.len(), 2);
            assert_eq!(call.end_offset, 7);
        }
        k => panic!("{k:?}"),
    }
    match &s.statements[6].kind {
        TokenKind::NativeCall { call, .. } => {
            let d = call.debug_info.as_ref().expect("debug info consumed");
            assert!(matches!(
                d.kind,
                TokenKind::DebugInfo {
                    version: 100,
                    line: 12,
                    ..
                }
            ));
        }
        k => panic!("{k:?}"),
    }
    assert_eq!(s.statements.len(), 7);
}

#[test]
fn context_struct_array_cast_and_delegate_tokens() {
    let mut b = Vec::new();
    let mut mem = 0;
    // Context(Self, skip 5, size 4, Instance 0): 1 + 1 + 2 + 1 + 5 = 10
    b.extend([0x19, 0x17, 5, 0, 4, 0x01, 0x01]);
    mem += 10;
    // ClassContext(ObjectConst 0, 0, 0, Default 1): 1 + 5 + 3 + 5 = 14
    b.extend([0x12, 0x20, 0x01, 0, 0, 0, 0x02, 0x02]);
    mem += 14;
    // StructMember(prop 2, Local 3): 1 + 4 + 5 = 10
    b.extend([0x36, 0x03, 0x00, 0x04]);
    mem += 10;
    // ArrayElement(IntZero, Local 0), DynArrayElement(IntOne, Local 0): 7 each
    b.extend([0x1A, 0x25, 0x00, 0x01, 0x10, 0x26, 0x00, 0x01]);
    mem += 14;
    // DynArrayLength(Local 0): 6; Insert/Remove(Local 0, IntZero, IntOne): 8 each
    b.extend([0x37, 0x00, 0x01]);
    b.extend([0x40, 0x00, 0x01, 0x25, 0x26, 0x41, 0x00, 0x01, 0x25, 0x26]);
    mem += 6 + 16;
    // DynamicCast(class 1, Self) 6, MetaCast(class 1, Self) 6, PrimitiveCast(0x39, IntOne) 3
    b.extend([0x2E, 0x02, 0x17, 0x13, 0x02, 0x17, 0x39, 0x39, 0x26]);
    mem += 15;
    // New(None, None, IntZero, ObjectConst 0): 1 + 1 + 1 + 1 + 5 = 9
    b.extend([0x11, 0x2A, 0x2A, 0x25, 0x20, 0x01]);
    mem += 9;
    // StructCmpEq(struct 0, Self, Self): 1 + 4 + 1 + 1 = 7
    b.extend([0x32, 0x01, 0x17, 0x17]);
    mem += 7;
    // DelegateCmpEq(Self, Self, EndParms) 4; DelegateProperty(name 1) 5; LetDelegate 3; EmptyDelegate 1
    b.extend([0x3B, 0x17, 0x17, 0x16, 0x44, 1, 0x45, 0x17, 0x3F]);
    mem += 4 + 5 + 3;
    // EatString(StringConst ""), BoolVariable(Instance 0), LetBool(Local, True)
    b.extend([0x0E, 0x1F, 0, 0x2D, 0x01, 0x01, 0x14, 0x00, 0x01, 0x27]);
    mem += 3 + 6 + 7;
    let s = decode(&b, mem).unwrap();
    assert_eq!(s.statements.len(), 19);
    assert!(matches!(&s.statements[0].kind, TokenKind::Context(c) if c.skip == 5 && c.size == 4));
    assert!(matches!(
        &s.statements[10].kind,
        TokenKind::PrimitiveCast { cast: 0x39, .. }
    ));
}

// ---------------------------------------------------------------------------------------
// Errors and limits

#[test]
fn unknown_tokens_fail_with_offsets() {
    for op in [0x03u8, 0x15, 0x35, 0x3A, 0x46, 0x47, 0x49, 0x5F] {
        // Two valid statements, then the unknown opcode at file offset 2 / code offset 2.
        let b = [0x0B, 0x0B, op, 0, 0, 0];
        let e = decode(&b, 6).unwrap_err();
        assert_eq!(
            e.kind,
            ScriptErrorKind::UnknownToken { opcode: op },
            "{op:#x}"
        );
        assert_eq!(e.offset, Some(2));
        assert_eq!(e.script_offset, Some(2));
        assert!(e.to_string().contains("unknown bytecode token"));
    }
    // Nested: Let(Local 0, <0x15>) reports the inner offsets.
    let e = decode(&[0x0F, 0x00, 0x01, 0x15], 20).unwrap_err();
    assert_eq!(e.kind, ScriptErrorKind::UnknownToken { opcode: 0x15 });
    assert_eq!((e.offset, e.script_offset), (Some(3), Some(6)));
}

#[test]
fn truncation_and_size_mismatch_fail() {
    // IntConst missing bytes.
    let e = decode(&[0x1D, 1, 2], 5).unwrap_err();
    assert!(matches!(e.kind, ScriptErrorKind::Package(_)), "{e:?}");
    assert_eq!(e.offset, Some(1));
    // Declared size larger than the stream: end of data.
    let e = decode(&[0x0B], 2).unwrap_err();
    assert!(matches!(e.kind, ScriptErrorKind::Package(_)));
    // Declared size smaller than the tokens: overrun (Let needs 11 memory bytes).
    let e = decode(&[0x0F, 0x00, 0x01, 0x01, 0x01], 3).unwrap_err();
    assert!(
        matches!(e.kind, ScriptErrorKind::ScriptOverrun { .. }),
        "{e:?}"
    );
    // Call without terminator runs past the script.
    let e = decode(&[0x1B, 1, 0x0B, 0x0B], 7).unwrap_err();
    assert!(matches!(
        e.kind,
        ScriptErrorKind::ScriptOverrun { .. } | ScriptErrorKind::Package(_)
    ));
    // Unterminated string constant.
    let e = decode(&[0x1F, b'a', b'b'], 10).unwrap_err();
    assert!(matches!(e.kind, ScriptErrorKind::Package(_)));
}

#[test]
fn references_are_checked() {
    // Export 201 is outside 200 exports.
    let mut b = vec![0x00];
    b.extend(compact(201));
    let e = decode(&b, 5).unwrap_err();
    assert_eq!(e.kind, ScriptErrorKind::BadObjectRef { raw: 201 });
    // Import -3 is outside 2 imports.
    let e = decode(&[0x00, 0x83], 5).unwrap_err();
    assert_eq!(e.kind, ScriptErrorKind::BadObjectRef { raw: -3 });
    // Name 8 is outside 8 names.
    let e = decode(&[0x21, 8], 5).unwrap_err();
    assert_eq!(e.kind, ScriptErrorKind::BadNameIndex { index: 8 });
}

#[test]
fn limits_are_enforced() {
    let deep: Vec<u8> = std::iter::repeat_n(0x04u8, 10).chain([0x0B]).collect();
    let mut l = ScriptLimits {
        max_depth: 5,
        ..ScriptLimits::default()
    };
    let e = decode_with(&deep, 11, l).unwrap_err();
    assert_eq!(e.kind, ScriptErrorKind::DepthExceeded { max: 5 });
    l = ScriptLimits {
        max_tokens: 3,
        ..ScriptLimits::default()
    };
    let e = decode_with(&[0x0B; 4], 4, l).unwrap_err();
    assert_eq!(e.kind, ScriptErrorKind::TooManyTokens { max: 3 });
    l = ScriptLimits {
        max_script_size: 10,
        ..ScriptLimits::default()
    };
    let e = decode_with(&[0x0B; 4], 11, l).unwrap_err();
    assert!(matches!(e.kind, ScriptErrorKind::ScriptSizeInvalid { .. }));
    l = ScriptLimits {
        max_labels: 1,
        ..ScriptLimits::default()
    };
    let labels = [0x0C, 1, 0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let e = decode_with(&labels, 25, l).unwrap_err();
    assert!(matches!(
        e.kind,
        ScriptErrorKind::CountTooLarge {
            what: "label table",
            ..
        }
    ));
    l = ScriptLimits {
        max_string_bytes: 2,
        ..ScriptLimits::default()
    };
    let e = decode_with(&[0x1F, b'a', b'b', b'c', 0], 5, l).unwrap_err();
    assert!(matches!(
        e.kind,
        ScriptErrorKind::CountTooLarge {
            what: "string constant",
            ..
        }
    ));
}

#[test]
fn empty_script_is_valid() {
    let s = decode(&[], 0).unwrap();
    assert!(s.statements.is_empty());
    assert_eq!(s.file_span.len(), 0);
}

// ---------------------------------------------------------------------------------------
// Synthetic packages

pub(crate) struct Exp {
    pub(crate) class: i32,
    pub(crate) outer: i32,
    pub(crate) name: i32,
    pub(crate) flags: u32,
    pub(crate) payload: Vec<u8>,
}

/// Builds a version-100/licensee-58 package: header, payloads, then names/imports/exports.
pub(crate) fn build_package(
    names: &[&str],
    imports: &[(i32, i32, i32, i32)],
    exports: &[Exp],
) -> Vec<u8> {
    let mut body: Vec<u8> = Vec::new();
    let header_len = 64usize;
    let mut offsets = Vec::new();
    for e in exports {
        offsets.push(header_len + body.len());
        body.extend(&e.payload);
    }
    let names_off = header_len + body.len();
    let mut tables = Vec::new();
    for n in names {
        tables.extend(compact(n.len() as i32 + 1));
        tables.extend(n.as_bytes());
        tables.push(0);
        tables.extend(0u32.to_le_bytes());
    }
    let imports_off = names_off + tables.len();
    for &(cp, cn, outer, name) in imports {
        tables.extend(compact(cp));
        tables.extend(compact(cn));
        tables.extend(outer.to_le_bytes());
        tables.extend(compact(name));
    }
    let exports_off = names_off + tables.len();
    for (e, off) in exports.iter().zip(&offsets) {
        tables.extend(compact(e.class));
        tables.extend(compact(0));
        tables.extend(e.outer.to_le_bytes());
        tables.extend(compact(e.name));
        tables.extend(e.flags.to_le_bytes());
        tables.extend(compact(e.payload.len() as i32));
        if !e.payload.is_empty() {
            tables.extend(compact(*off as i32));
        }
    }
    let mut out = Vec::new();
    out.extend(0x9E2A_83C1u32.to_le_bytes());
    out.extend(100u16.to_le_bytes());
    out.extend(58u16.to_le_bytes());
    out.extend(1u32.to_le_bytes());
    for v in [
        names.len() as i32,
        names_off as i32,
        exports.len() as i32,
        exports_off as i32,
        imports.len() as i32,
        imports_off as i32,
    ] {
        out.extend(v.to_le_bytes());
    }
    out.extend([0u8; 16]);
    out.extend(1i32.to_le_bytes());
    out.extend((exports.len() as i32).to_le_bytes());
    out.extend((names.len() as i32).to_le_bytes());
    assert_eq!(out.len(), header_len);
    out.extend(body);
    out.extend(tables);
    out
}

// Names.
const N_NONE: i32 = 0;
const N_CORE: i32 = 1;
const N_CLASS: i32 = 2;
const N_FUNCTION: i32 = 3;
const N_STATE: i32 = 4;
const N_MYCLASS: i32 = 6;
const N_TICK: i32 = 7;
const N_IDLE: i32 = 8;
const N_COUNT: i32 = 9;
const N_INTPROPERTY: i32 = 10;
const N_PACKAGE: i32 = 11;
const N_SYSTEM: i32 = 12;
const N_BEGIN: i32 = 13;

const NAMES: &[&str] = &[
    "None",
    "Core",
    "Class",
    "Function",
    "State",
    "IntProp",
    "MyClass",
    "Tick",
    "Idle",
    "Count",
    "IntProperty",
    "Package",
    "System",
    "Begin",
];

fn imports() -> Vec<(i32, i32, i32, i32)> {
    vec![
        // -1 Core (package)
        (N_CORE, N_PACKAGE, 0, N_CORE),
        // -2 Core.Function, -3 Core.State, -4 Core.IntProperty (classes)
        (N_CORE, N_CLASS, -1, N_FUNCTION),
        (N_CORE, N_CLASS, -1, N_STATE),
        (N_CORE, N_CLASS, -1, N_INTPROPERTY),
    ]
}

fn struct_part(next: i32, children: i32, friendly: i32, script: &[u8], mem: u32) -> Vec<u8> {
    let mut p = Vec::new();
    p.extend(compact(0)); // SuperField
    p.extend(compact(next)); // Next
    p.extend(compact(0)); // ScriptText
    p.extend(compact(children));
    p.extend(compact(friendly));
    p.extend(10i32.to_le_bytes()); // Line
    p.extend(20i32.to_le_bytes()); // TextPos
    p.extend((mem as i32).to_le_bytes());
    p.extend(script);
    p
}

/// Exports: 0 MyClass (class), 1 MyClass.Tick (function), 2 MyClass.Idle (state),
/// 3 MyClass.Count (int property), 4 MyClass.Tick.Count (int parameter of Tick).
fn sample_package(function_tail: &[u8], class_tail_extra: &[u8]) -> Vec<u8> {
    // Function Tick: script `Count = 1; return;` -> Let(Instance export3, IntOne), Return(Nothing)
    let fscript = [0x0F, 0x01, 0x04, 0x26, 0x04, 0x0B];
    let mut func = compact(N_NONE); // empty tagged-property block
    func.extend(struct_part(3, 5, N_TICK, &fscript, 1 + 5 + 1 + 2));
    func.extend(function_tail);
    // State Idle: labels at 0: `Begin: stop; labeltable`
    let sscript = [0x08, 0x0C, N_BEGIN as u8, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut state = compact(N_NONE);
    state.extend(struct_part(0, 0, N_IDLE, &sscript, 1 + 17));
    state.extend(0xFFu64.to_le_bytes()); // probe
    state.extend(u64::MAX.to_le_bytes()); // ignore
    state.extend(1u16.to_le_bytes()); // label table offset
    state.extend(2u16.to_le_bytes()); // auto
    // Int property Count (net -> rep offset)
    let mut prop = compact(N_NONE);
    prop.extend(compact(0));
    prop.extend(compact(3)); // next = Idle (export 2)
    prop.extend(1i16.to_le_bytes());
    prop.extend(0x0000_0021u32.to_le_bytes()); // edit + net
    prop.extend(compact(N_NONE));
    prop.extend(7u16.to_le_bytes());
    // Parameter of Tick (export 4): int parm
    let mut parm = compact(N_NONE);
    parm.extend(compact(0));
    parm.extend(compact(0));
    parm.extend(1i16.to_le_bytes());
    parm.extend(0x0000_0080u32.to_le_bytes());
    parm.extend(compact(N_NONE));
    // Class MyClass: no tagged block first.
    let mut class = struct_part(0, 2, N_MYCLASS, &[], 0); // children = Tick
    class.extend(0u64.to_le_bytes());
    class.extend(u64::MAX.to_le_bytes());
    class.extend(0xFFFFu16.to_le_bytes());
    class.extend(0u16.to_le_bytes());
    class.extend(0x0012u16.to_le_bytes()); // class flags
    class.extend([0u8; 16]); // guid
    class.extend(compact(1)); // one dependency
    class.extend(compact(1)); // on itself
    class.extend(1u32.to_le_bytes());
    class.extend(0xDEAD_BEEFu32.to_le_bytes());
    class.extend(compact(1)); // package imports
    class.extend(compact(N_CORE));
    class.extend(compact(0)); // within
    class.extend(compact(N_SYSTEM)); // config
    class.extend(compact(0)); // hide categories
    // defaults: Count=5 (IntProperty tag: name, info 0x22 = int size code 2 (4 bytes))
    class.extend(compact(N_COUNT));
    class.push(0x22);
    class.extend(5i32.to_le_bytes());
    class.extend(compact(N_NONE));
    class.extend(class_tail_extra);
    let exports = vec![
        Exp {
            class: 0,
            outer: 0,
            name: N_MYCLASS,
            flags: 0,
            payload: class,
        },
        Exp {
            class: -2,
            outer: 1,
            name: N_TICK,
            flags: 0,
            payload: func,
        },
        Exp {
            class: -3,
            outer: 1,
            name: N_IDLE,
            flags: 0,
            payload: state,
        },
        Exp {
            class: -4,
            outer: 1,
            name: N_COUNT,
            flags: 0,
            payload: prop,
        },
        Exp {
            class: -4,
            outer: 2,
            name: N_COUNT,
            flags: 0,
            payload: parm,
        },
    ];
    build_package(NAMES, &imports(), &exports)
}

fn function_tail(native: u16, flags: u32, rep: Option<u16>) -> Vec<u8> {
    let mut t = native.to_le_bytes().to_vec();
    t.push(0);
    t.extend(&flags.to_le_bytes()[..3]);
    if let Some(r) = rep {
        t.extend(r.to_le_bytes());
    }
    t
}

#[test]
fn synthetic_struct_function_state_class_property_payloads() {
    let flags = function_flags::FINAL | function_flags::NATIVE | function_flags::DEFINED;
    let data = sample_package(&function_tail(300, flags, None), &[]);
    let p = Package::parse(&data, &Limits::default()).unwrap();
    let l = ScriptLimits::default();
    let pl = Limits::default();
    match read_script_object(&p, &data, 1, &l, &pl).unwrap() {
        ScriptObject::Function(f) => {
            assert_eq!(f.native_index, 300);
            assert_eq!(f.flags, flags);
            assert!(f.is_native());
            assert_eq!(f.rep_offset, None);
            assert_eq!(f.header.line, 10);
            assert_eq!(f.header.children, ObjectRef::Export(4));
            assert_eq!(f.header.script.memory_size, 9);
            assert_eq!(f.header.script.file_span.len(), 6);
        }
        o => panic!("{o:?}"),
    }
    match read_script_object(&p, &data, 2, &l, &pl).unwrap() {
        ScriptObject::State(s) => {
            assert_eq!(s.state.label_table_offset, 1);
            assert_eq!(s.state.state_flags, 2);
            assert_eq!(s.header.script.labels()[0].name, N_BEGIN as u32);
        }
        o => panic!("{o:?}"),
    }
    match read_script_object(&p, &data, 3, &l, &pl).unwrap() {
        ScriptObject::Property(q) => {
            assert_eq!(q.kind, PropertyKind::Int);
            assert_eq!(q.rep_offset, Some(7));
            assert_eq!(q.array_dim, 1);
        }
        o => panic!("{o:?}"),
    }
    match read_script_object(&p, &data, 0, &l, &pl).unwrap() {
        ScriptObject::Class(c) => {
            assert_eq!(c.class_flags, 0x12);
            assert_eq!(c.dependencies.len(), 1);
            assert_eq!(c.dependencies[0].script_text_crc, 0xDEAD_BEEF);
            assert_eq!(c.package_imports, vec![N_CORE as u32]);
            assert_eq!(c.config_name, N_SYSTEM as u32);
            assert_eq!(c.defaults.properties.len(), 1);
            assert_eq!(
                c.defaults.span.end,
                p.exports()[0].serial_span().unwrap().end
            );
        }
        o => panic!("{o:?}"),
    }
    let d = class_defaults(&p, &data, 0, &l, &pl).unwrap();
    assert_eq!(p.property_name(&d.properties[0]), "Count");
    // class_defaults refuses non-class exports.
    assert!(class_defaults(&p, &data, 1, &l, &pl).is_err());
}

#[test]
fn net_function_reads_replication_offset() {
    let flags = function_flags::NET | function_flags::DEFINED;
    let data = sample_package(&function_tail(0, flags, Some(0x1234)), &[]);
    let p = Package::parse(&data, &Limits::default()).unwrap();
    match read_script_object(&p, &data, 1, &ScriptLimits::default(), &Limits::default()).unwrap() {
        ScriptObject::Function(f) => assert_eq!(f.rep_offset, Some(0x1234)),
        o => panic!("{o:?}"),
    }
    // Same flags without the offset bytes: truncated.
    let data = sample_package(&function_tail(0, flags, None), &[]);
    let p = Package::parse(&data, &Limits::default()).unwrap();
    let e =
        read_script_object(&p, &data, 1, &ScriptLimits::default(), &Limits::default()).unwrap_err();
    assert_eq!(e.export, Some(1));
    assert_eq!(e.field, Some("UFunction.RepOffset"));
}

#[test]
fn trailing_bytes_are_reported() {
    let mut tail = function_tail(0, function_flags::DEFINED, None);
    tail.push(0xAA);
    let data = sample_package(&tail, &[0x55, 0x66]);
    let p = Package::parse(&data, &Limits::default()).unwrap();
    let e =
        read_script_object(&p, &data, 1, &ScriptLimits::default(), &Limits::default()).unwrap_err();
    assert!(
        matches!(e.kind, ScriptErrorKind::TrailingBytes { .. }),
        "{e:?}"
    );
    // Class defaults followed by junk: the block ends early (or fails) -> error, not success.
    let e =
        read_script_object(&p, &data, 0, &ScriptLimits::default(), &Limits::default()).unwrap_err();
    assert_eq!(e.export, Some(0));
}

#[test]
fn script_set_catalogs_natives_and_resolves_fields() {
    let flags = function_flags::FINAL | function_flags::NATIVE;
    let data = sample_package(&function_tail(300, flags, None), &[]);
    let pkg =
        ScriptPackage::load("Sample", data, &ScriptLimits::default(), &Limits::default()).unwrap();
    assert!(pkg.errors.is_empty(), "{:?}", pkg.errors);
    let mut set = ScriptSet::new();
    let i = set.add(pkg);
    assert_eq!(set.native_functions(300).len(), 1);
    let cat = crate::natives::native_catalog(&set);
    assert_eq!(cat.len(), 1);
    assert_eq!(cat[0].path(), "Sample.MyClass.Tick");
    assert_eq!(cat[0].params.len(), 1);
    assert_eq!(cat[0].params[0].type_name, "int");
    let class = crate::GlobalRef {
        package: i,
        export: 0,
    };
    let tick = set.find_field(class, "tick").unwrap();
    assert_eq!(tick.export, 1);
    let idle = set.find_field(class, "Idle").unwrap();
    assert_eq!(idle.export, 2);
    let mut stats = crate::natives::CallStats::default();
    stats.add_package(&set, i);
    assert_eq!(stats.opcodes.get(&0x0F), Some(&1));
    // Disassembly mentions the instance variable.
    let d = crate::disasm::Disasm::new(&set, i);
    let f = set.packages[i].function(1).unwrap();
    assert!(d.listing(&f.header.script).contains("self.Count = 1"));
}
