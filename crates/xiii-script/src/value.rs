//! Script values and types used by the interpreter.

use std::fmt;

use crate::linker::GlobalRef;

/// Interpreter object handle (index into the VM object table).
pub type ObjectId = u32;

/// Reference held by an object-typed value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjRef {
    /// An instantiated object (map actor, synthetic actor, class-default object).
    Instance(ObjectId),
    /// A package object that is not instantiated (class, function, texture, ...).
    Static(GlobalRef),
    /// An object in a package outside the loaded script set (e.g. a `Sound` in a `.uax`, a
    /// `Texture` in a `.utx`). The VM interns the full object path and the class recorded in the
    /// referencing package's import table; see [`crate::vm::Vm::external_object`]. Equality is by
    /// path (the interning table deduplicates paths), and the value keeps the reference instead of
    /// collapsing to `None` (the pre-item3i behaviour).
    External(u32),
}

/// An UnrealScript delegate: the object the delegate runs on plus the function name. `UE2`
/// stores delegates as a `(Object, FunctionName)` pair; an unbound delegate has no object.
/// Produced by the `delegate <property>` token (`0x44`, a fresh delegate bound to the current
/// context) and by `DelegateProperty` assignment (`0x45`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delegate {
    /// The object the delegate is bound to (`None` for an empty delegate).
    pub object: Option<ObjRef>,
    /// The UnrealScript function to run.
    pub function: String,
}

/// A script value. `Unsupported` holds values the loader could not convert (unknown struct
/// layouts, raw arrays); reading one in script is an explicit error.
#[derive(Debug, Clone, PartialEq)]
pub enum Value {
    /// No value (procedure result).
    Void,
    /// `int`.
    Int(i32),
    /// `float`.
    Float(f32),
    /// `bool`.
    Bool(bool),
    /// `byte` / enum.
    Byte(u8),
    /// `name` (compared case-insensitively).
    Name(String),
    /// `string`.
    Str(String),
    /// `object` / `class` reference (`None` = null).
    Object(Option<ObjRef>),
    /// A native-only meta-class (`class'Core.Class'`) that has no export in any loaded package.
    /// Distinct from `Object(None)` so class checks and `DynamicLoadObject` do not silently
    /// treat it as null.
    NativeClass(String),
    /// `vector`.
    Vector([f32; 3]),
    /// `rotator`.
    Rotator([i32; 3]),
    /// Other struct: members by (lowercase) name.
    Struct(Vec<(String, Value)>),
    /// Dynamic array.
    Array(Vec<Value>),
    /// `delegate` reference (`None` = unbound/empty delegate).
    Delegate(Option<Delegate>),
    /// Value the loader could not convert (description).
    Unsupported(String),
}

impl Value {
    /// Short type label for errors.
    pub fn type_name(&self) -> &'static str {
        match self {
            Value::Void => "void",
            Value::Int(_) => "int",
            Value::Float(_) => "float",
            Value::Bool(_) => "bool",
            Value::Byte(_) => "byte",
            Value::Name(_) => "name",
            Value::Str(_) => "string",
            Value::Object(_) => "object",
            Value::NativeClass(_) => "class",
            Value::Vector(_) => "vector",
            Value::Rotator(_) => "rotator",
            Value::Struct(_) => "struct",
            Value::Array(_) => "array",
            Value::Delegate(_) => "delegate",
            Value::Unsupported(_) => "unsupported",
        }
    }

    /// True for the `None` name (case-insensitive).
    pub fn is_none_name(&self) -> bool {
        matches!(self, Value::Name(n) if n.eq_ignore_ascii_case("None"))
    }
}

/// Static type of a property slot.
#[derive(Debug, Clone, PartialEq)]
pub enum Ty {
    /// `int`.
    Int,
    /// `float`.
    Float,
    /// `bool`.
    Bool,
    /// `byte`.
    Byte,
    /// `name`.
    Name,
    /// `string`.
    Str,
    /// `object` / `class`.
    Object,
    /// `vector`.
    Vector,
    /// `rotator`.
    Rotator,
    /// Other struct with member (name, type) pairs.
    Struct(Vec<(String, Ty)>),
    /// Dynamic array of the inner type.
    Array(Box<Ty>),
    /// Delegate (an `(object, function)` pair).
    Delegate,
}

impl Ty {
    /// Zero value of the type (UnrealScript default initialisation).
    pub fn zero(&self) -> Value {
        match self {
            Ty::Int => Value::Int(0),
            Ty::Float => Value::Float(0.0),
            Ty::Bool => Value::Bool(false),
            Ty::Byte => Value::Byte(0),
            Ty::Name => Value::Name("None".to_owned()),
            Ty::Str => Value::Str(String::new()),
            Ty::Object => Value::Object(None),
            Ty::Vector => Value::Vector([0.0; 3]),
            Ty::Rotator => Value::Rotator([0; 3]),
            Ty::Struct(members) => {
                Value::Struct(members.iter().map(|(n, t)| (n.clone(), t.zero())).collect())
            }
            Ty::Array(_) => Value::Array(Vec::new()),
            Ty::Delegate => Value::Delegate(None),
        }
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Value::Void => write!(f, "void"),
            Value::Int(v) => write!(f, "{v}"),
            Value::Float(v) => write!(f, "{v:?}"),
            Value::Bool(v) => write!(f, "{v}"),
            Value::Byte(v) => write!(f, "{v}b"),
            Value::Name(n) => write!(f, "'{n}'"),
            Value::Str(s) => write!(f, "{s:?}"),
            Value::Object(None) => write!(f, "None"),
            Value::Object(Some(ObjRef::Instance(i))) => write!(f, "obj#{i}"),
            Value::Object(Some(ObjRef::Static(g))) => {
                write!(f, "static#{}:{}", g.package, g.export)
            }
            Value::Object(Some(ObjRef::External(i))) => write!(f, "external#{i}"),
            Value::NativeClass(n) => write!(f, "class'{n}'"),
            Value::Vector([x, y, z]) => write!(f, "vect({x:?},{y:?},{z:?})"),
            Value::Rotator([p, y, r]) => write!(f, "rot({p},{y},{r})"),
            Value::Struct(m) => write!(f, "struct({} members)", m.len()),
            Value::Array(a) => write!(f, "array({})", a.len()),
            Value::Delegate(None) => write!(f, "delegate(None)"),
            Value::Delegate(Some(d)) => write!(
                f,
                "delegate({}:{})",
                d.function,
                d.object
                    .map_or_else(|| "None".to_owned(), |o| format!("{o:?}"))
            ),
            Value::Unsupported(d) => write!(f, "<unsupported {d}>"),
        }
    }
}
