//! Compiled UnrealScript support for XIII Classic (M2c): reflected class/struct/function/state
//! payloads (version 100, licensee 58), a bytecode token decoder, a disassembler, a native
//! function catalog and DLL-symbol cross-reference.
//!
//! Filesystem-free: callers pass bytes. Every untrusted size is bounded by
//! [`ScriptLimits`] or `xiii_package::Limits`; unknown opcodes and incomplete payloads fail
//! with [`ScriptError`] carrying file and code offsets. See `README.md` for the verified
//! layouts, corpus evidence and the interpreter plan.

#![warn(missing_docs)]

pub mod bytecode;
pub mod disasm;
mod error;
pub mod linker;
pub mod natives;
pub mod pe;
pub mod physics;
mod reader;
pub mod reflect;
pub mod registry;
pub mod value;
pub mod vm;

pub use bytecode::{Call, Label, Script, ScriptLimits, Token, TokenKind, decode_script};
pub use error::{Result, ScriptError, ScriptErrorKind};
pub use linker::{GlobalRef, ScriptPackage, ScriptSet};
pub use physics::{MoveOutcome, WorldHit, WorldPhysics};
pub use reader::{Reader, Tables};
pub use reflect::{
    Class, Function, Property, PropertyKind, ScriptClassKind, ScriptObject, State, StructHeader,
    class_defaults, read_script_object, script_class_kind,
};
pub use value::{ObjRef, ObjectId, Ty, Value};
pub use vm::{TraceEvent, TraceKind, Vm, VmError, VmErrorKind, VmLimits};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vm_tests;
