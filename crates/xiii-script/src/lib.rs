//! Compiled UnrealScript support for XIII Classic (M2c): reflected class/struct/function/state
//! payloads (version 100, licensee 58), a bytecode token decoder, a disassembler, a native
//! function catalog and DLL-symbol cross-reference.
//!
//! Filesystem-free: callers pass bytes. Every untrusted size is bounded by
//! [`ScriptLimits`] or `xiii_package::Limits`; unknown opcodes and incomplete payloads fail
//! with [`ScriptError`] carrying file and code offsets. See `README.md` for the verified
//! layouts, corpus evidence and the interpreter plan.

#![warn(missing_docs)]

pub mod animation;
pub mod bytecode;
pub mod canvas;
pub mod cartoon;
pub mod cinematics;
pub mod disasm;
mod error;
pub mod events;
pub mod external;
mod item20;
pub mod linker;
pub mod localize;
pub mod natives;
pub mod navigation;
pub mod pe;
pub mod physics;
mod reader;
pub mod reflect;
pub mod registry;
pub mod residuals;
pub mod value;
pub mod vm;
pub mod voice;

pub use animation::{AnimationData, FixedAnimation, SeqInfo};
pub use bytecode::{Call, Label, Script, ScriptLimits, Token, TokenKind, decode_script};
pub use error::{Result, ScriptError, ScriptErrorKind};
pub use events::{
    DialogueEvent, PresentationEvent, RenderTargetEvent, SaveCheckpointEvent, SoundEvent,
    TravelRequest, TravelSource,
};
pub use external::ExternalObjectData;
pub use linker::{ExternalLookup, ExternalPackage, GlobalRef, ScriptPackage, ScriptSet};
pub use localize::{LocalizationData, placeholder};
pub use navigation::{
    EmptyNavigation, NavEdgeInfo, NavPointInfo, NavigationData, find_path, move_step, nearest_point,
};
pub use physics::{CylinderZones, HitZones, MoveOutcome, WorldHit, WorldPhysics};
pub use reader::{Reader, Tables};
pub use reflect::{
    Class, Function, Property, PropertyKind, ScriptClassKind, ScriptObject, State, StructHeader,
    class_defaults, read_script_object, script_class_kind,
};
pub use value::{ObjRef, ObjectId, Ty, Value};
pub use vm::{
    ActorAnimation, AnimChannelState, BoneDirection, BoneState, ExternalObject, MoverState,
    NativeProfile, SpineControl, TraceEvent, TraceKind, Vm, VmError, VmErrorKind, VmLimits,
};
pub use voice::{FixedVoiceDuration, VoiceDuration};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod vm_tests;
