//! UnrealScript (UE2, package version 100) bytecode token decoder.
//!
//! On disk, the script of a `UStruct` is serialized token by token: object references and
//! names are compact indices, while the stored script size counts **in-memory** bytes, where
//! every object reference and name takes 4 bytes. The decoder therefore tracks two positions:
//! the memory (code) offset, which jump/label targets refer to, and the absolute file offset.
//! Decoding stops when the memory offset reaches the declared size; it must land exactly on it.
//!
//! The accepted opcode set is the UE2 table for package versions 95..177 as implemented by
//! UELib (`src/Branch/DefaultEngineBranch.cs` `BuildTokenMap` and `src/Core/Tokens/*.cs` at
//! `EliotVU/Unreal-Library@3207a17e9b294be3d1bf26b18e07ccff7e1d4b0c`, MIT). This is an
//! independent Rust implementation of that layout (no code copied), verified against the GOG
//! corpus. Opcodes UELib marks as version-dependent or engine-specific and that never occur in
//! the corpus (`0x03`, `0x15` line number, `0x35` range constant, `0x3A`, `0x46`, `0x47`
//! end-of-script, `0x49..=0x5F`) are rejected as [`ScriptErrorKind::UnknownToken`].

use xiii_package::{ObjectRef, Span};

use crate::error::{Result, ScriptError, ScriptErrorKind};
use crate::reader::{Reader, Tables};

/// In-memory size of an object reference in version-100 bytecode.
pub const OBJECT_MEMORY_SIZE: u32 = 4;
/// In-memory size of a name in version-100 bytecode.
pub const NAME_MEMORY_SIZE: u32 = 4;
/// First extended-native opcode (`0x60..=0x6F` + one extension byte).
pub const EX_EXTENDED_NATIVE: u8 = 0x60;
/// First single-byte native opcode (`0x70..=0xFF` call native index == opcode).
pub const EX_FIRST_NATIVE: u8 = 0x70;
/// Debug-info version that marks an optional trailing debug token after a call.
pub const DEBUG_INFO_VERSION: i32 = 100;

/// Decoder limits; every untrusted size is checked against one of these.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScriptLimits {
    /// Maximum declared in-memory script size.
    pub max_script_size: u32,
    /// Maximum expression nesting.
    pub max_depth: u32,
    /// Maximum tokens per script.
    pub max_tokens: u32,
    /// Maximum label-table entries.
    pub max_labels: u32,
    /// Maximum bytes in one string constant.
    pub max_string_bytes: u32,
    /// Maximum elements in reflected arrays (dependencies, imports, enum names, ...).
    pub max_array: u32,
}

impl Default for ScriptLimits {
    fn default() -> Self {
        Self {
            max_script_size: 1 << 20,
            max_depth: 512,
            max_tokens: 1 << 20,
            max_labels: 4096,
            max_string_bytes: 1 << 16,
            max_array: 1 << 16,
        }
    }
}

/// One decoded token with its operands. Child expressions are nested.
#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    /// Memory (code) offset of the opcode; jump and label targets use this space.
    pub offset: u32,
    /// Absolute file offset of the opcode byte.
    pub file_offset: usize,
    /// Memory size of the token including its children.
    pub memory_size: u32,
    /// Opcode byte (for native calls: the first byte, `0x60..=0xFF`).
    pub opcode: u8,
    /// Decoded operands.
    pub kind: TokenKind,
}

/// A function call's argument list (terminated on disk by `EndFunctionParms`).
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    /// Argument expressions (skipped optional arguments appear as `Nothing`).
    pub args: Vec<Token>,
    /// Memory offset of the `EndFunctionParms` terminator.
    pub end_offset: u32,
    /// Optional trailing debug-info token (version 100), as in the engine serializer.
    pub debug_info: Option<Box<Token>>,
}

/// `Context` / `ClassContext` operands.
#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    /// Object (or class) expression.
    pub object: Box<Token>,
    /// Memory bytes to skip when the object is `None`.
    pub skip: u16,
    /// Size of the result value (0 for none/strings).
    pub size: u8,
    /// Member expression evaluated in the object's context.
    pub member: Box<Token>,
}

/// One label-table entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label {
    /// Name index.
    pub name: u32,
    /// Memory offset of the label in the state code.
    pub offset: u32,
}

/// Token operands, by opcode. Names are name-table indices.
#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    /// `0x00` local variable or parameter.
    LocalVariable(ObjectRef),
    /// `0x01` instance variable of `self`.
    InstanceVariable(ObjectRef),
    /// `0x02` default (class-default) variable.
    DefaultVariable(ObjectRef),
    /// `0x04` return with value expression (`Nothing` for none).
    Return(Box<Token>),
    /// `0x05` switch on an expression; `size` is the value size.
    Switch {
        /// Value size in bytes (0 for strings).
        size: u8,
        /// Switched expression.
        expr: Box<Token>,
    },
    /// `0x06` unconditional jump.
    Jump {
        /// Target memory offset.
        target: u16,
    },
    /// `0x07` jump when the condition is false.
    JumpIfNot {
        /// Target memory offset.
        target: u16,
        /// Boolean condition.
        cond: Box<Token>,
    },
    /// `0x08` stop state code.
    Stop,
    /// `0x09` assertion.
    Assert {
        /// Source line.
        line: u16,
        /// Asserted condition.
        cond: Box<Token>,
    },
    /// `0x0A` switch case; `target == 0xFFFF` marks `default:`.
    Case {
        /// Memory offset of the next case.
        target: u16,
        /// Case value (absent for `default:`).
        value: Option<Box<Token>>,
    },
    /// `0x0B` no operation / omitted optional argument.
    Nothing,
    /// `0x0C` state label table (the terminating `None` entry is not stored).
    LabelTable {
        /// Entries in file order.
        labels: Vec<Label>,
        /// Name index of the terminator entry.
        terminator: u32,
    },
    /// `0x0D` goto label (name expression).
    GotoLabel(Box<Token>),
    /// `0x0E` evaluate a string expression and discard it.
    EatString(Box<Token>),
    /// `0x0F` assignment.
    Let {
        /// Destination.
        lhs: Box<Token>,
        /// Value.
        rhs: Box<Token>,
    },
    /// `0x10` dynamic-array element.
    DynArrayElement {
        /// Index expression.
        index: Box<Token>,
        /// Array expression.
        array: Box<Token>,
    },
    /// `0x11` `new(outer, name, flags) class`.
    New {
        /// Outer expression.
        outer: Box<Token>,
        /// Name expression.
        name: Box<Token>,
        /// Flags expression.
        flags: Box<Token>,
        /// Class expression.
        class: Box<Token>,
    },
    /// `0x12` class context (static/default access through a class).
    ClassContext(Context),
    /// `0x13` metaclass cast.
    MetaCast {
        /// Target metaclass.
        class: ObjectRef,
        /// Cast expression.
        expr: Box<Token>,
    },
    /// `0x14` boolean assignment.
    LetBool {
        /// Destination.
        lhs: Box<Token>,
        /// Value.
        rhs: Box<Token>,
    },
    /// `0x16` end of call arguments (only seen standalone when malformed).
    EndFunctionParms,
    /// `0x17` `self`.
    SelfRef,
    /// `0x18` skip offset for short-circuit operators.
    Skip {
        /// Memory bytes to skip.
        skip: u16,
        /// Wrapped expression.
        expr: Box<Token>,
    },
    /// `0x19` object context (`obj.member`).
    Context(Context),
    /// `0x1A` static-array element.
    ArrayElement {
        /// Index expression.
        index: Box<Token>,
        /// Array expression.
        array: Box<Token>,
    },
    /// `0x1B` virtual call by name.
    VirtualFunction {
        /// Function name.
        name: u32,
        /// Arguments.
        call: Call,
    },
    /// `0x1C` final (non-virtual) call by object reference.
    FinalFunction {
        /// Function object.
        function: ObjectRef,
        /// Arguments.
        call: Call,
    },
    /// `0x1D` int constant.
    IntConst(i32),
    /// `0x1E` float constant.
    FloatConst(f32),
    /// `0x1F` Latin-1 string constant (NUL-terminated on disk).
    StringConst(Vec<u8>),
    /// `0x20` object constant.
    ObjectConst(ObjectRef),
    /// `0x21` name constant.
    NameConst(u32),
    /// `0x22` rotator constant (pitch, yaw, roll).
    RotationConst([i32; 3]),
    /// `0x23` vector constant.
    VectorConst([f32; 3]),
    /// `0x24` byte constant.
    ByteConst(u8),
    /// `0x25` int 0.
    IntZero,
    /// `0x26` int 1.
    IntOne,
    /// `0x27` true.
    True,
    /// `0x28` false.
    False,
    /// `0x29` native function parameter.
    NativeParm(ObjectRef),
    /// `0x2A` `None`.
    NoObject,
    /// `0x2C` int constant stored in one byte.
    IntConstByte(u8),
    /// `0x2D` bool variable wrapper.
    BoolVariable(Box<Token>),
    /// `0x2E` dynamic class cast.
    DynamicCast {
        /// Target class.
        class: ObjectRef,
        /// Cast expression.
        expr: Box<Token>,
    },
    /// `0x2F` iterator (`foreach`) with loop-end offset.
    Iterator {
        /// Iterator call.
        expr: Box<Token>,
        /// Memory offset after the loop.
        end: u16,
    },
    /// `0x30` leave an iterator.
    IteratorPop,
    /// `0x31` advance an iterator.
    IteratorNext,
    /// `0x32` struct equality.
    StructCmpEq {
        /// Struct type.
        strukt: ObjectRef,
        /// Left operand.
        a: Box<Token>,
        /// Right operand.
        b: Box<Token>,
    },
    /// `0x33` struct inequality.
    StructCmpNe {
        /// Struct type.
        strukt: ObjectRef,
        /// Left operand.
        a: Box<Token>,
        /// Right operand.
        b: Box<Token>,
    },
    /// `0x34` UTF-16 string constant.
    UnicodeStringConst(Vec<u16>),
    /// `0x36` struct member access.
    StructMember {
        /// Member property.
        property: ObjectRef,
        /// Struct expression.
        expr: Box<Token>,
    },
    /// `0x37` dynamic-array length.
    DynArrayLength(Box<Token>),
    /// `0x38` `global.` call by name.
    GlobalFunction {
        /// Function name.
        name: u32,
        /// Arguments.
        call: Call,
    },
    /// `0x39` primitive conversion (`cast` is the UE2 conversion code).
    PrimitiveCast {
        /// Conversion code.
        cast: u8,
        /// Converted expression.
        expr: Box<Token>,
    },
    /// `0x3B..=0x3E` delegate comparisons (`opcode` distinguishes them).
    DelegateCompare {
        /// Left operand.
        a: Box<Token>,
        /// Right operand.
        b: Box<Token>,
        /// Third token (the terminator in UELib's reading).
        end: Box<Token>,
    },
    /// `0x3F` empty delegate.
    EmptyDelegate,
    /// `0x40` dynamic-array insert (array, index, count).
    DynArrayInsert {
        /// Array expression.
        array: Box<Token>,
        /// Index expression.
        index: Box<Token>,
        /// Count expression.
        count: Box<Token>,
    },
    /// `0x41` dynamic-array remove (array, index, count).
    DynArrayRemove {
        /// Array expression.
        array: Box<Token>,
        /// Index expression.
        index: Box<Token>,
        /// Count expression.
        count: Box<Token>,
    },
    /// `0x42` debug information.
    DebugInfo {
        /// Debug-info version (100).
        version: i32,
        /// Source line.
        line: i32,
        /// Source text position.
        text_pos: i32,
        /// Debug opcode.
        op: u8,
    },
    /// `0x43` delegate call.
    DelegateFunction {
        /// Delegate property.
        property: ObjectRef,
        /// Function name.
        name: u32,
        /// Arguments.
        call: Call,
    },
    /// `0x44` delegate property by name.
    DelegateProperty(u32),
    /// `0x45` delegate assignment.
    LetDelegate {
        /// Destination.
        lhs: Box<Token>,
        /// Value.
        rhs: Box<Token>,
    },
    /// `0x48` conditional (`cond ? a : b`).
    Conditional {
        /// Condition.
        cond: Box<Token>,
        /// Memory size of the true branch.
        skip_true: u16,
        /// True branch.
        a: Box<Token>,
        /// Memory size of the false branch.
        skip_false: u16,
        /// False branch.
        b: Box<Token>,
    },
    /// `0x60..=0xFF` call of a native function by index.
    NativeCall {
        /// Native function index (`0x70..=0xFF`, or `(op - 0x60) << 8 | ext`).
        index: u16,
        /// Arguments.
        call: Call,
    },
}

impl Token {
    /// Visits this token and all nested tokens in file order.
    pub fn walk<'t>(&'t self, f: &mut impl FnMut(&'t Token)) {
        f(self);
        for c in self.children() {
            c.walk(f);
        }
    }

    /// Direct child tokens in file order.
    pub fn children(&self) -> Vec<&Token> {
        fn call(c: &Call) -> impl Iterator<Item = &Token> {
            c.args.iter().chain(c.debug_info.as_deref())
        }
        use TokenKind as K;
        match &self.kind {
            K::Return(e)
            | K::GotoLabel(e)
            | K::EatString(e)
            | K::BoolVariable(e)
            | K::DynArrayLength(e) => vec![e],
            K::Switch { expr, .. }
            | K::MetaCast { expr, .. }
            | K::Skip { expr, .. }
            | K::DynamicCast { expr, .. }
            | K::Iterator { expr, .. }
            | K::StructMember { expr, .. }
            | K::PrimitiveCast { expr, .. } => vec![expr],
            K::JumpIfNot { cond, .. } | K::Assert { cond, .. } => vec![cond],
            K::Case { value, .. } => value.iter().map(|b| &**b).collect(),
            K::Let { lhs, rhs } | K::LetBool { lhs, rhs } | K::LetDelegate { lhs, rhs } => {
                vec![lhs, rhs]
            }
            K::DynArrayElement { index, array } | K::ArrayElement { index, array } => {
                vec![index, array]
            }
            K::New {
                outer,
                name,
                flags,
                class,
            } => vec![outer, name, flags, class],
            K::ClassContext(c) | K::Context(c) => vec![&c.object, &c.member],
            K::VirtualFunction { call: c, .. }
            | K::FinalFunction { call: c, .. }
            | K::GlobalFunction { call: c, .. }
            | K::DelegateFunction { call: c, .. }
            | K::NativeCall { call: c, .. } => call(c).collect(),
            K::StructCmpEq { a, b, .. } | K::StructCmpNe { a, b, .. } => vec![a, b],
            K::DelegateCompare { a, b, end } => vec![a, b, end],
            K::DynArrayInsert {
                array,
                index,
                count,
            }
            | K::DynArrayRemove {
                array,
                index,
                count,
            } => vec![array, index, count],
            K::Conditional { cond, a, b, .. } => vec![cond, a, b],
            _ => Vec::new(),
        }
    }

    /// Short mnemonic for the opcode.
    pub fn mnemonic(&self) -> &'static str {
        opcode_name(self.opcode)
    }
}

/// Mnemonic of an opcode (`"Native"` for `0x60..=0xFF`, `"Unknown"` for rejected opcodes).
pub fn opcode_name(op: u8) -> &'static str {
    match op {
        0x00 => "LocalVariable",
        0x01 => "InstanceVariable",
        0x02 => "DefaultVariable",
        0x04 => "Return",
        0x05 => "Switch",
        0x06 => "Jump",
        0x07 => "JumpIfNot",
        0x08 => "Stop",
        0x09 => "Assert",
        0x0A => "Case",
        0x0B => "Nothing",
        0x0C => "LabelTable",
        0x0D => "GotoLabel",
        0x0E => "EatString",
        0x0F => "Let",
        0x10 => "DynArrayElement",
        0x11 => "New",
        0x12 => "ClassContext",
        0x13 => "MetaCast",
        0x14 => "LetBool",
        0x16 => "EndFunctionParms",
        0x17 => "Self",
        0x18 => "Skip",
        0x19 => "Context",
        0x1A => "ArrayElement",
        0x1B => "VirtualFunction",
        0x1C => "FinalFunction",
        0x1D => "IntConst",
        0x1E => "FloatConst",
        0x1F => "StringConst",
        0x20 => "ObjectConst",
        0x21 => "NameConst",
        0x22 => "RotationConst",
        0x23 => "VectorConst",
        0x24 => "ByteConst",
        0x25 => "IntZero",
        0x26 => "IntOne",
        0x27 => "True",
        0x28 => "False",
        0x29 => "NativeParm",
        0x2A => "NoObject",
        0x2C => "IntConstByte",
        0x2D => "BoolVariable",
        0x2E => "DynamicCast",
        0x2F => "Iterator",
        0x30 => "IteratorPop",
        0x31 => "IteratorNext",
        0x32 => "StructCmpEq",
        0x33 => "StructCmpNe",
        0x34 => "UnicodeStringConst",
        0x36 => "StructMember",
        0x37 => "DynArrayLength",
        0x38 => "GlobalFunction",
        0x39 => "PrimitiveCast",
        0x3B => "DelegateCmpEq",
        0x3C => "DelegateCmpNe",
        0x3D => "DelegateFunctionCmpEq",
        0x3E => "DelegateFunctionCmpNe",
        0x3F => "EmptyDelegate",
        0x40 => "DynArrayInsert",
        0x41 => "DynArrayRemove",
        0x42 => "DebugInfo",
        0x43 => "DelegateFunction",
        0x44 => "DelegateProperty",
        0x45 => "LetDelegate",
        0x48 => "Conditional",
        0x60..=0xFF => "Native",
        _ => "Unknown",
    }
}

/// A decoded script (function, state or class code).
#[derive(Debug, Clone, PartialEq)]
pub struct Script {
    /// Top-level tokens (statements) in file order.
    pub statements: Vec<Token>,
    /// Declared in-memory size (reached exactly).
    pub memory_size: u32,
    /// Absolute file range of the serialized tokens.
    pub file_span: Span,
    /// Total tokens including nested ones.
    pub token_count: u32,
}

impl Script {
    /// Visits every token in file order.
    pub fn walk<'t>(&'t self, f: &mut impl FnMut(&'t Token)) {
        for s in &self.statements {
            s.walk(f);
        }
    }

    /// Label table entries (state code), if present.
    pub fn labels(&self) -> Vec<Label> {
        let mut out = Vec::new();
        for s in &self.statements {
            if let TokenKind::LabelTable { labels, .. } = &s.kind {
                out.extend_from_slice(labels);
            }
        }
        out
    }
}

/// Decodes `script_size` in-memory bytes of tokens from `data[start..end]`.
///
/// Returns the script; its `file_span.end` is where the serialized tokens stop (the caller
/// continues reading struct fields from there). `names_none` reports whether a name index's
/// text is `None` (label-table terminator); pass the package's name table lookup.
pub fn decode_script(
    data: &[u8],
    start: usize,
    end: usize,
    script_size: u32,
    tables: Tables,
    is_none_name: &dyn Fn(u32) -> bool,
    limits: &ScriptLimits,
) -> Result<Script> {
    let mut r = Reader::new(data, start, end, tables)?;
    let script = decode_with_reader(&mut r, script_size, is_none_name, limits)?;
    Ok(script)
}

/// Same as [`decode_script`] but continues an existing reader (used by struct readers).
pub fn decode_with_reader(
    r: &mut Reader<'_>,
    script_size: u32,
    is_none_name: &dyn Fn(u32) -> bool,
    limits: &ScriptLimits,
) -> Result<Script> {
    if script_size > limits.max_script_size {
        return Err(ScriptError::at(
            ScriptErrorKind::ScriptSizeInvalid {
                size: script_size as i32,
                max: limits.max_script_size,
            },
            r.pos(),
        ));
    }
    let start = r.pos();
    let mut d = Decoder {
        r,
        mem: 0,
        size: script_size,
        tokens: 0,
        limits,
        is_none_name,
    };
    let mut statements = Vec::new();
    while d.mem < d.size {
        let t = d.expr(0)?;
        statements.push(t);
    }
    if d.mem != d.size {
        return Err(ScriptError::at(
            ScriptErrorKind::ScriptOverrun {
                memory_offset: d.mem,
                script_size: d.size,
            },
            d.r.pos(),
        )
        .at_script(d.mem));
    }
    Ok(Script {
        statements,
        memory_size: script_size,
        file_span: Span {
            start,
            end: d.r.pos(),
        },
        token_count: d.tokens,
    })
}

struct Decoder<'r, 'a> {
    r: &'r mut Reader<'a>,
    mem: u32,
    size: u32,
    tokens: u32,
    limits: &'r ScriptLimits,
    is_none_name: &'r dyn Fn(u32) -> bool,
}

impl Decoder<'_, '_> {
    fn ctx(&self, e: ScriptError, field: &'static str) -> ScriptError {
        e.in_field(field).at_script(self.mem)
    }

    fn u8(&mut self) -> Result<u8> {
        let v = self.r.u8().map_err(|e| e.at_script(self.mem))?;
        self.mem += 1;
        Ok(v)
    }

    fn u16(&mut self) -> Result<u16> {
        let v = self.r.u16().map_err(|e| e.at_script(self.mem))?;
        self.mem += 2;
        Ok(v)
    }

    fn i32(&mut self) -> Result<i32> {
        let v = self.r.i32().map_err(|e| e.at_script(self.mem))?;
        self.mem += 4;
        Ok(v)
    }

    fn f32(&mut self) -> Result<f32> {
        Ok(f32::from_bits(self.i32()? as u32))
    }

    fn object(&mut self) -> Result<ObjectRef> {
        let v = self.r.object().map_err(|e| e.at_script(self.mem))?;
        self.mem += OBJECT_MEMORY_SIZE;
        Ok(v)
    }

    fn name(&mut self) -> Result<u32> {
        let v = self.r.name().map_err(|e| e.at_script(self.mem))?;
        self.mem += NAME_MEMORY_SIZE;
        Ok(v)
    }

    fn sub(&mut self, depth: u32) -> Result<Box<Token>> {
        Ok(Box::new(self.expr(depth + 1)?))
    }

    fn call(&mut self, depth: u32) -> Result<Call> {
        let mut args = Vec::new();
        loop {
            let t = self.expr(depth + 1)?;
            if matches!(t.kind, TokenKind::EndFunctionParms) {
                let end_offset = t.offset;
                // The engine serializer accepts one optional debug-info token (version 100)
                // directly after a call; consume it the same way.
                let debug_info = if self.r.peek_u8() == Some(0x42)
                    && self.r.peek_i32_after_byte() == Some(DEBUG_INFO_VERSION)
                {
                    Some(self.sub(depth)?)
                } else {
                    None
                };
                return Ok(Call {
                    args,
                    end_offset,
                    debug_info,
                });
            }
            args.push(t);
        }
    }

    fn context(&mut self, depth: u32) -> Result<Context> {
        let object = self.sub(depth)?;
        let skip = self.u16()?;
        let size = self.u8()?;
        let member = self.sub(depth)?;
        Ok(Context {
            object,
            skip,
            size,
            member,
        })
    }

    fn expr(&mut self, depth: u32) -> Result<Token> {
        if depth >= self.limits.max_depth {
            return Err(ScriptError::at(
                ScriptErrorKind::DepthExceeded {
                    max: self.limits.max_depth,
                },
                self.r.pos(),
            )
            .at_script(self.mem));
        }
        if self.tokens >= self.limits.max_tokens {
            return Err(ScriptError::at(
                ScriptErrorKind::TooManyTokens {
                    max: self.limits.max_tokens,
                },
                self.r.pos(),
            )
            .at_script(self.mem));
        }
        if self.mem >= self.size {
            // A token must start inside the declared script; anything else means the
            // stream and the declared size disagree.
            return Err(ScriptError::at(
                ScriptErrorKind::ScriptOverrun {
                    memory_offset: self.mem,
                    script_size: self.size,
                },
                self.r.pos(),
            )
            .at_script(self.mem));
        }
        self.tokens += 1;
        let offset = self.mem;
        let file_offset = self.r.pos();
        let op = self.u8().map_err(|e| self.ctx(e, "opcode"))?;
        use TokenKind as K;
        let kind = match op {
            0x00 => K::LocalVariable(self.object()?),
            0x01 => K::InstanceVariable(self.object()?),
            0x02 => K::DefaultVariable(self.object()?),
            0x04 => K::Return(self.sub(depth)?),
            0x05 => {
                let size = self.u8()?;
                K::Switch {
                    size,
                    expr: self.sub(depth)?,
                }
            }
            0x06 => K::Jump {
                target: self.u16()?,
            },
            0x07 => {
                let target = self.u16()?;
                K::JumpIfNot {
                    target,
                    cond: self.sub(depth)?,
                }
            }
            0x08 => K::Stop,
            0x09 => {
                let line = self.u16()?;
                K::Assert {
                    line,
                    cond: self.sub(depth)?,
                }
            }
            0x0A => {
                let target = self.u16()?;
                let value = if target == 0xFFFF {
                    None
                } else {
                    Some(self.sub(depth)?)
                };
                K::Case { target, value }
            }
            0x0B => K::Nothing,
            0x0C => {
                let mut labels = Vec::new();
                loop {
                    let name = self.name()?;
                    let off = self.i32()?;
                    if (self.is_none_name)(name) {
                        break K::LabelTable {
                            labels,
                            terminator: name,
                        };
                    }
                    if labels.len() as u32 >= self.limits.max_labels {
                        return Err(ScriptError::at(
                            ScriptErrorKind::CountTooLarge {
                                what: "label table",
                                count: labels.len() as i64 + 1,
                                max: u64::from(self.limits.max_labels),
                            },
                            file_offset,
                        )
                        .at_script(offset));
                    }
                    let offset_u = u32::try_from(off).map_err(|_| {
                        ScriptError::at(
                            ScriptErrorKind::ValueOutOfRange {
                                what: "label offset",
                                value: i64::from(off),
                            },
                            file_offset,
                        )
                        .at_script(offset)
                    })?;
                    labels.push(Label {
                        name,
                        offset: offset_u,
                    });
                }
            }
            0x0D => K::GotoLabel(self.sub(depth)?),
            0x0E => K::EatString(self.sub(depth)?),
            0x0F => K::Let {
                lhs: self.sub(depth)?,
                rhs: self.sub(depth)?,
            },
            0x10 => K::DynArrayElement {
                index: self.sub(depth)?,
                array: self.sub(depth)?,
            },
            0x11 => K::New {
                outer: self.sub(depth)?,
                name: self.sub(depth)?,
                flags: self.sub(depth)?,
                class: self.sub(depth)?,
            },
            0x12 => K::ClassContext(self.context(depth)?),
            0x13 => {
                let class = self.object()?;
                K::MetaCast {
                    class,
                    expr: self.sub(depth)?,
                }
            }
            0x14 => K::LetBool {
                lhs: self.sub(depth)?,
                rhs: self.sub(depth)?,
            },
            0x16 => K::EndFunctionParms,
            0x17 => K::SelfRef,
            0x18 => {
                let skip = self.u16()?;
                K::Skip {
                    skip,
                    expr: self.sub(depth)?,
                }
            }
            0x19 => K::Context(self.context(depth)?),
            0x1A => K::ArrayElement {
                index: self.sub(depth)?,
                array: self.sub(depth)?,
            },
            0x1B => {
                let name = self.name()?;
                K::VirtualFunction {
                    name,
                    call: self.call(depth)?,
                }
            }
            0x1C => {
                let function = self.object()?;
                K::FinalFunction {
                    function,
                    call: self.call(depth)?,
                }
            }
            0x1D => K::IntConst(self.i32()?),
            0x1E => K::FloatConst(self.f32()?),
            0x1F => {
                let mut bytes = Vec::new();
                loop {
                    let b = self.u8()?;
                    if b == 0 {
                        break;
                    }
                    if bytes.len() as u32 >= self.limits.max_string_bytes {
                        return Err(ScriptError::at(
                            ScriptErrorKind::CountTooLarge {
                                what: "string constant",
                                count: bytes.len() as i64 + 1,
                                max: u64::from(self.limits.max_string_bytes),
                            },
                            file_offset,
                        )
                        .at_script(offset));
                    }
                    bytes.push(b);
                }
                K::StringConst(bytes)
            }
            0x20 => K::ObjectConst(self.object()?),
            0x21 => K::NameConst(self.name()?),
            0x22 => K::RotationConst([self.i32()?, self.i32()?, self.i32()?]),
            0x23 => K::VectorConst([self.f32()?, self.f32()?, self.f32()?]),
            0x24 => K::ByteConst(self.u8()?),
            0x25 => K::IntZero,
            0x26 => K::IntOne,
            0x27 => K::True,
            0x28 => K::False,
            0x29 => K::NativeParm(self.object()?),
            0x2A => K::NoObject,
            0x2C => K::IntConstByte(self.u8()?),
            0x2D => K::BoolVariable(self.sub(depth)?),
            0x2E => {
                let class = self.object()?;
                K::DynamicCast {
                    class,
                    expr: self.sub(depth)?,
                }
            }
            0x2F => {
                let expr = self.sub(depth)?;
                K::Iterator {
                    expr,
                    end: self.u16()?,
                }
            }
            0x30 => K::IteratorPop,
            0x31 => K::IteratorNext,
            0x32 | 0x33 => {
                let strukt = self.object()?;
                let a = self.sub(depth)?;
                let b = self.sub(depth)?;
                if op == 0x32 {
                    K::StructCmpEq { strukt, a, b }
                } else {
                    K::StructCmpNe { strukt, a, b }
                }
            }
            0x34 => {
                let mut units = Vec::new();
                loop {
                    let u = self.u16()?;
                    if u == 0 {
                        break;
                    }
                    if units.len() as u32 * 2 >= self.limits.max_string_bytes {
                        return Err(ScriptError::at(
                            ScriptErrorKind::CountTooLarge {
                                what: "unicode string constant",
                                count: units.len() as i64 + 1,
                                max: u64::from(self.limits.max_string_bytes / 2),
                            },
                            file_offset,
                        )
                        .at_script(offset));
                    }
                    units.push(u);
                }
                K::UnicodeStringConst(units)
            }
            0x36 => {
                let property = self.object()?;
                K::StructMember {
                    property,
                    expr: self.sub(depth)?,
                }
            }
            0x37 => K::DynArrayLength(self.sub(depth)?),
            0x38 => {
                let name = self.name()?;
                K::GlobalFunction {
                    name,
                    call: self.call(depth)?,
                }
            }
            0x39 => {
                let cast = self.u8()?;
                K::PrimitiveCast {
                    cast,
                    expr: self.sub(depth)?,
                }
            }
            0x3B..=0x3E => K::DelegateCompare {
                a: self.sub(depth)?,
                b: self.sub(depth)?,
                end: self.sub(depth)?,
            },
            0x3F => K::EmptyDelegate,
            0x40 => K::DynArrayInsert {
                array: self.sub(depth)?,
                index: self.sub(depth)?,
                count: self.sub(depth)?,
            },
            0x41 => K::DynArrayRemove {
                array: self.sub(depth)?,
                index: self.sub(depth)?,
                count: self.sub(depth)?,
            },
            0x42 => K::DebugInfo {
                version: self.i32()?,
                line: self.i32()?,
                text_pos: self.i32()?,
                op: self.u8()?,
            },
            0x43 => {
                let property = self.object()?;
                let name = self.name()?;
                K::DelegateFunction {
                    property,
                    name,
                    call: self.call(depth)?,
                }
            }
            0x44 => K::DelegateProperty(self.name()?),
            0x45 => K::LetDelegate {
                lhs: self.sub(depth)?,
                rhs: self.sub(depth)?,
            },
            0x48 => {
                let cond = self.sub(depth)?;
                let skip_true = self.u16()?;
                let a = self.sub(depth)?;
                let skip_false = self.u16()?;
                let b = self.sub(depth)?;
                K::Conditional {
                    cond,
                    skip_true,
                    a,
                    skip_false,
                    b,
                }
            }
            EX_FIRST_NATIVE..=0xFF => K::NativeCall {
                index: u16::from(op),
                call: self.call(depth)?,
            },
            EX_EXTENDED_NATIVE..=0x6F => {
                let ext = self.u8()?;
                K::NativeCall {
                    index: (u16::from(op - EX_EXTENDED_NATIVE) << 8) | u16::from(ext),
                    call: self.call(depth)?,
                }
            }
            _ => {
                return Err(ScriptError::at(
                    ScriptErrorKind::UnknownToken { opcode: op },
                    file_offset,
                )
                .at_script(offset));
            }
        };
        Ok(Token {
            offset,
            file_offset,
            memory_size: self.mem - offset,
            opcode: op,
            kind,
        })
    }
}
