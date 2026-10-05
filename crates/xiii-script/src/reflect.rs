//! Reflected UnrealScript objects in version-100 XIII packages: `UField`, `UStruct`,
//! `UFunction`, `UState`, `UClass` (including the class defaults), `UConst`, `UEnum` and the
//! `UProperty` family.
//!
//! Layout references: UELib `src/Core/Classes/{UField,UStruct,UFunction,UState,UClass,UConst,
//! UEnum}.cs` and `src/Core/Classes/Props/*.cs` at
//! `EliotVU/Unreal-Library@3207a17e9b294be3d1bf26b18e07ccff7e1d4b0c` (MIT), read for the
//! version-100 branches. XIII (licensee 58) deviations were measured on the GOG corpus and are
//! documented on the fields below and in `README.md`. Independent implementation; no code was
//! copied.

use xiii_package::{Limits, ObjectRef, Package, PropertyBlock, RF_HAS_STACK, Span, StateFrame};

use crate::bytecode::{Script, ScriptLimits, decode_with_reader};
use crate::error::{Result, ScriptError, ScriptErrorKind};
use crate::reader::{Reader, Tables};

/// XIII 24-bit function flags (measured; differs from stock UE2 ordering).
///
/// Stock UE2 `Defined` (0x2) and `Singular` (0x20) are absent from the low bits; the
/// remaining stock flags shift down. Verified against the `engine.u` source text
/// (1 765 matched declarations): every flag below matched its keyword in 100% of cases.
pub mod function_flags {
    /// `final`.
    pub const FINAL: u32 = 0x0000_0001;
    /// `iterator`.
    pub const ITERATOR: u32 = 0x0000_0002;
    /// `latent`.
    pub const LATENT: u32 = 0x0000_0004;
    /// `preoperator`.
    pub const PRE_OPERATOR: u32 = 0x0000_0008;
    /// Replicated function (a `u16` replication offset follows the flags).
    pub const NET: u32 = 0x0000_0010;
    /// Reliable replication.
    pub const NET_RELIABLE: u32 = 0x0000_0020;
    /// `simulated`.
    pub const SIMULATED: u32 = 0x0000_0040;
    /// `exec`.
    pub const EXEC: u32 = 0x0000_0080;
    /// `native`.
    pub const NATIVE: u32 = 0x0000_0100;
    /// `event`.
    pub const EVENT: u32 = 0x0000_0200;
    /// `operator`.
    pub const OPERATOR: u32 = 0x0000_0400;
    /// `static`.
    pub const STATIC: u32 = 0x0000_0800;
    /// Set on almost every function (meaning unconfirmed; stock UE2 position suggests
    /// `NoExport`).
    pub const UNKNOWN_1000: u32 = 0x0000_1000;
    /// `singular` (moved; stock UE2 0x20).
    pub const SINGULAR: u32 = 0x0001_0000;
    /// XIII `debugonly` specifier.
    pub const DEBUG_ONLY: u32 = 0x0002_0000;
    /// Function has a body (stock UE2 `Defined`, 0x2).
    pub const DEFINED: u32 = 0x0004_0000;

    /// Names of the set flags (unknown bits as hex).
    pub fn names(flags: u32) -> Vec<String> {
        const TABLE: &[(u32, &str)] = &[
            (FINAL, "final"),
            (ITERATOR, "iterator"),
            (LATENT, "latent"),
            (PRE_OPERATOR, "preoperator"),
            (NET, "net"),
            (NET_RELIABLE, "reliable"),
            (SIMULATED, "simulated"),
            (EXEC, "exec"),
            (NATIVE, "native"),
            (EVENT, "event"),
            (OPERATOR, "operator"),
            (STATIC, "static"),
            (UNKNOWN_1000, "f1000"),
            (SINGULAR, "singular"),
            (DEBUG_ONLY, "debugonly"),
            (DEFINED, "defined"),
        ];
        let mut out = Vec::new();
        let mut rest = flags;
        for (bit, name) in TABLE {
            if flags & bit != 0 {
                out.push((*name).to_owned());
                rest &= !bit;
            }
        }
        if rest != 0 {
            out.push(format!("0x{rest:X}"));
        }
        out
    }
}

/// Stock UE2 property flags used here (verified: the `Net` bit gates the replication offset in
/// every property of the corpus).
pub mod property_flags {
    /// Optional parameter.
    pub const OPTIONAL_PARM: u32 = 0x0000_0010;
    /// Replicated (a `u16` replication offset follows the category).
    pub const NET: u32 = 0x0000_0020;
    /// Function parameter.
    pub const PARM: u32 = 0x0000_0080;
    /// `out` parameter.
    pub const OUT_PARM: u32 = 0x0000_0100;
    /// Return value.
    pub const RETURN_PARM: u32 = 0x0000_0400;
    /// `coerce` parameter.
    pub const COERCE_PARM: u32 = 0x0000_0800;
}

/// `UField` header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FieldHeader {
    /// Super field (`UField::SuperField`; v100 stores it in `UField`).
    pub super_field: ObjectRef,
    /// Next field in the owner's child list.
    pub next: ObjectRef,
}

/// `UStruct` header plus its bytecode.
#[derive(Debug, Clone, PartialEq)]
pub struct StructHeader {
    /// Field part.
    pub field: FieldHeader,
    /// Source text buffer (stripped placeholders in the game packages).
    pub script_text: ObjectRef,
    /// First child field.
    pub children: ObjectRef,
    /// Friendly name (operator symbol for operators).
    pub friendly_name: u32,
    /// Source line.
    pub line: i32,
    /// Source text position.
    pub text_pos: i32,
    /// Decoded bytecode.
    pub script: Script,
}

/// `UFunction`.
#[derive(Debug, Clone, PartialEq)]
pub struct Function {
    /// Struct part.
    pub header: StructHeader,
    /// Native index (`iNative`); 0 for non-indexed functions.
    pub native_index: u16,
    /// Operator precedence.
    pub operator_precedence: u8,
    /// XIII 24-bit function flags, see [`function_flags`].
    pub flags: u32,
    /// Replication offset (present when [`function_flags::NET`] is set).
    pub rep_offset: Option<u16>,
}

impl Function {
    /// True for `native` functions.
    pub fn is_native(&self) -> bool {
        self.flags & function_flags::NATIVE != 0
    }
}

/// `UState` fields after the struct part.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StateFields {
    /// Probe mask.
    pub probe_mask: u64,
    /// Ignore mask.
    pub ignore_mask: u64,
    /// Label table memory offset (`0xFFFF` = none).
    pub label_table_offset: u16,
    /// State flags; **u16 in XIII** (stock UE2 u32). Observed: 1 editable, 2 auto.
    pub state_flags: u16,
}

/// `UState`.
#[derive(Debug, Clone, PartialEq)]
pub struct State {
    /// Struct part.
    pub header: StructHeader,
    /// State part.
    pub state: StateFields,
}

/// One `UClass` dependency record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Dependency {
    /// Class depended upon.
    pub class: ObjectRef,
    /// Deep dependency flag.
    pub deep: u32,
    /// Script text CRC.
    pub script_text_crc: u32,
}

/// `UClass`, including its default properties.
#[derive(Debug, Clone, PartialEq)]
pub struct Class {
    /// State frame when the export has `RF_HasStack` (not observed for classes).
    pub state_frame: Option<StateFrame>,
    /// Struct part (class-level code, e.g. auto state entry).
    pub header: StructHeader,
    /// State part.
    pub state: StateFields,
    /// Class flags. XIII stores 18 bytes for flags + GUID where stock UE2 stores 20; bytes
    /// 2..18 are zero in every class of the corpus, so the split is inferred: a `u16` flags
    /// field (consistent with the narrowed state/function flags) and a 16-byte GUID.
    pub class_flags: u16,
    /// Class GUID (all zero in the corpus).
    pub class_guid: [u8; 16],
    /// Dependencies.
    pub dependencies: Vec<Dependency>,
    /// Imported package names.
    pub package_imports: Vec<u32>,
    /// `within` class.
    pub within: ObjectRef,
    /// Config file name.
    pub config_name: u32,
    /// Hidden editor categories.
    pub hide_categories: Vec<u32>,
    /// Default properties: a tagged-property block ending exactly at the payload end.
    pub defaults: PropertyBlock,
}

/// `UStruct` export (script struct).
#[derive(Debug, Clone, PartialEq)]
pub struct StructDef {
    /// Struct part (no defaults in v100).
    pub header: StructHeader,
}

/// `UConst`.
#[derive(Debug, Clone, PartialEq)]
pub struct Const {
    /// Field part.
    pub field: FieldHeader,
    /// Constant text.
    pub value: String,
}

/// `UEnum`.
#[derive(Debug, Clone, PartialEq)]
pub struct Enum {
    /// Field part.
    pub field: FieldHeader,
    /// Enumerator names.
    pub names: Vec<u32>,
}

/// Property type with its per-type references.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PropertyKind {
    /// `ByteProperty` with optional enum.
    Byte {
        /// Enum (null for plain bytes).
        enum_ref: ObjectRef,
    },
    /// `IntProperty`.
    Int,
    /// `BoolProperty`.
    Bool,
    /// `FloatProperty`.
    Float,
    /// `ObjectProperty`.
    Object {
        /// Property class.
        class: ObjectRef,
    },
    /// `ClassProperty`.
    Class {
        /// Property class (`Class`).
        class: ObjectRef,
        /// Meta class.
        meta_class: ObjectRef,
    },
    /// `NameProperty`.
    Name,
    /// `StrProperty`.
    Str,
    /// `StructProperty`.
    Struct {
        /// Struct type.
        strukt: ObjectRef,
    },
    /// `ArrayProperty`.
    Array {
        /// Inner property.
        inner: ObjectRef,
    },
    /// `DelegateProperty`.
    Delegate {
        /// Delegate function signature.
        function: ObjectRef,
    },
}

impl PropertyKind {
    /// Short type name.
    pub fn name(&self) -> &'static str {
        match self {
            PropertyKind::Byte { .. } => "byte",
            PropertyKind::Int => "int",
            PropertyKind::Bool => "bool",
            PropertyKind::Float => "float",
            PropertyKind::Object { .. } => "object",
            PropertyKind::Class { .. } => "class",
            PropertyKind::Name => "name",
            PropertyKind::Str => "string",
            PropertyKind::Struct { .. } => "struct",
            PropertyKind::Array { .. } => "array",
            PropertyKind::Delegate { .. } => "delegate",
        }
    }
}

/// `UProperty` and subclasses.
#[derive(Debug, Clone, PartialEq)]
pub struct Property {
    /// Field part.
    pub field: FieldHeader,
    /// Static array dimension; **i16 in XIII** (UELib reads XIII ArrayDim as 16-bit).
    pub array_dim: i16,
    /// Property flags (stock UE2 u32 values, see [`property_flags`]).
    pub flags: u32,
    /// Editor category name.
    pub category: u32,
    /// Replication offset (present when [`property_flags::NET`] is set).
    pub rep_offset: Option<u16>,
    /// Type-specific part.
    pub kind: PropertyKind,
}

/// A decoded reflected export.
#[derive(Debug, Clone, PartialEq)]
pub enum ScriptObject {
    /// `Core.Function`.
    Function(Function),
    /// `Core.State`.
    State(State),
    /// `Core.Class`.
    Class(Box<Class>),
    /// `Core.Struct`.
    Struct(StructDef),
    /// `Core.Const`.
    Const(Const),
    /// `Core.Enum`.
    Enum(Enum),
    /// `Core.*Property`.
    Property(Property),
}

impl ScriptObject {
    /// Struct header for functions, states, classes and structs.
    pub fn struct_header(&self) -> Option<&StructHeader> {
        match self {
            ScriptObject::Function(f) => Some(&f.header),
            ScriptObject::State(s) => Some(&s.header),
            ScriptObject::Class(c) => Some(&c.header),
            ScriptObject::Struct(s) => Some(&s.header),
            _ => None,
        }
    }

    /// Field header of any reflected object.
    pub fn field(&self) -> FieldHeader {
        match self {
            ScriptObject::Function(f) => f.header.field,
            ScriptObject::State(s) => s.header.field,
            ScriptObject::Class(c) => c.header.field,
            ScriptObject::Struct(s) => s.header.field,
            ScriptObject::Const(c) => c.field,
            ScriptObject::Enum(e) => e.field,
            ScriptObject::Property(p) => p.field,
        }
    }
}

/// Reflected export kinds recognised by class path.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScriptClassKind {
    /// `Core.Function`.
    Function,
    /// `Core.State`.
    State,
    /// `Core.Class` (or a null class reference).
    Class,
    /// `Core.Struct`.
    Struct,
    /// `Core.Const`.
    Const,
    /// `Core.Enum`.
    Enum,
    /// `Core.*Property`.
    Property,
}

/// Kind of a class path, if it is a reflected Core class.
pub fn script_class_kind(class_path: &str) -> Option<ScriptClassKind> {
    let lower = class_path.to_ascii_lowercase();
    Some(match lower.as_str() {
        "core.function" => ScriptClassKind::Function,
        "core.state" => ScriptClassKind::State,
        "core.class" => ScriptClassKind::Class,
        "core.struct" => ScriptClassKind::Struct,
        "core.const" => ScriptClassKind::Const,
        "core.enum" => ScriptClassKind::Enum,
        s if s.starts_with("core.") && s.ends_with("property") => ScriptClassKind::Property,
        _ => return None,
    })
}

fn none_lookup(package: &Package) -> impl Fn(u32) -> bool + '_ {
    move |i| {
        package
            .names()
            .get(i as usize)
            .is_some_and(|n| n.text.eq_ignore_ascii_case("None"))
    }
}

fn read_field(r: &mut Reader<'_>) -> Result<FieldHeader> {
    Ok(FieldHeader {
        super_field: r.object().map_err(|e| e.in_field("UField.SuperField"))?,
        next: r.object().map_err(|e| e.in_field("UField.Next"))?,
    })
}

fn read_struct(
    r: &mut Reader<'_>,
    is_none: &dyn Fn(u32) -> bool,
    limits: &ScriptLimits,
) -> Result<StructHeader> {
    let field = read_field(r)?;
    let script_text = r.object().map_err(|e| e.in_field("UStruct.ScriptText"))?;
    let children = r.object().map_err(|e| e.in_field("UStruct.Children"))?;
    let friendly_name = r.name().map_err(|e| e.in_field("UStruct.FriendlyName"))?;
    let line = r.i32().map_err(|e| e.in_field("UStruct.Line"))?;
    let text_pos = r.i32().map_err(|e| e.in_field("UStruct.TextPos"))?;
    let size_at = r.pos();
    let size = r.i32().map_err(|e| e.in_field("UStruct.ScriptSize"))?;
    let size = u32::try_from(size).map_err(|_| {
        ScriptError::at(
            ScriptErrorKind::ScriptSizeInvalid {
                size,
                max: limits.max_script_size,
            },
            size_at,
        )
        .in_field("UStruct.ScriptSize")
    })?;
    let script = decode_with_reader(r, size, is_none, limits).map_err(|e| e.in_field("script"))?;
    Ok(StructHeader {
        field,
        script_text,
        children,
        friendly_name,
        line,
        text_pos,
        script,
    })
}

fn read_state_fields(r: &mut Reader<'_>) -> Result<StateFields> {
    Ok(StateFields {
        probe_mask: r.u64().map_err(|e| e.in_field("UState.ProbeMask"))?,
        ignore_mask: r.u64().map_err(|e| e.in_field("UState.IgnoreMask"))?,
        label_table_offset: r.u16().map_err(|e| e.in_field("UState.LabelTableOffset"))?,
        state_flags: r.u16().map_err(|e| e.in_field("UState.StateFlags"))?,
    })
}

fn read_names(r: &mut Reader<'_>, what: &'static str, max: u32) -> Result<Vec<u32>> {
    let n = r.count(what, max).map_err(|e| e.in_field(what))?;
    let mut out = Vec::with_capacity(n as usize);
    for _ in 0..n {
        out.push(r.name().map_err(|e| e.in_field(what))?);
    }
    Ok(out)
}

fn read_state_frame(r: &mut Reader<'_>) -> Result<StateFrame> {
    let start = r.pos();
    let node = r.object().map_err(|e| e.in_field("state_frame.node"))?;
    let state_node = r
        .object()
        .map_err(|e| e.in_field("state_frame.state_node"))?;
    let probe_mask = r.u64().map_err(|e| e.in_field("state_frame.probe_mask"))?;
    let latent_action = r
        .u32()
        .map_err(|e| e.in_field("state_frame.latent_action"))?;
    let offset = if node.is_null() {
        None
    } else {
        Some(r.compact().map_err(|e| e.in_field("state_frame.offset"))?)
    };
    Ok(StateFrame {
        node,
        state_node,
        probe_mask,
        latent_action,
        offset,
        span: Span {
            start,
            end: r.pos(),
        },
    })
}

/// Reads one reflected export (function, state, class, struct, const, enum or property).
///
/// The payload must be consumed exactly; otherwise [`ScriptErrorKind::TrailingBytes`] is
/// returned. `data` must be the buffer the package was parsed from.
pub fn read_script_object(
    package: &Package,
    data: &[u8],
    export: usize,
    limits: &ScriptLimits,
    package_limits: &Limits,
) -> Result<ScriptObject> {
    read_inner(package, data, export, limits, package_limits)
        .map_err(|e| e.in_export(export as u32))
}

fn read_inner(
    package: &Package,
    data: &[u8],
    export: usize,
    limits: &ScriptLimits,
    package_limits: &Limits,
) -> Result<ScriptObject> {
    let class_path = package.export_class_path(export).unwrap_or("?");
    let kind = script_class_kind(class_path).ok_or_else(|| {
        ScriptError::new(ScriptErrorKind::UnsupportedClass {
            class: class_path.to_owned(),
        })
    })?;
    if kind == ScriptClassKind::Class {
        return read_class(package, data, export, limits, package_limits)
            .map(|c| ScriptObject::Class(Box::new(c)));
    }
    // Non-class objects start with UObject's state frame (if flagged) and tagged properties.
    let props = package
        .read_object_properties(data, export, package_limits)
        .map_err(|e| ScriptError::from_package(e, 0).in_field("object properties"))?;
    let tables = Tables::of(package);
    let mut r = Reader::new(data, props.block.span.end, props.payload.end, tables)?;
    let is_none = none_lookup(package);
    let obj = match kind {
        ScriptClassKind::Function => {
            let header = read_struct(&mut r, &is_none, limits)?;
            let native_index = r.u16().map_err(|e| e.in_field("UFunction.iNative"))?;
            let operator_precedence = r
                .u8()
                .map_err(|e| e.in_field("UFunction.OperatorPrecedence"))?;
            let flags = r.u24().map_err(|e| e.in_field("UFunction.FunctionFlags"))?;
            let rep_offset = if flags & function_flags::NET != 0 {
                Some(r.u16().map_err(|e| e.in_field("UFunction.RepOffset"))?)
            } else {
                None
            };
            ScriptObject::Function(Function {
                header,
                native_index,
                operator_precedence,
                flags,
                rep_offset,
            })
        }
        ScriptClassKind::State => {
            let header = read_struct(&mut r, &is_none, limits)?;
            let state = read_state_fields(&mut r)?;
            ScriptObject::State(State { header, state })
        }
        ScriptClassKind::Struct => ScriptObject::Struct(StructDef {
            header: read_struct(&mut r, &is_none, limits)?,
        }),
        ScriptClassKind::Const => {
            let field = read_field(&mut r)?;
            let value = r
                .fstring(package_limits.max_string_units)
                .map_err(|e| e.in_field("UConst.Value"))?;
            ScriptObject::Const(Const { field, value })
        }
        ScriptClassKind::Enum => {
            let field = read_field(&mut r)?;
            let names = read_names(&mut r, "UEnum.Names", limits.max_array)?;
            ScriptObject::Enum(Enum { field, names })
        }
        ScriptClassKind::Property => ScriptObject::Property(read_property(&mut r, class_path)?),
        ScriptClassKind::Class => unreachable!("handled above"),
    };
    r.expect_end()?;
    Ok(obj)
}

fn read_property(r: &mut Reader<'_>, class_path: &str) -> Result<Property> {
    let field = read_field(r)?;
    let array_dim = r.i16().map_err(|e| e.in_field("UProperty.ArrayDim"))?;
    let flags = r.u32().map_err(|e| e.in_field("UProperty.PropertyFlags"))?;
    let category = r.name().map_err(|e| e.in_field("UProperty.Category"))?;
    let rep_offset = if flags & property_flags::NET != 0 {
        Some(r.u16().map_err(|e| e.in_field("UProperty.RepOffset"))?)
    } else {
        None
    };
    let short = class_path
        .rsplit('.')
        .next()
        .unwrap_or(class_path)
        .to_ascii_lowercase();
    let mut obj = |f: &'static str| r.object().map_err(|e| e.in_field(f));
    let kind = match short.as_str() {
        "byteproperty" => PropertyKind::Byte {
            enum_ref: obj("ByteProperty.Enum")?,
        },
        "intproperty" => PropertyKind::Int,
        "boolproperty" => PropertyKind::Bool,
        "floatproperty" => PropertyKind::Float,
        "objectproperty" => PropertyKind::Object {
            class: obj("ObjectProperty.PropertyClass")?,
        },
        "classproperty" => PropertyKind::Class {
            class: obj("ClassProperty.PropertyClass")?,
            meta_class: obj("ClassProperty.MetaClass")?,
        },
        "nameproperty" => PropertyKind::Name,
        "strproperty" => PropertyKind::Str,
        "structproperty" => PropertyKind::Struct {
            strukt: obj("StructProperty.Struct")?,
        },
        "arrayproperty" => PropertyKind::Array {
            inner: obj("ArrayProperty.Inner")?,
        },
        "delegateproperty" => PropertyKind::Delegate {
            function: obj("DelegateProperty.Function")?,
        },
        _ => {
            // Map/FixedArray/Pointer properties do not occur in the corpus; refuse rather
            // than guess their layout.
            return Err(ScriptError::new(ScriptErrorKind::UnsupportedClass {
                class: class_path.to_owned(),
            }));
        }
    };
    Ok(Property {
        field,
        array_dim,
        flags,
        category,
        rep_offset,
        kind,
    })
}

fn read_class(
    package: &Package,
    data: &[u8],
    export: usize,
    limits: &ScriptLimits,
    package_limits: &Limits,
) -> Result<Class> {
    let e = package.exports().get(export).ok_or_else(|| {
        ScriptError::new(ScriptErrorKind::ValueOutOfRange {
            what: "export index",
            value: export as i64,
        })
    })?;
    let span = e.serial_span().ok_or_else(|| {
        ScriptError::new(ScriptErrorKind::ValueOutOfRange {
            what: "empty class payload",
            value: 0,
        })
    })?;
    if data.len() != package.file_len() {
        return Err(ScriptError::new(ScriptErrorKind::ValueOutOfRange {
            what: "buffer length",
            value: data.len() as i64,
        }));
    }
    let tables = Tables::of(package);
    let mut r = Reader::new(data, span.start, span.end, tables)?;
    let is_none = none_lookup(package);
    // UClass skips UObject's tagged properties: the payload starts with native data.
    let state_frame = if e.flags & RF_HAS_STACK != 0 {
        Some(read_state_frame(&mut r)?)
    } else {
        None
    };
    let header = read_struct(&mut r, &is_none, limits)?;
    let state = read_state_fields(&mut r)?;
    let class_flags = r.u16().map_err(|e| e.in_field("UClass.ClassFlags"))?;
    let guid_bytes = r.take(16).map_err(|e| e.in_field("UClass.ClassGuid"))?;
    let mut class_guid = [0u8; 16];
    class_guid.copy_from_slice(guid_bytes);
    let n = r
        .count("UClass.Dependencies", limits.max_array)
        .map_err(|e| e.in_field("UClass.Dependencies"))?;
    let mut dependencies = Vec::with_capacity(n as usize);
    for _ in 0..n {
        dependencies.push(Dependency {
            class: r
                .object()
                .map_err(|e| e.in_field("UClass.Dependencies.Class"))?,
            deep: r
                .u32()
                .map_err(|e| e.in_field("UClass.Dependencies.Deep"))?,
            script_text_crc: r
                .u32()
                .map_err(|e| e.in_field("UClass.Dependencies.ScriptTextCRC"))?,
        });
    }
    let package_imports = read_names(&mut r, "UClass.PackageImports", limits.max_array)?;
    let within = r.object().map_err(|e| e.in_field("UClass.ClassWithin"))?;
    let config_name = r.name().map_err(|e| e.in_field("UClass.ClassConfigName"))?;
    let hide_categories = read_names(&mut r, "UClass.HideCategories", limits.max_array)?;
    let defaults = package
        .read_property_block(data, r.pos(), span.end, package_limits)
        .map_err(|e| ScriptError::from_package(e, 0).in_field("UClass defaults"))?;
    if defaults.span.end != span.end {
        return Err(ScriptError::at(
            ScriptErrorKind::TrailingBytes {
                consumed_to: defaults.span.end,
                payload_end: span.end,
            },
            defaults.span.end,
        )
        .in_field("UClass defaults"));
    }
    Ok(Class {
        state_frame,
        header,
        state,
        class_flags,
        class_guid,
        dependencies,
        package_imports,
        within,
        config_name,
        hide_categories,
        defaults,
    })
}

/// Default properties of a `Core.Class` export: the tagged-property block that follows the
/// native `UField`/`UStruct`/`UState`/`UClass` data. Ends exactly at the payload end.
pub fn class_defaults(
    package: &Package,
    data: &[u8],
    export: usize,
    limits: &ScriptLimits,
    package_limits: &Limits,
) -> Result<PropertyBlock> {
    let class_path = package.export_class_path(export).unwrap_or("?");
    if script_class_kind(class_path) != Some(ScriptClassKind::Class) {
        return Err(ScriptError::new(ScriptErrorKind::UnsupportedClass {
            class: class_path.to_owned(),
        })
        .in_export(export as u32));
    }
    read_class(package, data, export, limits, package_limits)
        .map(|c| c.defaults)
        .map_err(|e| e.in_export(export as u32))
}
