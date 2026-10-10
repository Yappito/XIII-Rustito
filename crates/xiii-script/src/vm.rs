//! Minimal UnrealScript interpreter (M2c second half).
//!
//! Interprets the decoded token trees of a [`ScriptSet`] directly. Objects have a class
//! layout (slots for every property of the class chain, static arrays expanded), defaults
//! from the class-default blocks and instance values from map exports. Functions run on the
//! Rust stack with a depth cap; state code runs in a per-object resumable state frame with
//! latent actions (`Sleep`). A fixed-step [`Vm::tick`] drives timers and state code with a
//! per-tick step budget.
//!
//! No filesystem access and no engine dependency: the caller loads packages into the set and
//! decides which actors are *active* (ticked). Direct script calls run independently of ticking.
//! The diagnostic harness can explicitly restrict calls to its selected scope; skipped calls
//! are traced, and return-valued calls fail. Unsupported tokens and unimplemented natives fail with
//! [`VmError`] carrying a script stack trace. Nothing is stubbed silently.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::time::Instant;

use xiii_package::{Limits, ObjectRef, PropertyBlock, PropertyValue, RawReason, StructValue};

use crate::animation::{AnimationData, SeqInfo};
use crate::bytecode::{Call, Context, Script, Token, TokenKind, opcode_name};
use crate::canvas::CanvasState;
use crate::events::{PresentationEvent, SoundEvent, TravelRequest, TravelSource};
use crate::external::ExternalObjectData;
use crate::linker::{GlobalRef, ScriptSet};
use crate::localize::{LocalizationData, placeholder};
#[cfg(test)]
use crate::navigation::move_step;
use crate::navigation::{
    NavEdgeInfo, NavPointInfo, NavigationData, find_path, nearest_point, point_fits,
};
use crate::physics::{HitZones, WorldHit, WorldPhysics};
use crate::reflect::{Property, PropertyKind, ScriptObject, function_flags, property_flags};
use crate::registry::{NativeCtx, NativeDef, NativeOutcome, Registry};
use crate::value::{Delegate, ObjRef, ObjectId, Ty, Value};

/// Interpreter limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmLimits {
    /// Statements + calls executed per tick (or per external call) before failing.
    pub max_steps: u64,
    /// Maximum script call depth.
    pub max_call_depth: usize,
    /// Maximum struct nesting when building types.
    pub max_type_depth: u32,
    /// Seed of the VM's deterministic PRNG (`Rand`/`FRand`). Default `0x9E3779B97F4A7C15`
    /// (the 64-bit golden-ratio constant); the engine's own RNG sequence is not reproduced.
    pub rng_seed: u64,
}

impl Default for VmLimits {
    fn default() -> Self {
        Self {
            max_steps: 1_000_000,
            // The engine's own script recursion limit: Core.dll `UObject::ProcessInternal`
            // increments the global runaway counter (VA 0x101939e4) per interpreted call and
            // compares against 0xFA = 250 (cmp at 0x101166e0); past 250 it logs
            // "Infinite script recursion (%i calls) detected" (string 0x10178cd8, message
            // pushed with the 250 constant at 0x101166f3). `GInitRunaway` (0x10115dc0) resets
            // the counter to 0. 250 frames at the measured ~4.4 KiB interpreter stack per
            // script frame (debug build) is ~1.1 MiB, which overflows the binary's 1 MiB
            // main thread, so the shipped VM-driving host entry points run on an explicit
            // 64 MiB stack (see xiii-app vmstack); the ~2 MiB test-thread default fits with
            // ~2x margin and needs no wrapper.
            // `vm_tests::recursion_guard_fits_a_2mib_stack` pins the property.
            max_call_depth: 250,
            max_type_depth: 16,
            rng_seed: 0x9E37_79B9_7F4A_7C15,
        }
    }
}

/// What went wrong while executing.
#[derive(Debug, Clone, PartialEq)]
pub enum VmErrorKind {
    /// A decoded token the interpreter does not implement.
    UnsupportedToken {
        /// Opcode.
        opcode: u8,
        /// Mnemonic.
        name: &'static str,
    },
    /// A native function without an implementation in the registry.
    UnimplementedNative {
        /// `Class.Function` of the declaration.
        path: String,
        /// Native index, if called by index.
        index: Option<u16>,
    },
    /// A native index with no declaring function.
    UnregisteredNative {
        /// Index.
        index: u16,
    },
    /// A native index declared by several functions that the argument count cannot separate.
    AmbiguousNative {
        /// Index.
        index: u16,
        /// Candidate paths.
        candidates: Vec<String>,
    },
    /// Step budget exhausted (runaway loop).
    BudgetExceeded {
        /// Limit.
        limit: u64,
    },
    /// Call depth limit hit.
    CallDepthExceeded {
        /// Limit.
        limit: usize,
    },
    /// A reference that does not resolve to a loaded object.
    Unresolved {
        /// Description.
        what: String,
    },
    /// Operand of the wrong type.
    TypeMismatch {
        /// Expected type.
        expected: &'static str,
        /// Found type.
        found: &'static str,
    },
    /// Array index out of range.
    ArrayIndex {
        /// Index.
        index: i64,
        /// Length.
        len: usize,
    },
    /// Jump target that is not a statement start.
    BadJumpTarget {
        /// Memory offset.
        offset: u32,
    },
    /// Reading a value the loader could not convert.
    UnsupportedValue {
        /// Description.
        desc: String,
    },
    /// An expression used as an assignment target that cannot be one.
    NotAPlace {
        /// Opcode.
        opcode: u8,
    },
    /// A latent native outside state code.
    LatentOutsideState {
        /// Native path.
        path: String,
    },
    /// A call into an inactive actor that needs a return value.
    DeferredWithReturnValue {
        /// Target object.
        target: String,
        /// Function.
        function: String,
    },
    /// `assert` failed.
    AssertionFailed {
        /// Source line.
        line: u16,
    },
    /// A native needing world collision ran without a physics provider set
    /// (with [`Vm::set_physics`]); never silently succeeds.
    NoPhysicsProvider {
        /// `Class.Function` of the native that needed it.
        native: String,
    },
    /// A native needing sequence data ran without an animation provider set
    /// (with [`Vm::set_animation_data`]); never silently succeeds.
    NoAnimationProvider {
        /// `Class.Function` of the native that needed it.
        native: String,
    },
    /// A pathing native needing the decoded navigation graph ran without a navigation provider
    /// set (with [`Vm::set_navigation`]); never silently succeeds.
    NoNavProvider {
        /// `Class.Function` of the native that needed it.
        native: String,
    },
    /// `Object.Localize` ran without a localisation provider set (with
    /// [`Vm::set_localization`]); never silently succeeds.
    NoLocalizationProvider {
        /// `Class.Function` of the native that needed it.
        native: String,
    },
    /// `Actor.WaveHasPosition` ran without a wave-position provider set (with
    /// [`Vm::set_wave_position`]); never silently succeeds (item48).
    NoAudioProvider {
        /// `Class.Function` of the native that needed it.
        native: String,
    },
    /// A sequence the animation provider does not know (and is not the `None` name).
    UnknownAnimation {
        /// Sequence name.
        sequence: String,
        /// Mesh path the sequence was looked up on.
        mesh: String,
    },
    /// The animation provider failed to decode/resolve a source (never treated as an unknown
    /// sequence).
    AnimationDataError {
        /// Source path the lookup was on.
        source: String,
        /// Sequence name.
        sequence: String,
        /// Provider message.
        message: String,
    },
    /// `new` was asked to construct an `Actor` (or subclass); upstream forbids that
    /// (actors are created with `Actor.Spawn`).
    NewOnActor {
        /// Class path.
        class: String,
    },
    /// State code ran past its last statement.
    StateCodeEnded,
    /// Virtual/global call with no matching function.
    NoSuchFunction {
        /// Object.
        object: String,
        /// Function name.
        name: String,
    },
    /// Division by zero.
    DivisionByZero,
    /// Anything else (description).
    Other(String),
}

/// One distinct native a script called that has no implementation, collected in survey mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingNative {
    /// `Class.Function` (or `#<index>` for an unregistered index).
    pub path: String,
    /// Declared native index, if any.
    pub index: Option<u16>,
    /// Calls seen.
    pub calls: u64,
    /// Script stack at the first call.
    pub first_stack: Vec<StackEntry>,
}

/// One stack-trace entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StackEntry {
    /// `Package.Class.Function` (or state).
    pub function: String,
    /// Object executing it.
    pub object: String,
    /// Memory offset of the current statement.
    pub offset: u32,
}

/// Interpreter failure with a script stack trace (innermost last).
#[derive(Debug, Clone, PartialEq)]
pub struct VmError {
    /// Failure.
    pub kind: VmErrorKind,
    /// Script stack at the failure.
    pub stack: Vec<StackEntry>,
}

impl fmt::Display for VmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "script error: {:?}", self.kind)?;
        for e in self.stack.iter().rev() {
            writeln!(
                f,
                "  at {} [{}] code 0x{:04X}",
                e.function, e.object, e.offset
            )?;
        }
        Ok(())
    }
}

impl std::error::Error for VmError {}

/// Result alias.
pub type VmResult<T> = Result<T, VmError>;

/// Level-start actor lifecycle order (`UGameEngine::LoadMap`): every actor runs each event
/// before the next event starts. Status/evidence in the report:
///
/// - `PreBeginPlay`, `BeginPlay`, `PostBeginPlay`, `PostNetBeginPlay` exist in `engine.u` and are
///   declared `event`; `SetInitialState` is a `simulated event` whose decoded body sets
///   `bScriptInitialized` and calls `GotoState`. That body is the evidence that it is the last
///   step of the lifecycle, not the first.
/// - The exact cross-actor grouping is not encoded in the retail packages; it follows the UE2
///   engine (community reconstruction) and is labelled a **hypothesis** here.
///
/// This one table is used by [`Vm::begin_play`] (level start, grouped) and is kept next to
/// [`RUNTIME_SPAWN_LIFECYCLE`] so the two orders cannot drift apart.
pub const LEVEL_START_LIFECYCLE: &[&str] = &[
    "PreBeginPlay",
    "BeginPlay",
    "PostBeginPlay",
    "PostNetBeginPlay",
    "SetInitialState",
];

/// Actor lifecycle run on a runtime `Actor.Spawn` (`ULevel::SpawnActor`): the spawned actor runs
/// these events in order. `Spawned` has no declaration in the GOG packages (no handler is a
/// no-op), so its presence is a hypothesis from the UE2 engine, recorded here explicitly.
///
/// All of these run **inside** `Spawn` before it returns to the caller (the caller's statements
/// after the `Spawn(...)` expression run afterwards). **Order measured in `Engine.dll`**
/// `ULevel::SpawnActor` (`XIII_Game/system/Engine.dll`, image base 0x10300000, export RVA
/// 0x88a20 -> VA 0x10388a20): `call eventPreBeginPlay` @0x10388dad, `call eventBeginPlay`
/// @0x10388db4, `call eventPostBeginPlay` @0x10388e6b, then `GetLevelInfo()` +
/// `cmpb $0x3, 0x410(%eax)` (`ALevelInfo.NetMode != NM_Client`, 0x10388e78) with a conditional
/// `call eventPostNetBeginPlay` @0x10388e83, then `call eventSetInitialState` @0x10388e8a.
/// `NetMode` is `NM_Standalone (0)` in single player, so `PostNetBeginPlay` runs; `PostBeginPlay`
/// runs before it. This matches UT2004 and the original hypothesis.
pub const RUNTIME_SPAWN_LIFECYCLE: &[&str] = &[
    "Spawned",
    "PreBeginPlay",
    "BeginPlay",
    "PostBeginPlay",
    "PostNetBeginPlay",
    "SetInitialState",
];

/// UE2 `CLASS_Abstract` class flag (stock Engine `0x00000001`). XIII narrows class flags to a
/// `u16`; no class in the GOG corpus sets bit 0, so the position is **unverified** and this
/// check never fires on retail data (documented in the report).
const CLASS_FLAG_ABSTRACT: u16 = 0x0001;

/// Native (C++) class default value that the serialized `defaultproperties` block cannot carry.
///
/// The VM reconstructs defaults from the tagged-property block of each `Core.Class` export; a
/// property whose value is set only in the native class constructor keeps its zero value. The
/// one case the corpus needs is `Engine.Camera` (`class Camera extends PlayerController native`,
/// measured 8 serialized editor-placement defaults, none of them `bOnlySpectator`).
///
/// Decoded evidence that this default is required for correct single-player startup:
/// `Engine.GameInfo.PostLogin` calls `StartMatch` when `bWaitingToStartMatch`, and
/// `Engine.GameInfo.StartMatch` calls `RestartPlayer(P)` for every `Level.ControllerList` entry
/// with `P.IsA('PlayerController') && P.Pawn == None && !PlayerController(P).bOnlySpectator`.
/// `PlayerController.Possess` early-returns on `bOnlySpectator` ("This controller is not allowed
/// to possess pawns", PlayerController.uc). The maps place 11 hidden `Engine.Camera` cutscene
/// controllers (`Camera.ScriptText`: "A camera, used in UnrealEd"); without this native default
/// each of them spawns a spurious `XIIIPlayerPawn` at the PlayerStart after login.
///
/// Keyed by lowercase short class name (matched anywhere in the class chain) and lowercase
/// property name. Returns `None` when there is no native default to apply.
fn native_class_default(class: &str, prop: &str) -> Option<Value> {
    match (class, prop) {
        ("camera", "bonlyspectator") => Some(Value::Bool(true)),
        _ => None,
    }
}

/// The retail licence-layer value of `MapInfo.TGSDummy`, keyed by the map's
/// `iLoadSpecificValue` (Xiii.dll native licence table, keyed by id - 26 into a 166-entry
/// selector; the seven non-default keys are the seven `xidmaps` MapInfo subclasses whose
/// scripts compare `TGSDummy`, each with the value its own code expects:
/// Hual01a 546, PRock01a 21627, Hual04a 856, Sanc02a 4, SSH101a 69, USA01 3589, SSH101c 703).
/// Measured: `?PostBeginPlay@AMapInfo@@UAEXXZ` VA 0x11b01640 in `XIII_Game/system/Xiii.dll`
/// writes offset 0x1f8 (`TGSDummy`) after the script `PostBeginPlay`; keys outside the
/// selector table keep the `xiii.MapInfo` default 0 (the demo build behaviour).
fn native_tgs_dummy(load_specific: i32) -> Option<i32> {
    match load_specific {
        26 => Some(546),
        55 => Some(21627),
        81 => Some(856),
        106 => Some(4),
        130 => Some(69),
        142 => Some(3589),
        191 => Some(703),
        _ => None,
    }
}

/// Latent action of a state frame.
#[derive(Debug, Clone, PartialEq)]
pub enum Latent {
    /// `Actor.Sleep`: remaining seconds.
    Sleep {
        /// Requested seconds.
        seconds: f32,
        /// Remaining seconds.
        remaining: f32,
        /// VM time when it started.
        started: f64,
    },
    /// `Actor.FinishAnim`: suspend until the channel's current animation ends.
    AnimEnd {
        /// Channel waited on.
        channel: u8,
        /// VM time when it started.
        started: f64,
    },
    /// `Controller.MoveTo`/`MoveToward`: latent horizontal movement of a pawn toward a point.
    Move {
        /// Pawn being moved.
        pawn: ObjectId,
        /// Destination in Unreal units (Z is left to world collision).
        destination: [f32; 3],
        /// Speed in Unreal units/second (`0` = the pawn's `GroundSpeed`).
        speed: f32,
        /// Remaining time budget in seconds; the latent ends when it drops below half a tick
        /// even if the pawn is blocked (upstream `MoveTo`'s `MoveTimer` fail-safe).
        remaining: f32,
        /// VM time when it started.
        started: f64,
        /// Native that started the latent (`Controller.MoveTo` or `Controller.MoveToward`).
        native: &'static str,
    },
    /// `Actor.FinishInterpolation`: suspend until the actor's `bInterpolating` clears. The
    /// per-tick `PHYS_MovingBrush` advance that clears it lives in
    /// [`Vm::advance_interpolation`] (a `Mover`'s `InterpolateTo` sets `bInterpolating`).
    Interp {
        /// VM time when it started.
        started: f64,
    },
    /// `Controller.FinishRotation`: wait until its pawn has reached Focus/FocalPoint.
    Rotation {
        /// VM time when it started.
        started: f64,
    },
    /// `Controller.WaitForLanding`: wait for a non-null pawn to stop falling.
    Landing {
        /// Countdown to repeated `LongFall` callbacks (not a release timeout).
        remaining: f32,
        /// VM time when it started.
        started: f64,
    },
}

/// An object reference into a package outside the loaded script set (e.g. a `Sound` in a
/// `.uax`). Interned by the VM so [`ObjRef::External`] stays `Copy` and equality is by path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExternalObject {
    /// Full `Package.Outer.Object` path as the referencing package spells it.
    pub path: String,
    /// Class path recorded for the reference: from the referencing package's import table, or
    /// from the external package's own export when the runtime registered and verified it.
    pub class: Option<String>,
}

/// Intern table for [`ExternalObject`]s. Equality by path is guaranteed because a path maps to
/// one id; the class is taken from the first resolution of that path.
#[derive(Debug, Default)]
struct ExternalTable {
    list: Vec<ExternalObject>,
    index: HashMap<String, u32>,
}

/// Current state of a UE2 `Mover` (`PHYS_MovingBrush`) actor, for the host's dynamic collision
/// and diagnostics. Unreal units/rotators, exactly as stored in the VM.
#[derive(Debug, Clone, PartialEq)]
pub struct MoverState {
    /// Display name of the actor.
    pub name: String,
    /// Current `Location` (Unreal units).
    pub location: [f32; 3],
    /// Current `Rotation` (Unreal rotator units).
    pub rotation: [i32; 3],
    /// `BasePos` the keys are relative to (Unreal units).
    pub base_pos: [f32; 3],
    /// `BaseRot` the keys are relative to.
    pub base_rot: [i32; 3],
    /// Current key index.
    pub key_num: u8,
    /// Interpolation fraction `0..=1`.
    pub phys_alpha: f32,
    /// Interpolation rate (1/seconds).
    pub phys_rate: f32,
    /// True while a `PHYS_MovingBrush` interpolation is in progress.
    pub interpolating: bool,
    /// `bCollideActors && bBlockPlayers`: the mover currently blocks the player pawn.
    pub blocks_players: bool,
}

/// Per-channel animation playback state owned by the VM.
#[derive(Debug, Clone)]
pub(crate) struct AnimChannel {
    /// Sequence name this channel is playing.
    pub(crate) sequence: String,
    /// Animation source the sequence was resolved from (a `LinkSkelAnim` path or the actor's
    /// `Mesh`); posed-bone queries address the same data through it.
    pub(crate) source: String,
    /// Total frames.
    pub(crate) frames: u32,
    /// Playback rate (frames/second).
    pub(crate) rate: f32,
    /// Current position in frames.
    pub(crate) frame: f32,
    /// Loop at the sequence boundary; `AnimEnd` is sent at its final frame.
    pub(crate) looping: bool,
    /// Still playing.
    pub(crate) active: bool,
    /// Seconds still to be spent tweening in before playback advances.
    pub(crate) tween_remaining: f32,
    pub(crate) tween_duration: f32,
    pub(crate) tween_source: Option<Box<AnimChannelState>>,
    pub(crate) tween_only: bool,
    pub(crate) loop_end_sent: bool,
    /// Script notifies as `(time01, function)`.
    pub(crate) notifies: Vec<(f32, String)>,
    /// Index of the next notify not yet fired.
    pub(crate) notify_idx: usize,
}

/// Per-actor animation state: the animation sources and one channel per `Channel` argument.
#[derive(Debug, Clone, Default)]
pub(crate) struct AnimState {
    /// `MeshAnimation` object paths linked with `Actor.LinkSkelAnim`, in call order.
    pub(crate) linked_anims: Vec<String>,
    /// Channels by index (ordered for deterministic notifies/traces).
    pub(crate) channels: std::collections::BTreeMap<u8, AnimChannel>,
    /// `Actor.AnimBlendParams` parameters by blend stage/channel (nil `Entry` = no call).
    pub(crate) blend_params: std::collections::BTreeMap<i32, AnimBlendParams>,
}

/// `Actor.AnimBlendParams` blending parameters for one animation stage/channel.
///
/// The decoded XIII signature has no `bGlobalPose` argument (unlike UT2003/2004); the values
/// are stored per channel exactly as passed. `bone_name` is the optional bone filter: upstream
/// applies the blend only to that bone's subtree, which the VM does not model (the native is
/// registered `Partial` for that reason).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AnimBlendParams {
    /// Blend amount (`0` = lower channel, `1` = this channel).
    pub(crate) blend_alpha: f32,
    /// Seconds to interpolate the blend in.
    pub(crate) in_time: f32,
    /// Seconds to interpolate the blend out.
    pub(crate) out_time: f32,
    /// Bone filter (`None` = `BoneName` was omitted or `None`).
    pub(crate) bone_name: Option<String>,
    pub(crate) alpha_target: Option<(f32, f32)>,
}

/// `Pawn.SpineYawControl(bool IsControlled, int MaxValue, float RotationSpeed)` parameters.
///
/// The engine sets a bit in the pawn's native flags word and stores the two values for the
/// skeletal-mesh bone controller. The headless VM keeps the same parameters per actor; no
/// skeletal pose is computed (the native is registered `Partial` for that reason).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpineControl {
    /// `IsControlled`: the spine yaw controller is active.
    pub is_controlled: bool,
    /// `MaxValue`: maximum controller value (engine units).
    pub max_value: i32,
    /// `RotationSpeed`: seconds-ish rate passed through unchanged.
    pub rotation_speed: f32,
}

/// `Actor.SetBoneDirection(name BoneName, rotator BoneTurn, vector BoneTrans, float Alpha)`.
///
/// The engine forwards the request to the skeletal-mesh instance's bone controller. The headless
/// VM stores the request per actor in call order; no skeletal transform is evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct BoneDirection {
    /// Target bone.
    pub bone: String,
    /// Bone rotation offset.
    pub turn: [i32; 3],
    /// Bone translation offset.
    pub trans: [f32; 3],
    /// Blend alpha.
    pub alpha: f32,
}

/// `Actor.SetBoneScalePerAxis(int Slot, float X, float Y, float Z, name BoneName)`.
///
/// The engine forwards the request to the skeletal-mesh instance's bone controller
/// (`?SetBoneScale@USkeletalMeshInstance`). The headless VM stores the request per actor in call
/// order; no skeletal transform is evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct BoneScale {
    /// Bone-controller slot.
    pub slot: i32,
    /// Per-axis scale (X, Y, Z); omitted optional axes default to 1.0 in the engine.
    pub scale: [f32; 3],
    /// Target bone.
    pub bone: String,
}

/// `Actor.SetBoneRotation(name BoneName, rotator BoneTurn, int Space, float Alpha)`.
///
/// The engine forwards the request to the skeletal-mesh instance's bone controller. The headless
/// VM stores the request per actor in call order; no skeletal transform is evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct BoneRotation {
    /// Target bone.
    pub bone: String,
    /// Bone rotation offset.
    pub turn: [i32; 3],
    /// Rotation space (engine value; `EX_Nothing` omitted argument reads as 0).
    pub space: i32,
    /// Blend alpha.
    pub alpha: f32,
}

/// `Actor.SetBoneLocation(name BoneName, vector BoneTrans, float Alpha)`.
///
/// The engine forwards the request to the skeletal-mesh instance's bone controller. The headless
/// VM stores the request per actor in call order; no skeletal transform is evaluated.
#[derive(Debug, Clone, PartialEq)]
pub struct BoneLocation {
    /// Target bone.
    pub bone: String,
    /// Bone translation offset.
    pub trans: [f32; 3],
    /// Blend alpha.
    pub alpha: f32,
}

/// Per-actor bone-control state set by `Pawn.SpineYawControl` / `Actor.SetBoneDirection` /
/// `Actor.SetBoneScalePerAxis` / `Actor.SetBoneRotation` / `Actor.SetBoneLocation`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoneState {
    /// Latest `Pawn.SpineYawControl` parameters, if the native ran.
    pub spine: Option<SpineControl>,
    /// `Actor.SetBoneDirection` requests, in call order.
    pub directions: Vec<BoneDirection>,
    /// `Actor.SetBoneScalePerAxis` requests, in call order.
    pub scales: Vec<BoneScale>,
    /// `Actor.SetBoneRotation` requests, in call order.
    pub rotations: Vec<BoneRotation>,
    /// `Actor.SetBoneLocation` requests, in call order.
    pub locations: Vec<BoneLocation>,
}

/// Read-only view of one animation channel, for a host renderer that samples the decoded
/// `MeshAnimation` at the VM's own playback position. Frames are in animation frames (the
/// provider's unit), matching `Actor.AnimFrame`.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimChannelState {
    /// `Channel` argument the sequence was started on.
    pub channel: u8,
    /// Sequence name the channel is playing.
    pub sequence: String,
    /// Current position in frames.
    pub frame: f32,
    /// Playback rate (frames/second).
    pub rate: f32,
    /// Total frames of the sequence.
    pub frames: u32,
    /// Whether the sequence loops (AnimEnd at the final frame, wrap one frame later).
    pub looping: bool,
    /// Still advancing (false once a non-looping sequence ended).
    pub active: bool,
    /// Frozen previous channel pose (one cached level, like the engine's per-channel cache;
    /// the frozen state's own `tween_source` is always `None`).
    pub tween_source: Option<Box<AnimChannelState>>,
    /// Seconds of tweening left and initial duration.
    pub tween_remaining: f32,
    /// Initial tween duration.
    pub tween_duration: f32,
    /// TweenAnim holds the target frame after completion.
    pub tween_only: bool,
    /// Current alpha of this channel (channel zero is authoritative).
    pub blend_alpha: f32,
    /// Blend-in fraction of sequence length, as decoded in GetFrame.
    pub blend_in: f32,
    /// Stored OutTime; no use was found in this PC GetFrame path.
    pub blend_out: f32,
    /// First bone of the blended subtree.
    pub blend_bone: Option<String>,
}

/// Read-only per-actor animation view for the host: the candidate animation sources (the
/// `LinkSkelAnim` links then the `Mesh`) and every channel, ordered by channel index.
#[derive(Debug, Clone, PartialEq)]
pub struct ActorAnimation {
    /// Animation-source object paths, in the order the VM tries them.
    pub sources: Vec<String>,
    /// Channels, ordered by channel index.
    pub channels: Vec<AnimChannelState>,
}

/// One trace record.
#[derive(Debug, Clone, PartialEq)]
pub struct TraceEvent {
    /// Tick number (0 = before the first tick).
    pub tick: u64,
    /// VM time in seconds.
    pub time: f64,
    /// What happened.
    pub kind: TraceKind,
}

/// Trace record kinds.
#[derive(Debug, Clone, PartialEq)]
pub enum TraceKind {
    /// Engine/harness-delivered event or a script call into another object's script.
    Event {
        /// Target object.
        target: String,
        /// Function path.
        function: String,
        /// Argument values.
        args: Vec<String>,
    },
    /// Native call.
    Native {
        /// `Class.Function`.
        path: String,
        /// Index if called by index.
        index: Option<u16>,
        /// Object.
        this: String,
        /// Arguments.
        args: Vec<String>,
        /// Result.
        result: String,
    },
    /// State transition.
    StateChange {
        /// Object.
        actor: String,
        /// Old state.
        from: Option<String>,
        /// New state.
        to: Option<String>,
        /// Label where state code resumes (none: no state code).
        label: Option<String>,
    },
    /// `GotoState` to a state the class does not have.
    StateNotFound {
        /// Object.
        actor: String,
        /// Requested state.
        state: String,
    },
    /// Latent action started.
    LatentStart {
        /// Object.
        actor: String,
        /// Native.
        native: String,
        /// Seconds.
        seconds: f32,
    },
    /// Latent action finished; state code resumes.
    LatentResume {
        /// Object.
        actor: String,
        /// Native.
        native: String,
        /// When it started.
        started: f64,
    },
    /// Iterator results.
    Iterator {
        /// Native.
        native: String,
        /// Objects produced.
        found: Vec<String>,
    },
    /// Call into an inactive (out-of-scope) actor, not executed.
    Deferred {
        /// Target object.
        target: String,
        /// Target class.
        class: String,
        /// Function path.
        function: String,
    },
    /// Event dropped because the probe is disabled.
    ProbeDisabled {
        /// Object.
        actor: String,
        /// Probe name.
        probe: String,
    },
    /// Event with no handler in the class.
    NoHandler {
        /// Object.
        actor: String,
        /// Event name.
        event: String,
    },
    /// Member access through `None` (UE2 logs and yields zero).
    AccessedNone {
        /// Function.
        function: String,
        /// Code offset.
        offset: u32,
    },
    /// State code reached `stop`.
    StateStop {
        /// Object.
        actor: String,
    },
    /// Script `Log`.
    Log(String),
    /// Timer fired.
    Timer {
        /// Object.
        actor: String,
    },
    /// `Actor.Spawn` created a new actor.
    Spawned {
        /// New object.
        actor: String,
        /// Requested class.
        class: String,
    },
    /// `Actor.Destroy` deleted an object.
    Destroyed {
        /// Object.
        actor: String,
        /// Return value.
        result: bool,
    },
    /// `Actor.Spawn` was asked for a class it refuses (None or abstract).
    SpawnRefused {
        /// Why.
        reason: String,
    },
    /// GameInfo selected at level start.
    GameInfo {
        /// Object.
        actor: String,
        /// Class path.
        class: String,
    },
    /// `new` constructed a non-actor object.
    NewObject {
        /// New object.
        object: String,
        /// Class path.
        class: String,
    },
    /// An animation channel reached the end of a non-looping sequence.
    AnimEnd {
        /// Object.
        actor: String,
        /// Channel.
        channel: u8,
    },
    /// A script notify fired during animation playback.
    AnimNotify {
        /// Object.
        actor: String,
        /// Notify function.
        function: String,
        /// Channel.
        channel: u8,
    },
    /// Latent `Actor.FinishAnim` suspended state code until the channel ended.
    AnimSuspend {
        /// Object.
        actor: String,
        /// Native path.
        native: String,
    },
    /// Harness note.
    Note(String),
}

impl fmt::Display for TraceEvent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[t{:>3} {:>7.3}s] ", self.tick, self.time)?;
        match &self.kind {
            TraceKind::Event {
                target,
                function,
                args,
            } => write!(f, "EVENT    {target}.{function}({})", args.join(", ")),
            TraceKind::Native {
                path,
                index,
                this,
                args,
                result,
            } => {
                let idx = index.map(|i| format!("#{i} ")).unwrap_or_default();
                write!(
                    f,
                    "NATIVE   {idx}{path}({}) on {this} -> {result}",
                    args.join(", ")
                )
            }
            TraceKind::StateChange {
                actor,
                from,
                to,
                label,
            } => write!(
                f,
                "STATE    {actor}: {} -> {} (code at {})",
                from.as_deref().unwrap_or("<none>"),
                to.as_deref().unwrap_or("<none>"),
                label.as_deref().unwrap_or("<none>")
            ),
            TraceKind::StateNotFound { actor, state } => {
                write!(
                    f,
                    "STATE    {actor}: GotoState('{state}') not found, unchanged"
                )
            }
            TraceKind::LatentStart {
                actor,
                native,
                seconds,
            } => write!(
                f,
                "LATENT   {actor}: {native}({seconds:?}) suspends state code"
            ),
            TraceKind::LatentResume {
                actor,
                native,
                started,
            } => write!(
                f,
                "RESUME   {actor}: {native} started at {started:.3}s finished"
            ),
            TraceKind::Iterator { native, found } => {
                write!(f, "ITER     {native} -> [{}]", found.join(", "))
            }
            TraceKind::Deferred {
                target,
                class,
                function,
            } => write!(
                f,
                "DEFERRED {target} ({class}).{function}: actor not in the executed scope (unsupported, not run)"
            ),
            TraceKind::ProbeDisabled { actor, probe } => {
                write!(f, "PROBE    {actor}.{probe} disabled, event dropped")
            }
            TraceKind::NoHandler { actor, event } => {
                write!(f, "EVENT    {actor}.{event}: no handler")
            }
            TraceKind::AccessedNone { function, offset } => {
                write!(f, "WARN     accessed None in {function} at 0x{offset:04X}")
            }
            TraceKind::StateStop { actor } => write!(f, "STOP     {actor}: state code stopped"),
            TraceKind::Log(s) => write!(f, "LOG      {s}"),
            TraceKind::Timer { actor } => write!(f, "TIMER    {actor}.Timer"),
            TraceKind::Spawned { actor, class } => {
                write!(f, "SPAWN    {actor} = Spawn({class})")
            }
            TraceKind::Destroyed { actor, result } => {
                write!(f, "DESTROY  {actor} -> {result}")
            }
            TraceKind::SpawnRefused { reason } => write!(f, "SPAWN    refused: {reason}"),
            TraceKind::GameInfo { actor, class } => {
                write!(f, "GAMEINFO {actor} ({class})")
            }
            TraceKind::NewObject { object, class } => {
                write!(f, "NEW      {object} = New({class})")
            }
            TraceKind::AnimEnd { actor, channel } => {
                write!(f, "ANIMEND  {actor}: channel {channel} finished")
            }
            TraceKind::AnimNotify {
                actor,
                function,
                channel,
            } => write!(f, "NOTIFY   {actor}.{function}(channel {channel})"),
            TraceKind::AnimSuspend { actor, native } => {
                write!(f, "LATENT   {actor}: {native} suspends state code")
            }
            TraceKind::Note(s) => write!(f, "NOTE     {s}"),
        }
    }
}

/// One property slot of a class or function layout.
#[derive(Debug, Clone)]
pub struct Slot {
    /// Property object.
    pub prop: GlobalRef,
    /// Lowercase name.
    pub name: String,
    /// Type.
    pub ty: Ty,
    /// Static array dimension.
    pub dim: usize,
    /// First slot index.
    pub base: usize,
    /// Property flags.
    pub flags: u32,
    /// Class that declares the property (for a class layout) or the function that declares the
    /// parameter (for a function layout). Used to resolve `localized` class defaults from the
    /// declaring class package's `.int`.
    pub declaring: GlobalRef,
}

/// Slots of a class chain plus defaults.
#[derive(Debug)]
pub struct ClassLayout {
    /// Class.
    pub class: GlobalRef,
    /// Class chain, most derived first.
    pub chain: Vec<GlobalRef>,
    /// Lowercase class names of the chain.
    pub chain_names: Vec<String>,
    /// Slots.
    pub slots: Vec<Slot>,
    by_prop: HashMap<GlobalRef, usize>,
    by_name: HashMap<String, usize>,
    /// Total values.
    pub size: usize,
    /// Default values (class-default blocks applied root to leaf).
    pub defaults: Vec<Value>,
    /// Precomputed `props` index of `Location` (no per-read name lookup on hot paths).
    pub location_slot: Option<usize>,
    /// Precomputed `props` index of `Rotation`.
    pub rotation_slot: Option<usize>,
    /// Precomputed `props` index of `bCollideActors`.
    pub collide_slot: Option<usize>,
    /// Precomputed `props` index of `bInterpolating`.
    pub interp_slot: Option<usize>,
    /// True when the class chain derives from `Mover` (avoids a chain scan per object per tick).
    pub is_mover_class: bool,
}

impl ClassLayout {
    /// Slot by lowercase property name.
    ///
    /// Pure caching optimisation with no semantic change: instead of allocating a lowercase
    /// `String` for every lookup (this is on the per-actor touch/sync hot paths), short ASCII
    /// names are lowercased into a fixed stack buffer. Non-ASCII or over-long names keep the
    /// original allocating path.
    pub fn slot_by_name(&self, name: &str) -> Option<&Slot> {
        let bytes = name.as_bytes();
        if bytes.is_ascii() && bytes.len() <= STACK_NAME_BYTES {
            let mut buf = [0u8; STACK_NAME_BYTES];
            for (i, b) in bytes.iter().enumerate() {
                buf[i] = b.to_ascii_lowercase();
            }
            let key = std::str::from_utf8(&buf[..bytes.len()]).unwrap_or(name);
            self.by_name.get(key).map(|i| &self.slots[*i])
        } else {
            self.by_name
                .get(&name.to_ascii_lowercase())
                .map(|i| &self.slots[*i])
        }
    }
}

/// Stack buffer size for the allocation-free property-name lowercase path. Property names in the
/// corpus are far shorter than this; longer names fall back to the allocating path.
const STACK_NAME_BYTES: usize = 64;

#[derive(Debug)]
struct ParamInfo {
    slot: usize,
    out: bool,
    ty: Ty,
}

#[derive(Debug)]
struct FuncLayout {
    slots: Vec<Slot>,
    by_prop: HashMap<GlobalRef, usize>,
    params: Vec<ParamInfo>,
    ret: Option<(usize, Ty)>,
    size: usize,
}

#[derive(Debug, Clone)]
struct StateCode {
    /// Script holding the code (the state or a super state with the label).
    owner: GlobalRef,
    /// Statement index.
    pc: usize,
    latent: Option<Latent>,
}

#[derive(Debug, Clone)]
struct Timer {
    rate: f32,
    remaining: f32,
    repeat: bool,
}

/// Event dispatched by timer slot 0/1/2 (UE2 `SetTimer`, `SetTimer2`, `Controller.SetTimer3`).
const TIMER_EVENTS: [&str; 3] = ["Timer", "Timer2", "Timer3"];
/// UE2 `EPhysics::PHYS_Walking` (engine.u enum order; see `item7b-movement-modes.md`).
const PHYS_WALKING: u8 = 1;
/// UE2 `EPhysics::PHYS_Falling` (item53: `AActor::performPhysics` jump table 0x103c14dc).
const PHYS_FALLING: u8 = 2;
/// UE2 `EPhysics::PHYS_Flying` (the grapple demo's `Jones.SetPhysics(4)`).
const PHYS_FLYING: u8 = 4;
/// UE2 `EPhysics::PHYS_Projectile` (the grapple demo's `CineHook` class default).
const PHYS_PROJECTILE: u8 = 6;

fn cine_trace_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var_os("XIII_CINE_TRACE").is_some_and(|v| v != "0" && !v.is_empty())
    })
}

/// An interpreter object.
#[derive(Debug)]
pub struct Instance {
    /// Class.
    pub class: GlobalRef,
    /// Display name.
    pub name: String,
    /// Property values.
    pub props: Vec<Value>,
    pub(crate) layout: Rc<ClassLayout>,
    /// Current state.
    pub state: Option<GlobalRef>,
    state_code: Option<StateCode>,
    generation: u64,
    disabled: HashSet<String>,
    /// `Destroy` is running its `Destroyed` event right now (`bDeleteMe` is not set yet). The
    /// engine sets this before the event: `ULevel::DestroyActor` clears the state and calls
    /// `AActor::ProcessEvent` for `Destroyed` only while `!bDeleteMe`. `Vm::destroy` uses it to
    /// make a nested `Destroy` a no-op (the engine returns 1 with `bDeleteMe` set by then) while
    /// still keeping the actor readable/writable during its own `Destroyed`.
    destroying: bool,
    /// All probes of the actor were disabled (`AActor+0x34` bit 0x1, `bProbesDisabled`). Read from
    /// the serialized property `bProbesDisabled` at spawn/layout time; `Disable`/`Enable` update it.
    probes_disabled: bool,
    /// Receives scheduled ticks, state code and timers. Does not gate direct script calls.
    pub active: bool,
    /// Scheduled execution suspended after a script error. Direct calls still execute and
    /// propagate any error; suspension never supplies a successful replacement result.
    pub suspended: bool,
    /// Derives from `Actor`.
    pub is_actor: bool,
    /// Destroyed: behaves as `None` for further references.
    pub deleted: bool,
    /// Memoised `Tick` dispatch for this instance, keyed by the state it was resolved in.
    ///
    /// `None` until the first `Tick` lookup; `Some((state, function))` afterwards. The class
    /// chain is fixed for the instance's life, so the entry stays valid while `state` is
    /// unchanged; `do_goto_state` clears it when the state changes (the only input the lookup
    /// depends on), so a stale entry is impossible. This turns the per-actor, per-frame
    /// state+class function scan into a single `Option` comparison in the steady state.
    tick_fn: Option<(Option<GlobalRef>, Option<GlobalRef>)>,
    /// Map export it was loaded from.
    pub export: Option<GlobalRef>,
    /// The three UE2 actor timers (`Timer`, `Timer2`, `Timer3`), independently scheduled.
    timers: [Option<Timer>; 3],
    /// Animation channels (actor animation natives).
    pub(crate) anim: AnimState,
    /// Bone-control parameters (skeletal natives); no skeletal pose yet.
    pub bone: BoneState,
}

#[derive(Debug, Clone)]
enum Place {
    Local(usize),
    Slot(ObjectId, usize),
    /// Dynamic-array element. The third field is the declared element type, used to
    /// initialise elements grown by an out-of-range assignment (UE2 zeroes new elements to the
    /// element type's default, e.g. a zero struct with all its members, not an `int 0`).
    Elem(Box<Place>, usize, Option<Ty>),
    Member(Box<Place>, String),
    /// A dynamic array's `Length` (UE2 `Array.Length = n` resizes the array). The second field is
    /// the declared element type, used to zero the grown elements.
    ArrayLen(Box<Place>, Option<Ty>),
}

struct IterState {
    items: Vec<Vec<Value>>,
    idx: usize,
    places: Vec<Option<Place>>,
    body: usize,
    /// TouchingActors walks the current native array, not a snapshot. The cursor is
    /// advanced before the body runs, matching execTouchingActors 0x103e66cf.
    touching: Option<(ObjectId, Option<GlobalRef>, usize)>,
}

type ActorTraceHit = (ObjectId, [f32; 3], [f32; 3]);

struct Frame<'s> {
    pkg: usize,
    this: ObjectId,
    locals: Vec<Value>,
    layout: Option<Rc<FuncLayout>>,
    script: &'s Script,
    map: Rc<HashMap<u32, usize>>,
    iters: Vec<IterState>,
    state_of: Option<ObjectId>,
    pending_latent: Option<(Latent, String)>,
}

enum Flow {
    Next,
    Goto(usize),
    Return(Value),
    Stop,
    Latent,
}

enum Exit {
    Return(Value),
    End,
    Stop,
    Latent(usize),
    Restart,
}

/// Optional VM profiling data (`Vm::enable_native_timers`). Cheap when disabled: the timing
/// guards only run when `enabled` is set, and the accumulators are only written then.
#[derive(Debug, Default, Clone)]
pub struct NativeProfile {
    /// Whether the timers are armed.
    pub enabled: bool,
    /// Per-native cumulative wall-clock microseconds, keyed by `Class.Function`.
    pub micros: BTreeMap<String, u64>,
    /// Per-native call counts.
    pub calls: BTreeMap<String, u64>,
    /// Cumulative microseconds in the timer loop of `tick_suspending`.
    pub timers_micros: u64,
    /// Cumulative microseconds in the animation-advance loop.
    pub animation_micros: u64,
    /// Cumulative microseconds in the mover-interpolation loop.
    pub movers_micros: u64,
    /// Cumulative microseconds in the state-code loop.
    pub state_micros: u64,
    /// Cumulative microseconds in the per-frame `Tick` dispatch loop of `tick_suspending`
    /// (the pass that resolves and calls `Tick` on every active actor).
    pub tick_dispatch_micros: u64,
    /// Cumulative microseconds in the per-frame `PlayerTick` dispatch loop.
    pub player_tick_dispatch_micros: u64,
    /// Cumulative microseconds in the scripted-physics integration pass (item53).
    pub scripted_physics_micros: u64,
    /// Cumulative microseconds inside per-frame `Tick`/`PlayerTick` executions, keyed by
    /// `Class.Function` (the dispatch loop's [`Vm::call_values`] time; natives inside are
    /// additionally counted in `micros`).
    pub tick_fns: BTreeMap<String, u64>,
    /// `Tick` resolution attempts and the subset served from the per-instance memo.
    pub tick_lookups: u64,
    /// `Tick` resolutions served from the per-instance memo (no state/class scan).
    pub tick_cache_hits: u64,
    /// `Vm::spawn` calls (per-frame object churn diagnostic).
    pub spawn_count: u64,
    /// `objects` `Vec` capacity growths inside `Vm::spawn` (per-frame reallocation diagnostic).
    pub objects_reallocs: u64,
    /// Cumulative microseconds inside native implementations (sum over all natives).
    pub natives_micros: u64,
    /// Cumulative microseconds writing the host-owned player fields into the VM.
    pub player_write_micros: u64,
    /// Cumulative microseconds in `refresh_touching_of` (the host moved the player).
    pub touch_micros: u64,
    /// Cumulative microseconds draining presentation events / updating touches.
    pub events_micros: u64,
    /// Cumulative microseconds in the one-way render sync (`update_sync`).
    pub sync_micros: u64,
    /// Cumulative microseconds in the host AI-perception pass (`update_ai_perception`, including
    /// the `SeePlayer`/`EnemyNotVisible` dispatch it performs).
    pub perception_micros: u64,
    /// Cumulative microseconds building mover collision states (`mover_states`).
    pub mover_states_micros: u64,
}

impl NativeProfile {
    /// Clears every accumulator (keeps `enabled`).
    pub fn reset(&mut self) {
        self.micros.clear();
        self.calls.clear();
        self.timers_micros = 0;
        self.animation_micros = 0;
        self.movers_micros = 0;
        self.state_micros = 0;
        self.tick_dispatch_micros = 0;
        self.player_tick_dispatch_micros = 0;
        self.scripted_physics_micros = 0;
        self.tick_fns.clear();
        self.tick_lookups = 0;
        self.tick_cache_hits = 0;
        self.spawn_count = 0;
        self.objects_reallocs = 0;
        self.natives_micros = 0;
        self.player_write_micros = 0;
        self.touch_micros = 0;
        self.events_micros = 0;
        self.sync_micros = 0;
        self.mover_states_micros = 0;
        self.perception_micros = 0;
    }
}

/// `HearNoise` probe bit: Engine.dll passes `FName(EName 0x156)` (342) to `IsProbing`, and
/// Core.dll `UObject::IsProbing` maps probe names 300..363 to mask bit `name - 300` (item41c).
const HEAR_NOISE_PROBE_BIT: u32 = 342 - 300;

/// Squared distance of two float vectors evaluated in double precision, standing in for the
/// x87 extended-precision `FVector::SizeSquared` the engine computes before storing or comparing.
fn dist_sq_f64(a: [f32; 3], b: [f32; 3]) -> f64 {
    let d = |i: usize| f64::from(a[i]) - f64::from(b[i]);
    d(0) * d(0) + d(1) * d(1) + d(2) * d(2)
}

/// Engine.dll 0x103db640 (upstream name `FSortedPathList::addPath`), used by `CanHear`'s
/// around-corner branch: up to 32 nodes in ascending key order. The insertion point comes from a
/// coarse binary step (count > 8: half, count > 16: an extra quarter step) followed by a linear
/// scan to the first key `>=` the new one; when full, the last entry falls off. Every entry
/// shifted down one slot has its key truncated to an integer (the decoded `_ftol` + `fild`).
#[derive(Debug, Default)]
struct SortedPathList {
    nodes: [ObjectId; 32],
    dist: [f32; 32],
    count: usize,
}

impl SortedPathList {
    fn add(&mut self, node: ObjectId, key: f32) {
        let n = self.count;
        let mut i = 0;
        if n > 8 {
            let half = n / 2;
            let step = if key > self.dist[half] {
                i = half;
                (n > 16).then_some(n / 4 + half)
            } else {
                (n > 16).then_some(n / 4)
            };
            if let Some(j) = step
                && key > self.dist[j]
            {
                i = j;
            }
        }
        while i < n && key > self.dist[i] {
            i += 1;
        }
        if i >= 32 {
            return;
        }
        let mut moved_node = self.nodes[i];
        let mut moved_dist = self.dist[i];
        self.nodes[i] = node;
        self.dist[i] = key;
        if self.count < 32 {
            self.count += 1;
        }
        i += 1;
        while i < self.count {
            let (next_node, next_dist) = (self.nodes[i], self.dist[i]);
            self.nodes[i] = moved_node;
            // `_ftol` truncates toward zero; only the low 32 bits are reloaded with `fild`.
            self.dist[i] = (moved_dist as i64) as i32 as f32;
            moved_node = next_node;
            moved_dist = next_dist;
            i += 1;
        }
    }
}

/// The interpreter.
pub struct Vm<'s> {
    set: &'s ScriptSet,
    /// Objects.
    pub objects: Vec<Instance>,
    by_export: HashMap<GlobalRef, ObjectId>,
    layouts: HashMap<GlobalRef, Rc<ClassLayout>>,
    func_layouts: HashMap<GlobalRef, Rc<FuncLayout>>,
    stmt_maps: HashMap<GlobalRef, Rc<HashMap<u32, usize>>>,
    default_objects: HashMap<GlobalRef, ObjectId>,
    registry: Registry,
    /// Trace records.
    pub trace: Vec<TraceEvent>,
    /// Record native calls in the trace.
    pub trace_natives: bool,
    stack: Vec<StackEntry>,
    /// Ticks run.
    pub tick_count: u64,
    /// VM time in seconds.
    pub time: f64,
    steps: u64,
    limits: VmLimits,
    /// Properties the loader could not place (name not in layout, bad index).
    pub load_warnings: Vec<String>,
    rng: u64,
    /// Native functions called (path -> (index, count)).
    pub natives_used: std::collections::BTreeMap<String, (Option<u16>, u64)>,
    /// First actual script stack for each invoked native (bounded by native paths).
    pub natives_first_caller: BTreeMap<String, Vec<StackEntry>>,
    /// Calls reached under weapon Fire or AI NotifyFiring, for opt-in combat diagnostics.
    pub combat_natives: BTreeMap<String, (u64, Vec<StackEntry>)>,
    /// Opt-in collection of firing-path call stacks (off during ordinary play).
    pub collect_combat_natives: bool,
    /// Survey mode: unimplemented natives are counted and skipped instead of failing.
    pub survey: bool,
    /// Distinct unimplemented natives seen in survey mode (path -> record, first-hit order).
    pub missing_natives: std::collections::BTreeMap<String, MissingNative>,
    pub(crate) pending_latent: Option<Latent>,
    /// World-collision provider (movement/trace natives). `None` = every collision native
    /// fails with [`VmErrorKind::NoPhysicsProvider`].
    pub(crate) physics: Option<Box<dyn WorldPhysics>>,
    /// Last mover collision state sent to `physics` ([`Vm::sync_mover_collision`]).
    mover_collision_sent: HashMap<ObjectId, bool>,
    /// Animation-sequence provider (animation natives). `None` = every native that needs
    /// sequence data fails with [`VmErrorKind::NoAnimationProvider`].
    pub(crate) animation: Option<Box<dyn AnimationData>>,
    /// Decoded navigation graph (pathing natives). `None` = every native that needs it fails
    /// with [`VmErrorKind::NoNavProvider`].
    pub(crate) navigation: Option<Box<dyn NavigationData>>,
    /// Hit-zone provider for `Actor.GetLastTraceBone` (item14). `None` = the default
    /// [`crate::physics::CylinderZones`] is used.
    hit_zones: Option<Box<dyn HitZones>>,
    /// Partial `CanHear` branches already reported with a trace note (item41c; once per VM).
    hearing_partials: HashSet<&'static str>,
    /// Bone name recorded by the most recent `Actor.Trace` actor hit, returned by
    /// `Actor.GetLastTraceBone` (`XIIIPawn.LastBoneHit`). `"None"` when the last trace hit world
    /// geometry (or nothing).
    last_trace_bone: String,
    /// Voice-wave duration provider (dialogue natives). `None` = `Actor.GetWaveDuration` reports
    /// `0` with a visible note (the script then falls back to its own default wave length).
    pub(crate) voice_duration: Option<Box<dyn crate::voice::VoiceDuration>>,
    /// Wave-position provider for `Actor.WaveHasPosition` (item48). `None` = the native fails
    /// with [`VmErrorKind::NoAudioProvider`] (never a silent answer).
    pub(crate) wave_position: Option<Box<dyn crate::voice::WavePosition>>,
    /// Optional host save-slot adapter for GUIController natives.
    pub(crate) save_slots: Option<Box<dyn crate::item20::SaveSlotProvider>>,
    /// Outbound presentation events emitted by presentation natives (sound, texture, display,
    /// projectors). Drained with [`Vm::drain_events`].
    events: Vec<PresentationEvent>,
    /// Explicit `ParticleEmitter.SpawnParticle` requests for the presentation host. The emitter
    /// object and request origin remain VM-owned; the renderer consumes these commands once.
    particle_spawns: Vec<(ObjectId, usize)>,
    /// Pending level-travel request (item15). Set by the `PlayerController.ClientTravel` native
    /// or observed on `LevelInfo.NextURL` after the game's `ServerTravel`; consumed by the host
    /// with [`Vm::take_travel_request`]. The VM itself never loads a map.
    pending_travel: Option<crate::events::TravelRequest>,
    /// Cached `LevelInfo` instance for the per-tick `NextURL` check (`None` until the map is
    /// loaded).
    level_info: Option<ObjectId>,
    /// Last non-empty `LevelInfo.NextURL` seen, so one travel request is reported per URL.
    last_next_url: String,
    /// Local URL the runtime loaded this map with (`<Map>?<options>`), returned by
    /// `LevelInfo.GetLocalURL` (UE2 `ALevelInfo::GetLocalURL`). The runtime owns the string;
    /// empty until it is configured.
    local_url: String,
    /// Options string (the `?Key=Value` tail of the local URL) passed to `GameInfo.InitGame`
    /// and `GameInfo.Login`, as the engine's `UGameEngine::LoadMap` does.
    url_options: String,
    /// `Host:Port` address form of the loaded URL, returned by `LevelInfo.GetAddressURL`
    /// (UE2 `ALevelInfo::GetAddressURL` formats the URL host and port as `%s:%i`). Empty until
    /// the runtime configures it.
    address_url: String,
    /// Script-drawn `Canvas` command buffer plus the host font provider. Drained by the host
    /// after each `HUD.PostRender` call ([`Vm::drain_canvas`]).
    pub canvas: CanvasState,
    /// Interned object references into packages outside the loaded script set.
    externals: std::cell::RefCell<ExternalTable>,
    /// Host localisation provider (the install's active `.int`/language files). `None` = no
    /// `Object.Localize` provider is installed and no `localized` class default is overridden;
    /// `Object.Localize` then fails explicitly.
    pub(crate) localization: Option<Box<dyn LocalizationData>>,
    /// `localized` class-default values filled from the `.int` files while building layouts.
    pub localized_overrides: u64,
    /// `Object.Localize` lookups that found a key.
    pub localization_hits: u64,
    /// `Object.Localize` lookups that missed (the placeholder was returned and counted).
    pub localization_misses: u64,
    /// Host resolver for properties of objects in non-script packages (`Texture.USize`/`VSize`).
    pub(crate) external_data: Option<Box<dyn ExternalObjectData>>,
    /// Optional per-native/section timing (`--perf-natives`).
    profile: NativeProfile,
    /// Edge state of the host-driven AI perception (`item14b`): `controller -> player visible`.
    /// Set by [`Vm::update_ai_perception`] so `SeePlayer`/`EnemyNotVisible` fire only on change,
    /// as the engine's sight counter does, instead of restarting an AI state every tick.
    ai_visible: HashMap<ObjectId, bool>,
    /// item14c: non-static calls dropped because the target actor was **suspended** (not merely
    /// out of the executed scope). Every such drop also records a trace note; this counter makes
    /// the total visible to the host/report so a suspended actor's silent no-ops cannot hide.
    suspended_deferred_calls: u64,
    /// Explicit partial-execution diagnostic policy, never used by normal gameplay.
    diagnostic_call_scope: bool,
    /// item18: Bink video durations in seconds, keyed by lowercased file stem. The host registers
    /// them (the VM deliberately has no filesystem access); an entry is absent when the Bink header
    /// could not be read, in which case `VideoPlayer.GetStatus` keeps the old "finished" Partial.
    video_durations: HashMap<String, f32>,
    /// item18: the currently open `Engine.VideoPlayer` (`None` before `Open`).
    video: Option<VideoPlayback>,
    /// item21: optional host playback provider (the Bevy runtime's `xiii-video` player). When
    /// installed, `GetStatus` reports completion from the actual host playback instead of the
    /// Bink-header duration timer.
    video_host: Option<Box<dyn VideoPlayerHost>>,
}

/// item21: host-backed cutscene playback for `Engine.VideoPlayer`. The VM is filesystem-free and
/// has no display or audio device; the host (the Bevy runtime) installs an implementation that
/// decodes the clip with the clean-room `xiii-video` decoder and plays it fullscreen with its
/// Bink Audio track. `Engine.VideoPlayer.GetStatus` then reports completion from the actual host
/// playback, not a VM timer. The duration fallback ([`Vm::set_video_duration`]) stays for hosts
/// that cannot decode a file (labelled at the native).
pub trait VideoPlayerHost {
    /// Opens the clip named `name` (lowercased file stem). `Some(duration)` when the host can
    /// decode the file and will play it; `None` when it cannot (the caller falls back to a
    /// registered duration, labelled).
    fn open(&mut self, name: &str) -> Option<f32>;
    /// Starts playback of the opened clip (`Engine.VideoPlayer.Play`).
    fn play(&mut self);
    /// Stops playback and releases the clip (`Engine.VideoPlayer.Stop`; the menu's
    /// `InternalOnKeyEvent` calls it, the level-end `PlayingVideo` state never does).
    fn stop(&mut self);
    /// True once the host playback ran to its end (every decoded frame shown).
    fn finished(&self) -> bool;
    /// True when the host playback failed (frames could not be decoded);
    /// `GetStatus` then reports the game's error status `2`.
    fn errored(&self) -> bool;
}

/// item21: how the open clip is timed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VideoTiming {
    /// The host decodes and plays the clip; completion follows the host playback.
    Host,
    /// The file could not be decoded (or no host is installed); the clip is timed from the
    /// registered Bink-header duration instead (labelled at the native).
    Duration,
    /// Nothing times the clip; `GetStatus` reports finished immediately.
    Untimed,
}

/// item18: host-driven `Engine.VideoPlayer` state.
///
/// `Engine.VideoPlayer.Open(name)` records the clip; `Play` starts it at [`Vm::time`];
/// `GetStatus` returns `1` (playing) until the clip ends and `0` (finished) after. With a
/// [`VideoPlayerHost`] installed the clip is decoded and played by the host and completion
/// follows the host playback; otherwise the host-registered Bink-header duration times it
/// (labelled Partial). A clip with no duration and no host reports finished immediately,
/// preserving the item16 menu behavior.
#[derive(Debug, Clone)]
pub struct VideoPlayback {
    /// Lowercased file stem as passed to `Open` (directory and `.bik` stripped).
    pub name: String,
    /// Decoded duration in seconds (from the host decoder or the Bink header).
    pub duration: Option<f32>,
    /// VM time at which `Play` was called (`None` before `Play`).
    pub started_at: Option<f64>,
    /// item21: how this clip is timed.
    pub timing: VideoTiming,
}

fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

/// UE2 `FixedTurn`: move the wrapped 16-bit rotator component toward its target by at most
/// `rate * dt` units. Values are kept signed because Unreal serializes rotators that way.
fn rotation_step(current: i32, desired: i32, rate: i32, dt: f32) -> i32 {
    if rate <= 0 || !dt.is_finite() || dt <= 0.0 {
        return current;
    }
    let delta = rotation_delta(current, desired);
    // Engine APawn::physicsRotation stores the rate*DeltaTime product as f32 and uses
    // x86 `fistp` (round to nearest, ties to even) before calling AActor::FixedTurn.
    let max_step = ((rate as f32 * dt).round_ties_even() as i64).max(0);
    let step = delta.clamp(-max_step, max_step);
    (i64::from(current) + step) as i32
}

fn rotation_delta(current: i32, desired: i32) -> i64 {
    let mut delta = (i64::from(desired) - i64::from(current)).rem_euclid(65536);
    if delta > 32768 {
        delta -= 65536;
    }
    delta
}

#[cfg(test)]
mod combat_timing_tests {
    use super::rotation_step;

    #[test]
    fn focus_rotation_respects_rate_and_wraps_signed_rotators() {
        assert_eq!(rotation_step(0, 10_000, 900, 0.5), 450);
        assert_eq!(rotation_step(0, 20, 10, 0.55), 6);
        assert_eq!(rotation_step(32_700, -32_700, 1_000, 0.1), 32_800);
        assert_eq!(rotation_step(-32_700, 32_700, 1_000, 0.1), -32_800);
        assert_eq!(rotation_step(10, -20, 0, 1.0), 10);
    }
}

/// item18: normalises a `VideoPlayer` clip name to a lowercased stem (drops any directory and a
/// trailing `.bik`), so `MapInfo.EndMapVideo` (`cine01`) and a registered `Cine01.bik` agree.
fn video_stem(name: &str) -> String {
    let file = name.rsplit(['/', '\\']).next().unwrap_or(name);
    let stem = if file.len() > 4 && file[file.len() - 4..].eq_ignore_ascii_case(".bik") {
        &file[..file.len() - 4]
    } else {
        file
    };
    stem.to_ascii_lowercase()
}

impl<'s> Vm<'s> {
    /// New VM over a loaded set with the built-in native registry.
    pub fn new(set: &'s ScriptSet, limits: VmLimits) -> Self {
        Self {
            set,
            objects: Vec::new(),
            by_export: HashMap::new(),
            layouts: HashMap::new(),
            func_layouts: HashMap::new(),
            stmt_maps: HashMap::new(),
            default_objects: HashMap::new(),
            registry: Registry::builtin(),
            trace: Vec::new(),
            trace_natives: true,
            stack: Vec::new(),
            tick_count: 0,
            time: 0.0,
            steps: 0,
            limits,
            load_warnings: Vec::new(),
            rng: limits.rng_seed,
            natives_used: Default::default(),
            natives_first_caller: Default::default(),
            combat_natives: Default::default(),
            collect_combat_natives: false,
            survey: false,
            missing_natives: Default::default(),
            pending_latent: None,
            physics: None,
            mover_collision_sent: HashMap::new(),
            animation: None,
            navigation: None,
            hit_zones: None,
            hearing_partials: HashSet::new(),
            last_trace_bone: "None".to_owned(),
            voice_duration: None,
            wave_position: None,
            save_slots: None,
            events: Vec::new(),
            particle_spawns: Vec::new(),
            pending_travel: None,
            level_info: None,
            last_next_url: String::new(),
            local_url: String::new(),
            url_options: String::new(),
            address_url: String::new(),
            canvas: CanvasState::default(),
            externals: std::cell::RefCell::new(ExternalTable::default()),
            localization: None,
            localized_overrides: 0,
            localization_hits: 0,
            localization_misses: 0,
            external_data: None,
            profile: NativeProfile::default(),
            ai_visible: HashMap::new(),
            suspended_deferred_calls: 0,
            diagnostic_call_scope: false,
            video_durations: HashMap::new(),
            video: None,
            video_host: None,
        }
    }

    /// Arms per-native/section timers (off by default; `--perf-natives`). Adds a timing guard
    /// around every native implementation and around the four `tick_suspending` loops.
    pub fn enable_native_timers(&mut self, on: bool) {
        self.profile.enabled = on;
    }

    /// Current profiling accumulators (empty unless [`Vm::enable_native_timers`] was armed).
    pub fn native_profile(&self) -> &NativeProfile {
        &self.profile
    }

    /// Mutable profiling accumulators, so the hosting application can record its own spans
    /// (player-field writes, touch refresh, event drain, render sync) alongside the VM's.
    pub fn native_profile_mut(&mut self) -> &mut NativeProfile {
        &mut self.profile
    }

    /// Clears the profiling accumulators (keeps the enabled flag).
    pub fn reset_native_profile(&mut self) {
        self.profile.reset();
    }

    /// Installs the host font-metrics provider used by `Canvas.StrLen`/`TextSize`.
    pub fn set_canvas_fonts(&mut self, fonts: Box<dyn crate::canvas::CanvasFonts>) {
        self.canvas.set_fonts(fonts);
    }

    /// Installs the host localisation provider (backed by `xiii-locale`). Call before loading
    /// actors/layouts so `localized` class defaults are filled in: the VM has no filesystem and
    /// cannot open `.int` files itself.
    pub fn set_localization(&mut self, provider: Box<dyn LocalizationData>) {
        self.localization = Some(provider);
    }

    /// True when a localisation provider is installed.
    pub fn has_localization(&self) -> bool {
        self.localization.is_some()
    }

    /// Installs the host resolver for properties of objects in non-script packages. Without it,
    /// property access on such an object is an explicit `UnsupportedValue` error.
    pub fn set_external_object_data(&mut self, provider: Box<dyn ExternalObjectData>) {
        self.external_data = Some(provider);
    }

    /// `Localize(Section, Key, Package)` through the installed provider, or `None` when no
    /// provider is installed or the key is absent. Counted in
    /// [`Vm::localization_hits`]/[`Vm::localization_misses`].
    pub fn localize(&mut self, package: &str, section: &str, key: &str) -> Option<String> {
        let value = self
            .localization
            .as_ref()
            .and_then(|l| l.get(package, section, key));
        if value.is_some() {
            self.localization_hits += 1;
        } else {
            self.localization_misses += 1;
        }
        value
    }

    /// Like [`Vm::localize`] but returns the UE2 placeholder
    /// (`<?language?Package.Section.Key?>`) on a miss, the way `Object.Localize` returns it.
    /// Without a provider the lookup is an explicit `None` (the caller decides), never a
    /// silently empty string.
    pub fn localize_or_placeholder(
        &mut self,
        package: &str,
        section: &str,
        key: &str,
    ) -> Option<String> {
        let lookup = self
            .localization
            .as_ref()
            .map(|l| (l.language().to_owned(), l.get(package, section, key)));
        let (language, value) = lookup?;
        match value {
            Some(v) => {
                self.localization_hits += 1;
                Some(v)
            }
            None => {
                self.localization_misses += 1;
                Some(placeholder(&language, package, section, key))
            }
        }
    }

    /// Draw commands recorded since the last [`Vm::drain_canvas`].
    pub fn canvas_commands(&self) -> &[crate::canvas::DrawCommand] {
        self.canvas.commands()
    }

    /// Removes and returns the recorded `Canvas` draw commands (one HUD frame).
    pub fn drain_canvas(&mut self) -> Vec<crate::canvas::DrawCommand> {
        self.canvas.drain()
    }

    /// The script set.
    pub fn set(&self) -> &'s ScriptSet {
        self.set
    }

    /// Native registry.
    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// Sets the world-physics provider (movement/trace natives). Call before runs that
    /// need collision; without one those natives fail explicitly.
    pub fn set_physics(&mut self, provider: Box<dyn WorldPhysics>) {
        self.physics = Some(provider);
        // A new provider starts with every mover enabled; resend the actual states.
        self.mover_collision_sent.clear();
    }

    /// True when a world-physics provider is available.
    pub fn has_physics(&self) -> bool {
        self.physics.is_some()
    }

    /// Registers a mover's collision triangles (world space at its base pose, Unreal units) with
    /// the installed physics provider so `Move`/`Trace` consider the moving brush. No-op without
    /// a provider or when the provider ignores moving brushes.
    pub fn register_mover(
        &mut self,
        actor: &str,
        source: u32,
        triangles: &[[[f32; 3]; 3]],
        origin: [f32; 3],
        rotation: [i32; 3],
    ) {
        if let Some(provider) = self.physics.as_mut() {
            provider.register_mover(actor, source, triangles, origin, rotation);
        }
    }

    /// Sets the animation-sequence provider (animation natives). Call before runs that need
    /// sequence data; without one those natives fail explicitly.
    pub fn set_animation_data(&mut self, provider: Box<dyn AnimationData>) {
        self.animation = Some(provider);
    }

    /// True when an animation-sequence provider is available.
    pub fn has_animation_data(&self) -> bool {
        self.animation.is_some()
    }

    /// Sets the decoded navigation provider (pathing natives). Call before runs that need the
    /// `ReachSpec` graph; without one those natives fail explicitly.
    pub fn set_navigation(&mut self, provider: Box<dyn NavigationData>) {
        self.navigation = Some(provider);
    }

    /// True when a navigation provider is available.
    pub fn has_navigation(&self) -> bool {
        self.navigation.is_some()
    }

    /// Installs a hit-zone provider for `Actor.GetLastTraceBone` (item14). Without one the
    /// default [`crate::physics::CylinderZones`] classifies hits against the target cylinder.
    pub fn set_hit_zones(&mut self, provider: Box<dyn HitZones>) {
        self.hit_zones = Some(provider);
    }

    /// Bone name recorded by the most recent `Actor.Trace` actor hit (a name constant such as
    /// `X Head`/`X Spine1`/`X Spine`, or `None`). Read by `Actor.GetLastTraceBone`.
    pub fn last_trace_bone(&self) -> &str {
        &self.last_trace_bone
    }

    /// Sets the voice-wave duration provider (dialogue natives). Call before runs that need a
    /// real subtitle lifetime; without one `Actor.GetWaveDuration` reports `0` (visible).
    pub fn set_voice_duration(&mut self, provider: Box<dyn crate::voice::VoiceDuration>) {
        self.voice_duration = Some(provider);
    }

    /// Installs the front-end's filesystem-backed GUI save-slot adapter.
    pub fn set_save_slots(&mut self, provider: Box<dyn crate::item20::SaveSlotProvider>) {
        self.save_slots = Some(provider);
    }

    /// Mutable access used by host GUI actions that need to complete a slot read.
    pub fn save_slots_mut(&mut self) -> Option<&mut dyn crate::item20::SaveSlotProvider> {
        match self.save_slots.as_mut() {
            Some(p) => Some(p.as_mut()),
            None => None,
        }
    }

    /// True when a voice-duration provider is available.
    pub fn has_voice_duration(&self) -> bool {
        self.voice_duration.is_some()
    }

    /// Duration of a script `SoundName` from the host provider, `None` when unavailable.
    pub fn voice_duration(&self, sound_name: &str) -> Option<f32> {
        self.voice_duration
            .as_ref()
            .and_then(|p| p.duration(sound_name))
    }

    /// Sets the wave-position provider (`Actor.WaveHasPosition`, item48). Call before runs that
    /// speak dialogue; without one the native fails explicitly.
    pub fn set_wave_position(&mut self, provider: Box<dyn crate::voice::WavePosition>) {
        self.wave_position = Some(provider);
    }

    /// Positional classification of a script `SoundName` from the host provider. `None` when no
    /// provider is installed (the native then fails) or the provider cannot classify the name
    /// (the native then notes and reports `false`).
    pub fn wave_position(&self, sound_name: &str) -> Option<bool> {
        self.wave_position
            .as_ref()
            .and_then(|p| p.has_position(sound_name))
    }

    /// True when a wave-position provider is installed (`Actor.WaveHasPosition`).
    pub fn has_wave_position(&self) -> bool {
        self.wave_position.is_some()
    }

    /// Configures the map's local URL (`<Map>?<options>`, the `url_options` being the
    /// `?Key=Value` tail) and the options the engine passes to `GameInfo.InitGame`/`Login`.
    /// The runtime owns these strings; the VM only stores and exposes them through
    /// `LevelInfo.GetLocalURL` and the `InitGame` call.
    pub fn set_local_url(&mut self, local_url: impl Into<String>, url_options: impl Into<String>) {
        self.local_url = local_url.into();
        self.url_options = url_options.into();
    }

    /// The configured local URL (empty until [`Vm::set_local_url`]).
    pub fn local_url(&self) -> &str {
        &self.local_url
    }

    /// The configured URL options (empty until [`Vm::set_local_url`]).
    pub fn url_options(&self) -> &str {
        &self.url_options
    }

    /// Sets the GameInfo's `StartSpotEvent` name (the value `Plage00.FirstFrame`'s 0x0013 guard
    /// checks before re-applying the wounded intro health; XIII's only script writer is
    /// `XIIIGameInfo.RestartPlayer` 0x037A, which copies `StartSpot.Event`, so the retail
    /// "LOAD" value must come from the engine's native checkpoint-load path after the login
    /// chain — the front-end host presents a resume by setting it at that same point).
    pub fn set_start_spot_event(&mut self, value: impl Into<String>) {
        let id = (0..self.objects.len() as ObjectId).find(|&id| {
            let o = &self.objects[id as usize];
            !o.deleted && o.is_actor && self.is_a(id, "gameinfo")
        });
        if let Some(id) = id {
            let _ = self.set_property(id, "StartSpotEvent", 0, Value::Name(value.into()));
        }
    }

    /// Configures the `Host:Port` address form returned by `LevelInfo.GetAddressURL`. The
    /// runtime owns it (see `xiii_world::runtime::level_address`).
    pub fn set_address_url(&mut self, address: impl Into<String>) {
        self.address_url = address.into();
    }

    /// The configured address URL (empty until [`Vm::set_address_url`]).
    pub fn address_url(&self) -> &str {
        &self.address_url
    }

    /// Physics natives check this before running: `Ok(true)` when a provider is present,
    /// `Ok(false)` when the run is in survey mode and the native was counted like a missing
    /// native (the caller returns the type's zero), `Err(NoPhysicsProvider)` otherwise.
    pub(crate) fn physics_ready(
        &mut self,
        native: &str,
        index: Option<u16>,
        this: ObjectId,
        ret: Value,
    ) -> VmResult<bool> {
        if self.physics.is_some() {
            return Ok(true);
        }
        if !self.survey {
            return Err(self.err(VmErrorKind::NoPhysicsProvider {
                native: native.to_owned(),
            }));
        }
        self.survey_missing(native.to_owned(), index, this, &[], ret, false)?;
        Ok(false)
    }

    /// Animation natives check this before running, mirroring [`Vm::physics_ready`].
    pub(crate) fn animation_ready(
        &mut self,
        native: &str,
        index: Option<u16>,
        this: ObjectId,
        ret: Value,
    ) -> VmResult<bool> {
        if self.animation.is_some() {
            return Ok(true);
        }
        if !self.survey {
            return Err(self.err(VmErrorKind::NoAnimationProvider {
                native: native.to_owned(),
            }));
        }
        self.survey_missing(native.to_owned(), index, this, &[], ret, false)?;
        Ok(false)
    }

    /// Pathing natives check this before running, mirroring [`Vm::physics_ready`].
    pub(crate) fn navigation_ready(
        &mut self,
        native: &str,
        index: Option<u16>,
        this: ObjectId,
        ret: Value,
    ) -> VmResult<bool> {
        if self.navigation.is_some() {
            return Ok(true);
        }
        if !self.survey {
            return Err(self.err(VmErrorKind::NoNavProvider {
                native: native.to_owned(),
            }));
        }
        self.survey_missing(native.to_owned(), index, this, &[], ret, false)?;
        Ok(false)
    }

    // ------------------------------------------------------------------ errors and trace

    pub(crate) fn err(&self, kind: VmErrorKind) -> VmError {
        VmError {
            kind,
            stack: self.stack.clone(),
        }
    }

    /// Appends a trace record at the current tick/time.
    pub fn note(&mut self, kind: TraceKind) {
        self.trace.push(TraceEvent {
            tick: self.tick_count,
            time: self.time,
            kind,
        });
    }

    fn step(&mut self) -> VmResult<()> {
        self.steps += 1;
        if self.steps > self.limits.max_steps {
            return Err(self.err(VmErrorKind::BudgetExceeded {
                limit: self.limits.max_steps,
            }));
        }
        Ok(())
    }

    /// Display label of an object reference.
    pub fn obj_label(&self, r: &ObjRef) -> String {
        match r {
            ObjRef::Instance(i) => self
                .objects
                .get(*i as usize)
                .map_or_else(|| format!("obj#{i}"), |o| o.name.clone()),
            ObjRef::Static(g) => self.set.path(*g),
            ObjRef::External(id) => self
                .external_object(*id)
                .map_or_else(|| format!("external#{id}"), |o| o.path),
        }
    }

    /// Display text of a value (objects by name).
    pub fn value_text(&self, v: &Value) -> String {
        match v {
            Value::Object(Some(r)) => self.obj_label(r),
            other => other.to_string(),
        }
    }

    /// `Package.Object.Path` of an object value (the instance display name for an instance),
    /// or `None` for a null/other value. Used to record presentation arguments.
    pub fn obj_path(&self, v: &Value) -> Option<String> {
        match v {
            Value::Object(Some(ObjRef::Instance(i))) => {
                self.objects.get(*i as usize).map(|o| o.name.clone())
            }
            Value::Object(Some(ObjRef::Static(g))) => Some(self.set.path(*g)),
            Value::Object(Some(ObjRef::External(id))) => self.external_path(&ObjRef::External(*id)),
            _ => None,
        }
    }

    /// Drains the outbound presentation events emitted since the last call.
    pub fn drain_events(&mut self) -> Vec<PresentationEvent> {
        std::mem::take(&mut self.events)
    }

    /// Queues an explicit script `ParticleEmitter.SpawnParticle` request against this VM object.
    pub fn spawn_particles(&mut self, emitter: ObjectId, amount: usize) {
        if amount > 0
            && self
                .objects
                .get(emitter as usize)
                .is_some_and(|o| !o.deleted)
        {
            self.particle_spawns.push((emitter, amount));
        }
    }

    /// Drains particle spawn commands for the renderer. Commands are consumed exactly once.
    pub fn drain_particle_spawns(&mut self) -> Vec<(ObjectId, usize)> {
        std::mem::take(&mut self.particle_spawns)
    }

    /// Number of presentation events waiting to be drained.
    pub fn queued_events(&self) -> usize {
        self.events.len()
    }

    /// Appends a presentation event at the current VM time.
    pub(crate) fn emit_event(&mut self, event: PresentationEvent) {
        self.events.push(event);
    }

    /// Takes the pending level-travel request, if any. The host calls this after each step; the
    /// VM never loads a map itself.
    pub fn take_travel_request(&mut self) -> Option<crate::events::TravelRequest> {
        self.pending_travel.take()
    }

    /// Whether a travel request is waiting (without consuming it).
    pub fn travel_requested(&self) -> bool {
        self.pending_travel.is_some()
    }

    // ---------------------------------------------------------------- item18/21 VideoPlayer

    /// Registers the real duration of a Bink clip (seconds), keyed by file stem. The host reads
    /// the Bink header because `xiii-script` has no filesystem access. A non-finite or negative
    /// duration is ignored (never stored) so `GetStatus` cannot be made to hang on bad data.
    /// With a [`VideoPlayerHost`] installed this is only the labelled fallback for clips the
    /// host cannot decode.
    pub fn set_video_duration(&mut self, name: &str, seconds: f32) {
        if seconds.is_finite() && seconds >= 0.0 {
            self.video_durations.insert(video_stem(name), seconds);
        }
    }

    /// item21: installs the host playback provider. `Engine.VideoPlayer` clips are then decoded
    /// and played by the host and `GetStatus` follows the host playback.
    pub fn set_video_host(&mut self, host: Box<dyn VideoPlayerHost>) {
        self.video_host = Some(host);
    }

    /// Whether a host playback provider is installed.
    pub fn has_video_host(&self) -> bool {
        self.video_host.is_some()
    }

    /// `Engine.VideoPlayer.Open(name)`: records the clip. Returns `true` when the clip is timed
    /// (the host decodes it, or a Bink-header duration is registered as the labelled fallback)
    /// and `false` when it is not (the call is still accepted, and `GetStatus` reports
    /// finished — the labelled Partial). A new `Open` stops any clip the host is still playing.
    pub fn video_open(&mut self, name: &str) -> bool {
        let stem = video_stem(name);
        if self
            .video
            .as_ref()
            .is_some_and(|v| v.timing == VideoTiming::Host)
            && let Some(host) = self.video_host.as_mut()
        {
            host.stop();
        }
        let mut duration = None;
        let mut timing = VideoTiming::Untimed;
        if let Some(host) = self.video_host.as_mut()
            && let Some(d) = host.open(&stem)
        {
            duration = Some(d);
            timing = VideoTiming::Host;
        }
        if timing == VideoTiming::Untimed
            && let Some(d) = self.video_durations.get(&stem).copied()
        {
            duration = Some(d);
            timing = VideoTiming::Duration;
        }
        self.video = Some(VideoPlayback {
            name: stem,
            duration,
            started_at: None,
            timing,
        });
        duration.is_some()
    }

    /// `Engine.VideoPlayer.Play()`: starts (or restarts) the clip. With a host decoder the
    /// host playback starts; completion then follows the host, not this timestamp.
    pub fn video_play(&mut self) {
        let host_played = match self.video.as_mut() {
            Some(v) => {
                v.started_at = Some(self.time);
                v.timing == VideoTiming::Host
            }
            None => false,
        };
        if host_played && let Some(host) = self.video_host.as_mut() {
            host.play();
        }
    }

    /// `Engine.VideoPlayer.Stop()`: stops the host playback and clears the clip.
    pub fn video_stop(&mut self) {
        if self.video.is_some()
            && let Some(host) = self.video_host.as_mut()
        {
            host.stop();
        }
        self.video = None;
    }

    /// `Engine.VideoPlayer.GetStatus() -> int`, the game's own status codes (decoded
    /// `XIIIPlayerController.PlayingVideo.PlayerTick` switches on them): `1` while the clip
    /// plays, `0` when it ended (or was never started / cannot be timed) and `2` when the host
    /// playback failed ("Error playing video"). With a host decoder installed, completion is
    /// the host playback's actual end; otherwise the registered Bink-header duration times the
    /// clip from `Play`.
    pub fn video_status(&self) -> i32 {
        let Some(v) = self.video.as_ref() else {
            return 0;
        };
        let Some(started) = v.started_at else {
            return 0;
        };
        match v.timing {
            VideoTiming::Host => match self.video_host.as_ref() {
                Some(h) if h.errored() => 2,
                Some(h) if !h.finished() => 1,
                _ => 0,
            },
            VideoTiming::Duration => match v.duration {
                Some(d) if (self.time - started) < f64::from(d) => 1,
                _ => 0,
            },
            VideoTiming::Untimed => 0,
        }
    }

    /// How the currently open clip is timed, if one is open (the native's trace label).
    pub fn video_timing(&self) -> Option<VideoTiming> {
        self.video.as_ref().map(|v| v.timing)
    }

    /// Duration of the currently open clip, if known (the native's trace label).
    pub fn video_duration(&self) -> Option<f32> {
        self.video.as_ref().and_then(|v| v.duration)
    }

    /// The current `Engine.VideoPlayer` clip stem, if one is open (diagnostics).
    pub fn video_name(&self) -> Option<&str> {
        self.video.as_ref().map(|v| v.name.as_str())
    }

    /// Records a travel request and queues the matching presentation event. The first request
    /// wins until the host consumes it (a repeated `ServerTravel`/`ClientTravel` in the same
    /// step does not overwrite it).
    pub(crate) fn request_travel(&mut self, request: crate::events::TravelRequest) {
        self.emit_event(PresentationEvent::TravelRequest(request.clone()));
        if self.pending_travel.is_none() {
            self.pending_travel = Some(request);
        }
    }

    /// Emits a `PlaySound`/`PlayMusic` event from a native's decoded arguments. `args[0]` is the
    /// `Sound`; `args[1..=5]` are `Param1..Param5` (an omitted optional argument becomes `None`).
    pub(crate) fn emit_sound(
        &mut self,
        music: bool,
        this: ObjectId,
        args: &[Value],
        omitted: &[bool],
    ) {
        let sound = args.first().and_then(|v| self.obj_path(v));
        let param = |i: usize| -> Option<i32> {
            if omitted.get(i).copied().unwrap_or(true) {
                return None;
            }
            match args.get(i) {
                Some(Value::Int(v)) => Some(*v),
                Some(Value::Byte(v)) => Some(i32::from(*v)),
                _ => None,
            }
        };
        let event = SoundEvent {
            actor: self.objects[this as usize].name.clone(),
            sound,
            rolloff_actor: None,
            slot: param(1),
            volume: param(2),
            radius: param(3),
            pitch: param(4),
            param5: param(5),
            time: self.time,
        };
        self.events.push(if music {
            PresentationEvent::PlayMusic(event)
        } else {
            PresentationEvent::PlaySound(event)
        });
    }

    /// Emits an `Actor.PlayRolloffSound` event: `args[0]` is the `Sound`, `args[1]` the
    /// `RollOffActor`, `args[2..=6]` are `Param1..Param5`.
    pub(crate) fn emit_rolloff_sound(&mut self, this: ObjectId, args: &[Value], omitted: &[bool]) {
        let sound = args.first().and_then(|v| self.obj_path(v));
        let rolloff_actor = args.get(1).and_then(|v| self.obj_path(v));
        let param = |i: usize| -> Option<i32> {
            if omitted.get(i).copied().unwrap_or(true) {
                return None;
            }
            match args.get(i) {
                Some(Value::Int(v)) => Some(*v),
                Some(Value::Byte(v)) => Some(i32::from(*v)),
                _ => None,
            }
        };
        let event = SoundEvent {
            actor: self.objects[this as usize].name.clone(),
            sound,
            rolloff_actor,
            slot: param(2),
            volume: param(3),
            radius: param(4),
            pitch: param(5),
            param5: param(6),
            time: self.time,
        };
        self.events.push(PresentationEvent::PlayRolloffSound(event));
    }

    /// Short `Class.Function` path of an export (package omitted).
    pub fn short_path(&self, g: GlobalRef) -> String {
        let p = &self.set.packages[g.package];
        p.package
            .object_path(ObjectRef::Export(g.export))
            .unwrap_or("?")
            .to_owned()
    }

    fn object_name(&self, g: GlobalRef) -> &'s str {
        self.set.packages[g.package].ref_name(ObjectRef::Export(g.export))
    }

    // ------------------------------------------------------------------ layouts

    fn struct_header(&self, g: GlobalRef) -> Option<&'s crate::reflect::StructHeader> {
        self.set.object(g)?.struct_header()
    }

    fn child_props(&self, owner: GlobalRef) -> Vec<(GlobalRef, &'s Property)> {
        let mut out = Vec::new();
        let set = self.set;
        let Some(h) = self.struct_header(owner) else {
            return out;
        };
        let p = &set.packages[owner.package];
        let mut child = h.children;
        let mut guard = 0;
        while let ObjectRef::Export(e) = child {
            guard += 1;
            if guard > 65_536 {
                break;
            }
            let Some(obj) = p.objects.get(&e) else { break };
            if let ScriptObject::Property(prop) = obj {
                out.push((
                    GlobalRef {
                        package: owner.package,
                        export: e,
                    },
                    prop,
                ));
            }
            child = obj.field().next;
        }
        out
    }

    fn ty_of(&self, pkg: usize, kind: &PropertyKind, depth: u32) -> Ty {
        match kind {
            PropertyKind::Byte { .. } => Ty::Byte,
            PropertyKind::Int => Ty::Int,
            PropertyKind::Bool => Ty::Bool,
            PropertyKind::Float => Ty::Float,
            PropertyKind::Object { .. } | PropertyKind::Class { .. } => Ty::Object,
            PropertyKind::Name => Ty::Name,
            PropertyKind::Str => Ty::Str,
            PropertyKind::Delegate { .. } => Ty::Delegate,
            PropertyKind::Array { inner } => {
                let inner_ty = self
                    .set
                    .resolve(pkg, *inner)
                    .and_then(|g| match self.set.object(g) {
                        Some(ScriptObject::Property(p)) => {
                            Some(self.ty_of(g.package, &p.kind, depth + 1))
                        }
                        _ => None,
                    })
                    .unwrap_or(Ty::Int);
                Ty::Array(Box::new(inner_ty))
            }
            PropertyKind::Struct { strukt } => {
                let Some(g) = self.set.resolve(pkg, *strukt) else {
                    return Ty::Struct(Vec::new());
                };
                let name = lower(self.object_name(g));
                match name.as_str() {
                    "vector" => Ty::Vector,
                    "rotator" => Ty::Rotator,
                    _ if depth >= self.limits.max_type_depth => Ty::Struct(Vec::new()),
                    _ => {
                        let mut members = Vec::new();
                        for (pg, p) in self.child_props(g) {
                            let t = self.ty_of(pg.package, &p.kind, depth + 1);
                            members.push((lower(self.object_name(pg)), t));
                        }
                        Ty::Struct(members)
                    }
                }
            }
        }
    }

    /// Class chain of a class (most derived first).
    pub fn class_chain(&self, class: GlobalRef) -> Vec<GlobalRef> {
        let mut out = vec![class];
        let mut cur = class;
        while out.len() < 256 {
            let Some(h) = self.struct_header(cur) else {
                break;
            };
            match self.set.resolve(cur.package, h.field.super_field) {
                Some(s) => {
                    out.push(s);
                    cur = s;
                }
                None => break,
            }
        }
        out
    }

    /// Layout (and defaults) of a class.
    pub fn class_layout(&mut self, class: GlobalRef) -> VmResult<Rc<ClassLayout>> {
        if let Some(l) = self.layouts.get(&class) {
            return Ok(l.clone());
        }
        if !matches!(self.set.object(class), Some(ScriptObject::Class(_))) {
            return Err(self.err(VmErrorKind::Unresolved {
                what: format!("{} is not a decoded class", self.set.path(class)),
            }));
        }
        let chain = self.class_chain(class);
        let mut slots = Vec::new();
        let mut by_prop = HashMap::new();
        let mut by_name = HashMap::new();
        let mut size = 0;
        for c in chain.iter().rev() {
            for (pg, p) in self.child_props(*c) {
                let ty = self.ty_of(pg.package, &p.kind, 0);
                let dim = usize::try_from(p.array_dim.max(1)).unwrap_or(1);
                let name = lower(self.object_name(pg));
                by_prop.insert(pg, slots.len());
                by_name.insert(name.clone(), slots.len());
                slots.push(Slot {
                    prop: pg,
                    name,
                    ty,
                    dim,
                    base: size,
                    flags: p.flags,
                    declaring: *c,
                });
                size += dim;
            }
        }
        let mut defaults = Vec::with_capacity(size);
        for s in &slots {
            for _ in 0..s.dim {
                defaults.push(s.ty.zero());
            }
        }
        let chain_names: Vec<String> = chain.iter().map(|g| lower(self.object_name(*g))).collect();
        let slot_base = |name: &str| by_name.get(name).map(|i| slots[*i].base);
        let location_slot = slot_base("location");
        let rotation_slot = slot_base("rotation");
        let collide_slot = slot_base("bcollideactors");
        let interp_slot = slot_base("binterpolating");
        let is_mover_class = chain_names.iter().any(|n| n == "mover");
        let mut layout = ClassLayout {
            class,
            chain: chain.clone(),
            chain_names,
            slots,
            by_prop,
            by_name,
            size,
            defaults: Vec::new(),
            location_slot,
            rotation_slot,
            collide_slot,
            interp_slot,
            is_mover_class,
        };
        for c in chain.iter().rev() {
            if let Some(ScriptObject::Class(cl)) = self.set.object(*c) {
                self.apply_block(c.package, &cl.defaults, &layout, &mut defaults);
            }
        }
        // Native defaults not present in the serialized class blocks (see native_class_default).
        // The table only lists properties that no class in the matching lineage serializes, so
        // applying it after the serialized blocks cannot hide an authored value.
        for slot in &layout.slots {
            if slot.dim == 0 {
                continue;
            }
            if let Some(v) = layout
                .chain_names
                .iter()
                .find_map(|n| native_class_default(n, &slot.name))
            {
                defaults[slot.base] = v;
            }
        }
        // `localized` class defaults come from the class package's `.int` file: section = class
        // name, key = property name (array elements as `Property[i]`, per UE2
        // `UObject::LoadLocalizedProperty`). The class being laid out is tried first (the
        // shipped `.int` files carry the text under the subclass section, for example
        // `[Plage01CahuteKeyPick] PickupMessage=`), then the class that declares the property
        // (`[Pickup]`). Applied last, so a shipped `.int` value wins over the serialized
        // placeholder; a missing key leaves the serialized value in place.
        let layout_pkg = self.set.packages[class.package].name.clone();
        let layout_class = self.object_name(class).to_owned();
        let localized: Vec<(usize, usize, String, String, GlobalRef)> = layout
            .slots
            .iter()
            // Localisation is a string-property feature (`localized` is always a string upstream);
            // a non-string slot with a stray flag bit is left alone rather than typed as text.
            .filter(|s| {
                s.flags & property_flags::LOCALIZED != 0 && s.dim > 0 && matches!(s.ty, Ty::Str)
            })
            .map(|s| {
                (
                    s.base,
                    s.dim,
                    layout_pkg.clone(),
                    s.name.clone(),
                    s.declaring,
                )
            })
            .collect();
        for (base, dim, package, name, declaring) in localized {
            let declaring_pkg = self.set.packages[declaring.package].name.clone();
            let declaring_class = self.object_name(declaring).to_owned();
            for elem in 0..dim {
                let key = if dim > 1 {
                    format!("{name}[{elem}]")
                } else {
                    name.clone()
                };
                let value = self
                    .localization
                    .as_ref()
                    .and_then(|l| l.get(&package, &layout_class, &key))
                    .or_else(|| {
                        self.localization
                            .as_ref()
                            .and_then(|l| l.get(&declaring_pkg, &declaring_class, &key))
                    });
                if let Some(v) = value {
                    defaults[base + elem] = Value::Str(v);
                    self.localized_overrides += 1;
                }
            }
        }
        layout.defaults = defaults;
        let rc = Rc::new(layout);
        self.layouts.insert(class, rc.clone());
        Ok(rc)
    }

    fn func_layout(&mut self, func: GlobalRef) -> Rc<FuncLayout> {
        if let Some(l) = self.func_layouts.get(&func) {
            return l.clone();
        }
        let mut slots = Vec::new();
        let mut by_prop = HashMap::new();
        let mut params = Vec::new();
        let mut ret = None;
        let mut size = 0;
        for (pg, p) in self.child_props(func) {
            let ty = self.ty_of(pg.package, &p.kind, 0);
            let dim = usize::try_from(p.array_dim.max(1)).unwrap_or(1);
            by_prop.insert(pg, slots.len());
            if p.flags & property_flags::RETURN_PARM != 0 {
                ret = Some((size, ty.clone()));
            } else if p.flags & property_flags::PARM != 0 {
                params.push(ParamInfo {
                    slot: size,
                    out: p.flags & property_flags::OUT_PARM != 0,
                    ty: ty.clone(),
                });
            }
            slots.push(Slot {
                prop: pg,
                name: lower(self.object_name(pg)),
                ty,
                dim,
                base: size,
                flags: p.flags,
                declaring: func,
            });
            size += dim;
        }
        let l = Rc::new(FuncLayout {
            slots,
            by_prop,
            params,
            ret,
            size,
        });
        self.func_layouts.insert(func, l.clone());
        l
    }

    fn stmt_map(&mut self, owner: GlobalRef, script: &Script) -> Rc<HashMap<u32, usize>> {
        if let Some(m) = self.stmt_maps.get(&owner) {
            return m.clone();
        }
        let m: HashMap<u32, usize> = script
            .statements
            .iter()
            .enumerate()
            .map(|(i, t)| (t.offset, i))
            .collect();
        let rc = Rc::new(m);
        self.stmt_maps.insert(owner, rc.clone());
        rc
    }

    // ------------------------------------------------------------------ loading

    fn resolve_value_ref(&self, pkg: usize, r: ObjectRef) -> Value {
        if r.is_null() {
            return Value::Object(None);
        }
        match self.set.resolve(pkg, r) {
            Some(g) => match self.by_export.get(&g) {
                Some(id) => Value::Object(Some(ObjRef::Instance(*id))),
                None => Value::Object(Some(ObjRef::Static(g))),
            },
            None => {
                let path = self.set.packages[pkg].ref_path(r);
                // `class'Core.Class'` names the native meta-class, which has no export in
                // core.u. Give it a distinct value (not `None`) so class checks and
                // `DynamicLoadObject` see a class, not a null. The same holds for engine
                // classes declared native-only (no export in engine.u), e.g. `Engine.Mesh`,
                // `Engine.SkeletalMesh`, `Engine.Level`; an unresolved reference into a
                // *loaded* package is therefore treated as a native-only class (measured:
                // those names never have an export).
                if meta_class_path(&path) || self.native_only_class(&path) {
                    Value::NativeClass(self.set.packages[pkg].ref_name(r).to_owned())
                } else if self.ref_package_loaded(&path) {
                    // Unresolved inside a *loaded* script package: a real decode/reference
                    // error (never silently hidden).
                    Value::Unsupported(format!("unresolved reference {path}"))
                } else {
                    // The referenced package is not among the loaded script packages: a
                    // non-script asset package (a `.uax` sound, a `.utx` texture, ...). Keep a
                    // real object value carrying the full path and the class from the
                    // referencing package's import table, so presentation events (sounds,
                    // music, textures) receive the path instead of `None`. When the runtime
                    // registered the external package, a missing package/export is an explicit
                    // error (counted), not `None`.
                    self.external_object_value(pkg, r, &path)
                }
            }
        }
    }

    /// Builds the value for an unresolved reference into a non-script package. The class comes
    /// from the referencing package's import table (always available) or, when the runtime
    /// registered and verified the external package, from the package's own export.
    fn external_object_value(&self, pkg: usize, r: ObjectRef, path: &str) -> Value {
        match self.set.external_lookup(path) {
            crate::linker::ExternalLookup::MissingPackage => Value::Unsupported(format!(
                "unresolved external object {path}: package not found"
            )),
            crate::linker::ExternalLookup::MissingExport => Value::Unsupported(format!(
                "unresolved external object {path}: export not found"
            )),
            crate::linker::ExternalLookup::Found(class) => {
                let class = self.set.packages[pkg].import_class_path(r).unwrap_or(class);
                Value::Object(Some(ObjRef::External(
                    self.intern_external(path, Some(class)),
                )))
            }
            crate::linker::ExternalLookup::Unknown => {
                let class = self.set.packages[pkg].import_class_path(r);
                Value::Object(Some(ObjRef::External(self.intern_external(path, class))))
            }
        }
    }

    /// Resolves a `Package.Object` name against the registered external (non-script) packages
    /// the way `DynamicLoadObject` must, returning the interned [`ObjRef::External`] value and
    /// its recorded class path. `None` when the package is unregistered/missing or the export is
    /// absent (or an ambiguous bare name). Used by `Object.DynamicLoadObject` for `.utx`
    /// textures, `.usx` meshes and `.uax` sounds, and by the front-end menu host for the root
    /// window's background textures.
    pub fn external_asset(&self, path: &str) -> Option<(Value, String)> {
        match self.set.external_lookup(path) {
            crate::linker::ExternalLookup::Found(class) => {
                let id = self.intern_external(path, Some(class.clone()));
                Some((Value::Object(Some(ObjRef::External(id))), class))
            }
            _ => None,
        }
    }

    /// Interns an external object path, returning its id. A path maps to exactly one id, so
    /// equality by path holds even when two references record different classes.
    ///
    /// `pub(crate)` so `DynamicLoadObject` (registry.rs) can turn a registered non-script asset
    /// path (a `.utx` texture, `.usx` mesh, `.uax` sound) into the same [`ObjRef::External`] a
    /// reference from a script package would produce; the external-property provider then
    /// answers `USize`/`VSize`.
    pub(crate) fn intern_external(&self, path: &str, class: Option<String>) -> u32 {
        let mut table = self.externals.borrow_mut();
        if let Some(&id) = table.index.get(path) {
            return id;
        }
        let id = table.list.len() as u32;
        table.index.insert(path.to_owned(), id);
        table.list.push(ExternalObject {
            path: path.to_owned(),
            class,
        });
        id
    }

    /// The external object interned at `id` (path and recorded class), if any.
    pub fn external_object(&self, id: u32) -> Option<ExternalObject> {
        self.externals.borrow().list.get(id as usize).cloned()
    }

    /// Full path of an external object reference (`None` for other references).
    pub fn external_path(&self, r: &ObjRef) -> Option<String> {
        match r {
            ObjRef::External(id) => self.external_object(*id).map(|o| o.path),
            _ => None,
        }
    }

    /// `IsA`-style class test for an external object, against the class recorded from the
    /// referencing import table (or the verified external export). Uses the native-class table
    /// from `registry::native_class_is_a`; an object with no recorded class matches nothing.
    pub fn external_is_a(&self, id: u32, name: &str) -> bool {
        self.external_object(id)
            .and_then(|o| o.class)
            .is_some_and(|c| crate::registry::native_class_is_a(&c, name))
    }

    /// True when the package named by a `Package.Object.Path` reference is one of the loaded
    /// script packages.
    fn ref_package_loaded(&self, path: &str) -> bool {
        let Some((package, _)) = path.split_once('.') else {
            return false;
        };
        self.set.package_index(package).is_some()
    }

    /// True when `Package.Object.Path` names a package that is loaded but has no such export
    /// (a native-only class such as `Engine.Mesh`), as opposed to an unresolved external.
    fn native_only_class(&self, path: &str) -> bool {
        let Some((pkg, object)) = path.split_once('.') else {
            return false;
        };
        match self.set.package_index(pkg) {
            Some(pi) => self.set.packages[pi].export_by_path(object).is_none(),
            None => false,
        }
    }

    fn tagged_value(&self, pkg: usize, q: &xiii_package::Property, ty: &Ty) -> Value {
        let p = &self.set.packages[pkg];
        match (&q.value, ty) {
            (PropertyValue::Int(v), Ty::Int) => Value::Int(*v),
            (PropertyValue::Float(v), Ty::Float) => Value::Float(*v),
            (PropertyValue::Bool(v), Ty::Bool) => Value::Bool(*v),
            (PropertyValue::Byte(v), Ty::Byte) => Value::Byte(*v),
            (PropertyValue::Name(n), Ty::Name) => Value::Name(p.package.name(*n).to_owned()),
            (PropertyValue::Str(s), Ty::Str) => Value::Str(s.clone()),
            (PropertyValue::Object(r) | PropertyValue::Class(r), Ty::Object) => {
                self.resolve_value_ref(pkg, *r)
            }
            (PropertyValue::Struct(StructValue::Vector(v)), Ty::Vector) => Value::Vector(*v),
            (PropertyValue::Struct(StructValue::Rotator(v)), Ty::Rotator) => Value::Rotator(*v),
            // The package reader stores a `Color` as four bytes (`b,g,r,a`); the script layout
            // spells the member names, so map by name (a class default like
            // `XIIIDialogMessage.MessageColor` is read as a `struct<Color>`).
            (PropertyValue::Struct(StructValue::Color(c)), Ty::Struct(members)) => {
                let mut fields = Vec::with_capacity(members.len());
                for (name, _) in members {
                    let byte = match name.to_ascii_lowercase().as_str() {
                        "b" => c[0],
                        "g" => c[1],
                        "r" => c[2],
                        "a" => c[3],
                        _ => return Value::Unsupported(format!("color member {name}")),
                    };
                    fields.push((name.clone(), Value::Byte(byte)));
                }
                Value::Struct(fields)
            }
            (PropertyValue::Array { count, elements }, Ty::Array(inner)) => {
                self.decode_array(pkg, *count, *elements, inner)
            }
            // `FRange`/`FRangeVector` are decoded by the package reader as typed structs; map
            // them into the script layout's `{min,max}` members. A class-default subobject
            // template can carry them (e.g. `XIIIBreakingGlassEmitterA.StartSizeRange`), which
            // is applied when the subobject is instantiated.
            (PropertyValue::Struct(StructValue::Range(r)), Ty::Struct(members)) => {
                let mut fields = Vec::with_capacity(members.len());
                for (name, _) in members {
                    let v = match name.to_ascii_lowercase().as_str() {
                        "min" => r[0],
                        "max" => r[1],
                        _ => return Value::Unsupported(format!("range member {name}")),
                    };
                    fields.push((name.clone(), Value::Float(v)));
                }
                Value::Struct(fields)
            }
            (PropertyValue::Struct(StructValue::RangeVector(rv)), Ty::Struct(members)) => {
                if members.len() != 3 {
                    return Value::Unsupported(format!("range vector arity {}", members.len()));
                }
                let mut fields = Vec::with_capacity(3);
                for (i, (name, ty)) in members.iter().enumerate() {
                    let r = rv[i];
                    let inner = match ty {
                        Ty::Struct(inner) => {
                            let mut sub = Vec::with_capacity(inner.len());
                            for (n, _) in inner {
                                let v = match n.to_ascii_lowercase().as_str() {
                                    "min" => r[0],
                                    "max" => r[1],
                                    _ => {
                                        return Value::Unsupported(format!(
                                            "range vector member {n}"
                                        ));
                                    }
                                };
                                sub.push((n.clone(), Value::Float(v)));
                            }
                            Value::Struct(sub)
                        }
                        _ => return Value::Unsupported(format!("range vector member {name}")),
                    };
                    fields.push((name.clone(), inner));
                }
                Value::Struct(fields)
            }
            // A struct the package reader kept raw (`RawReason::UnknownStruct`) can still be
            // decoded from its value span member-by-member when the script class layout gives
            // the member types (e.g. `BaseSoldier.InitialInventory[i]` = {Inventory, Count}).
            // A member that cannot be decoded stays an explicit `Unsupported`, never a guess.
            (PropertyValue::Raw(RawReason::UnknownStruct), Ty::Struct(members)) => {
                self.decode_raw_struct(pkg, q.value_span, members)
            }
            (v, t) => Value::Unsupported(format!("{v:?} as {t:?}")),
        }
    }

    /// Decodes an untagged struct value ([`PropertyValue::Raw`] with a known class-layout type)
    /// from its raw value span. `None`/unsupported on any member or trailing bytes.
    fn decode_raw_struct(
        &self,
        pkg: usize,
        span: xiii_package::Span,
        members: &[(String, Ty)],
    ) -> Value {
        let p = &self.set.packages[pkg];
        let tables = crate::reader::Tables::of(&p.package);
        let Ok(mut r) = crate::reader::Reader::new(&p.data, span.start, span.end, tables) else {
            return Value::Unsupported("struct span".into());
        };
        let mut fields = Vec::new();
        for (name, t) in members {
            match self.decode_ty(&mut r, pkg, t) {
                Some(v) => fields.push((name.clone(), v)),
                None => return Value::Unsupported(format!("raw struct member {name}")),
            }
        }
        if r.remaining() != 0 {
            return Value::Unsupported("struct trailing bytes".into());
        }
        Value::Struct(fields)
    }

    fn decode_array(&self, pkg: usize, count: u32, span: xiii_package::Span, inner: &Ty) -> Value {
        let p = &self.set.packages[pkg];
        let tables = crate::reader::Tables::of(&p.package);
        let Ok(mut r) = crate::reader::Reader::new(&p.data, span.start, span.end, tables) else {
            return Value::Unsupported("array span".into());
        };
        let mut out = Vec::new();
        for _ in 0..count {
            match self.decode_ty(&mut r, pkg, inner) {
                Some(v) => out.push(v),
                None => return Value::Unsupported(format!("array of {inner:?}")),
            }
        }
        if r.remaining() != 0 {
            return Value::Unsupported("array trailing bytes".into());
        }
        Value::Array(out)
    }

    /// Decodes one raw (untagged) value of `ty` from an array/struct element stream. Strings,
    /// nested arrays and structs are decoded member-by-member; `None` for types without a
    /// verified raw layout (never a silently wrong value).
    fn decode_ty(&self, r: &mut crate::reader::Reader<'_>, pkg: usize, ty: &Ty) -> Option<Value> {
        let p = &self.set.packages[pkg];
        Some(match ty {
            Ty::Int => Value::Int(r.i32().ok()?),
            Ty::Float => Value::Float(r.f32().ok()?),
            Ty::Byte => Value::Byte(r.u8().ok()?),
            // A bool inside a raw array/struct element is one byte (verified against the
            // Plage00 `MapInfo.Objectif` element size: empty FString + 3 bools = 4 bytes).
            Ty::Bool => Value::Bool(r.u8().ok()? != 0),
            Ty::Name => Value::Name(p.name_text(r.name().ok()?).to_owned()),
            Ty::Str => Value::Str(r.fstring(1 << 20).ok()?),
            Ty::Object => self.resolve_value_ref(pkg, r.object().ok()?),
            Ty::Vector => Value::Vector([r.f32().ok()?, r.f32().ok()?, r.f32().ok()?]),
            Ty::Rotator => Value::Rotator([r.i32().ok()?, r.i32().ok()?, r.i32().ok()?]),
            Ty::Array(inner) => {
                let n = r.compact().ok()?;
                if n < 0 {
                    return None;
                }
                let mut items = Vec::new();
                for _ in 0..n {
                    items.push(self.decode_ty(r, pkg, inner)?);
                }
                Value::Array(items)
            }
            Ty::Struct(members) => {
                let mut fields = Vec::new();
                for (name, t) in members {
                    fields.push((name.clone(), self.decode_ty(r, pkg, t)?));
                }
                Value::Struct(fields)
            }
            Ty::Delegate => return None,
        })
    }

    fn apply_block(
        &mut self,
        pkg: usize,
        block: &PropertyBlock,
        layout: &ClassLayout,
        values: &mut [Value],
    ) {
        let p = &self.set.packages[pkg];
        for q in &block.properties {
            let name = p.package.property_name(q);
            let Some(slot) = layout.slot_by_name(name) else {
                self.load_warnings.push(format!(
                    "{}: property {name} not in class layout",
                    self.set.path(layout.class)
                ));
                continue;
            };
            let idx = q.array_index as usize;
            if idx >= slot.dim {
                self.load_warnings
                    .push(format!("{name}[{idx}] outside dimension {}", slot.dim));
                continue;
            }
            values[slot.base + idx] = self.tagged_value(pkg, q, &slot.ty);
        }
    }

    /// Creates an instance of a class with its defaults.
    pub fn spawn(&mut self, class: GlobalRef, name: &str) -> VmResult<ObjectId> {
        self.spawn_depth(class, name, 0)
    }

    /// [`Vm::spawn`] with a recursion guard for per-instance default subobjects.
    fn spawn_depth(&mut self, class: GlobalRef, name: &str, depth: u8) -> VmResult<ObjectId> {
        let layout = self.class_layout(class)?;
        let is_actor = layout.chain_names.iter().any(|n| n == "actor");
        let id = self.objects.len() as ObjectId;
        if self.profile.enabled {
            self.profile.spawn_count += 1;
        }
        let cap_before = self.objects.capacity();
        let mut props = layout.defaults.clone();
        // UE2 `Object.Class` is a native property that always answers the object's UClass; it is
        // not a serialized default. Scripts read `default.Class` / `self.Class` to identify a
        // class (for example `LocalMessage.ClientReceive` passes `default.Class` to the HUD, and
        // `XIIISaveMessage.GetString` returns `default.CheckpointReached`). Without this, an
        // object's `Class` default stayed `None` and static dispatch from it returned `Void`,
        // suspending the HUD message widgets.
        if let Some(slot) = layout.slot_by_name("class") {
            props[slot.base] = Value::Object(Some(ObjRef::Static(class)));
        }
        // `Disable`/`Enable` set `bProbesDisabled`; the actor carries that flag from its class
        // defaults at construction, so a `Disable` before the first `Tick` is already effective
        // and `Enable` can clear it (Engine.dll `AActor+0x34` bit 0x1, `?execDisable@AActor`).
        let probes_disabled = layout
            .slot_by_name("bprobesdisabled")
            .is_some_and(|s| matches!(props[s.base], Value::Bool(true)));
        self.objects.push(Instance {
            class,
            name: name.to_owned(),
            props,
            layout,
            state: None,
            state_code: None,
            generation: 0,
            disabled: HashSet::new(),
            destroying: false,
            probes_disabled,
            active: false,
            suspended: false,
            is_actor,
            deleted: false,
            tick_fn: None,
            export: None,
            timers: [None, None, None],
            anim: AnimState::default(),
            bone: BoneState::default(),
        });
        if self.profile.enabled && self.objects.capacity() != cap_before {
            self.profile.objects_reallocs += 1;
        }
        // UE2 gives every instance its own copy of the class-default subobjects (component
        // objects) its default properties reference. The serialized class defaults hold `Static`
        // references to those class-package exports, which have no VM instance of their own;
        // expand them now so script property access through the reference resolves to an
        // instance. Evidence: `xidcine.BreakableMover.InitializeEmitters` 0x006A writes
        // `emit.Emitters[0].StartVelocityRange` on a `XIIIBreakingGlassEmitter` spawned from
        // `Fragments_Type`, whose `Emitters[0]` is the class subobject
        // `xidcine.XIIIBreakingGlassEmitter.XIIIBreakingGlassEmitterA`.
        let mut values = std::mem::take(&mut self.objects[id as usize].props);
        self.expand_default_subobjects(&mut values, id, depth)?;
        self.objects[id as usize].props = values;
        Ok(id)
    }

    /// Depth guard for per-instance default-subobject expansion (a cyclic reference graph would
    /// otherwise recurse forever; UE2's component chains are shallow).
    const MAX_SUBOBJECT_DEPTH: u8 = 16;

    /// Replaces every class-default-subobject reference in `values` with a fresh per-instance
    /// copy whose `Outer` is `owner`. Arrays and struct fields are walked. See
    /// [`Vm::class_subobject_class`].
    fn expand_default_subobjects(
        &mut self,
        values: &mut [Value],
        owner: ObjectId,
        depth: u8,
    ) -> VmResult<()> {
        if depth >= Self::MAX_SUBOBJECT_DEPTH {
            return Ok(());
        }
        for v in values.iter_mut() {
            self.expand_default_value(v, owner, depth)?;
        }
        Ok(())
    }

    fn expand_default_value(&mut self, v: &mut Value, owner: ObjectId, depth: u8) -> VmResult<()> {
        match v {
            Value::Object(Some(ObjRef::Static(g))) => {
                let Some(class) = self.class_subobject_class(*g) else {
                    return Ok(());
                };
                let name = self.subobject_name(*g);
                let sub = self.spawn_depth(class, &name, depth + 1)?;
                self.set_property(
                    sub,
                    "Outer",
                    0,
                    Value::Object(Some(ObjRef::Instance(owner))),
                );
                // Apply the subobject export's own serialized template (its overridden values).
                self.apply_export_properties(*g, sub)?;
                // A template property may itself reference another class subobject.
                let mut values = std::mem::take(&mut self.objects[sub as usize].props);
                self.expand_default_subobjects(&mut values, sub, depth + 1)?;
                self.objects[sub as usize].props = values;
                *v = Value::Object(Some(ObjRef::Instance(sub)));
            }
            Value::Array(items) => {
                for item in items.iter_mut() {
                    self.expand_default_value(item, owner, depth)?;
                }
            }
            Value::Struct(fields) => {
                for (_, field) in fields.iter_mut() {
                    self.expand_default_value(field, owner, depth)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    /// Class of a class-default subobject export, or `None` when `g` is not one. A class
    /// subobject's outer chain consists of exports and reaches a decoded `Core.Class` export
    /// (e.g. `xidcine.XIIIBreakingGlassEmitter.XIIIBreakingGlassEmitterA`, whose outer is the
    /// `XIIIBreakingGlassEmitter` class). A map export's outer is the map package import, so it
    /// is never treated as a subobject.
    fn class_subobject_class(&self, g: GlobalRef) -> Option<GlobalRef> {
        let p = self.set.packages.get(g.package)?;
        let e = p.package.exports().get(g.export as usize)?;
        let mut outer = p.package.object_outer(ObjectRef::Export(g.export))?;
        let mut steps = 0u32;
        loop {
            steps += 1;
            if steps > 64 {
                return None;
            }
            let ObjectRef::Export(oi) = outer else {
                return None;
            };
            let owner = GlobalRef {
                package: g.package,
                export: oi,
            };
            if matches!(self.set.object(owner), Some(ScriptObject::Class(_))) {
                return self.set.resolve(g.package, e.class);
            }
            outer = p.package.object_outer(ObjectRef::Export(oi))?;
        }
    }

    /// Short unique name for a class-subobject instance (its export's own name).
    fn subobject_name(&self, g: GlobalRef) -> String {
        let short = self.set.packages[g.package].ref_name(ObjectRef::Export(g.export));
        self.unique_name(short)
    }

    /// Applies an export's own tagged properties (a class-default subobject's serialized
    /// template) to a freshly spawned instance, mirroring the map-property pass in
    /// [`Vm::load_level`].
    fn apply_export_properties(&mut self, g: GlobalRef, id: ObjectId) -> VmResult<()> {
        let set = self.set;
        let p = &set.packages[g.package];
        let props = p
            .package
            .read_object_properties(&p.data, g.export as usize, &Limits::default())
            .map_err(|e| {
                self.err(VmErrorKind::Other(format!(
                    "class subobject properties of {}: {e}",
                    self.objects[id as usize].name
                )))
            })?;
        let layout = self.objects[id as usize].layout.clone();
        let mut values = std::mem::take(&mut self.objects[id as usize].props);
        self.apply_block(g.package, &props.block, &layout, &mut values);
        self.objects[id as usize].props = values;
        Ok(())
    }

    /// Instantiates every script-class export of a loaded map package (two passes: create, then
    /// apply the map's tagged properties). Returns the created Actor ids in export order; non-actor
    /// subobjects are created and property-loaded but not returned for lifecycle.
    pub fn load_level(&mut self, map: usize, limits: &Limits) -> VmResult<Vec<ObjectId>> {
        let set = self.set;
        let p = &set.packages[map];
        // Every map export whose class resolves to a script class is instantiated: not only
        // `Actor`s but also their non-actor subobjects, which UE2 serialises as top-level map
        // exports (e.g. the `Engine.SpriteEmitter` elements of an `Emitter.Emitters` array).
        // Only the Actor-derived instances are returned for lifecycle; the rest exist so script
        // property access on them (`.Disabled = ...`) resolves to an instance, not a static
        // reference. Evidence: `xidcine.TrigerredEmitter.PostBeginPlay` walks `Emitters[i]`.
        let mut actors = Vec::new();
        let mut created = Vec::new();
        for (i, e) in p.package.exports().iter().enumerate() {
            let Some(class) = set.resolve(map, e.class) else {
                continue;
            };
            if !matches!(set.object(class), Some(ScriptObject::Class(_))) {
                continue;
            }
            if e.serial_size == 0 {
                continue;
            }
            let is_actor = self
                .class_layout(class)?
                .chain_names
                .iter()
                .any(|n| n == "actor");
            let name = p.ref_name(ObjectRef::Export(i as u32)).to_owned();
            let id = self.spawn(class, &name)?;
            let g = GlobalRef {
                package: map,
                export: i as u32,
            };
            self.objects[id as usize].export = Some(g);
            self.by_export.insert(g, id);
            created.push(id);
            if is_actor {
                actors.push(id);
            }
        }
        for &id in &created {
            let g = self.objects[id as usize].export.expect("set above");
            let props = p
                .package
                .read_object_properties(&p.data, g.export as usize, limits)
                .map_err(|e| {
                    self.err(VmErrorKind::Other(format!(
                        "map properties of {}: {e}",
                        self.objects[id as usize].name
                    )))
                })?;
            let layout = self.objects[id as usize].layout.clone();
            let mut values = std::mem::take(&mut self.objects[id as usize].props);
            self.apply_block(map, &props.block, &layout, &mut values);
            self.objects[id as usize].props = values;
            // item61: level-placed actors get the engine's `InitExecution` writes too (the
            // retail `AGenAlerte::InitExecution` initialises map-placed alert generators).
            if self.objects[id as usize].is_actor {
                self.apply_native_init_execution(id);
            }
        }
        // Cache the map's `LevelInfo` for the per-tick `NextURL` travel check.
        self.level_info = self.find_level_info();
        Ok(actors)
    }

    /// Object id by display name (case-insensitive).
    pub fn find_object(&self, name: &str) -> Option<ObjectId> {
        self.objects
            .iter()
            .position(|o| !o.deleted && o.name.eq_ignore_ascii_case(name))
            .map(|i| i as ObjectId)
    }

    /// Finds a live instance created from a package export by its full object path.
    /// Map subobjects such as `SpriteEmitter` are ordinary VM instances even though they do not
    /// derive from `Actor`; presentation systems use this lookup to read their authoritative
    /// script properties without replaying lifecycle events from the trace.
    pub fn find_export_instance(&self, path: &str) -> Option<ObjectId> {
        self.by_export.iter().find_map(|(g, &id)| {
            if self.objects.get(id as usize).is_none_or(|o| o.deleted) {
                return None;
            }
            let p = &self.set.packages[g.package];
            let object_path = p
                .package
                .object_path(ObjectRef::Export(g.export))
                .unwrap_or_default();
            object_path.eq_ignore_ascii_case(path).then_some(id)
        })
    }

    /// Class-level function by name, ignoring state shadowing. Host-driven verbs that the engine
    /// resolves against the class (for example the player's `Fire` while a weapon state defines an
    /// empty shadow) can call this instead of [`Vm::send_event`], which is state-aware.
    pub fn class_function(&self, id: ObjectId, name: &str) -> Option<GlobalRef> {
        self.find_function(id, name, false)
    }

    /// True when `id`'s class-chain names contain `needle` (case-insensitive). `find_level_info`
    /// uses it for `levelinfo`; the host uses it for the `XIDCine.BeachFinalFall` allow-list.
    pub fn class_chain_contains(&self, id: ObjectId, needle: &str) -> bool {
        self.objects
            .get(id as usize)
            .is_some_and(|o| o.layout.chain_names.iter().any(|n| n.contains(needle)))
    }

    /// Restricts direct calls to active objects for a partial-execution diagnostic.
    /// This is a harness policy, not an UnrealScript rule. Normal gameplay leaves it disabled.
    pub fn set_diagnostic_call_scope(&mut self, enabled: bool) {
        self.diagnostic_call_scope = enabled;
    }

    /// Marks an object for scheduled execution (ticks, timers and state code).
    pub fn set_active(&mut self, id: ObjectId, active: bool) {
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.active = active;
        }
    }

    /// True when the actor's whole-probe-disable bit is set (`Disable('All')`/`bProbesDisabled`).
    pub fn probes_disabled(&self, id: ObjectId) -> bool {
        self.objects
            .get(id as usize)
            .is_some_and(|o| o.probes_disabled)
    }

    /// True while `[Vm::destroy]` is running this actor's `Destroyed` event (the engine has not
    /// set `bDeleteMe` yet).
    pub fn is_destroying(&self, id: ObjectId) -> bool {
        self.objects.get(id as usize).is_some_and(|o| o.destroying)
    }

    /// Reads a property by name (first element).
    pub fn get_property(&self, id: ObjectId, name: &str) -> Option<&Value> {
        let o = self.objects.get(id as usize)?;
        let s = o.layout.slot_by_name(name)?;
        o.props.get(s.base)
    }

    /// Reads element `elem` of a fixed-array property by name (`None` when the property is
    /// absent or `elem` is outside the declared dimension).
    pub fn get_property_elem(&self, id: ObjectId, name: &str, elem: usize) -> Option<&Value> {
        let o = self.objects.get(id as usize)?;
        let s = o.layout.slot_by_name(name)?;
        if elem >= s.dim {
            return None;
        }
        o.props.get(s.base + elem)
    }

    /// Per-actor bone-control state (`Pawn.SpineYawControl` / `Actor.SetBoneDirection`).
    pub fn bone_state(&self, id: ObjectId) -> Option<&BoneState> {
        self.objects.get(id as usize).map(|o| &o.bone)
    }

    /// The actor's `Mesh` object as `(object path, class path)`, when it is set and non-null.
    /// The class path is e.g. `Engine.SkeletalMesh`; a host renderer uses it to decide whether
    /// it can decode the object. Both an exported (static) and a dynamically constructed
    /// (instance) `Mesh` are handled.
    pub fn mesh_object(&self, id: ObjectId) -> Option<(String, String)> {
        self.mesh_property_object(id, "Mesh")
    }

    /// Resolved StaticMesh for native third-person InventoryAttachment actors.
    pub fn static_mesh_object(&self, id: ObjectId) -> Option<(String, String)> {
        self.mesh_property_object(id, "StaticMesh")
    }

    fn mesh_property_object(&self, id: ObjectId, property: &str) -> Option<(String, String)> {
        let r = match self.get_property(id, property) {
            Some(Value::Object(Some(r))) => *r,
            _ => return None,
        };
        let path = self.ref_path(&r);
        if path.is_empty() {
            return None;
        }
        let class = match r {
            ObjRef::Static(g) => self.class_path_of(g)?,
            ObjRef::Instance(i) => {
                let o = self.objects.get(i as usize)?;
                self.set.path(o.class)
            }
            // A mesh in a package outside the script set (e.g. a `.ukx`): its recorded class.
            ObjRef::External(e) => self.external_object(e)?.class?,
        };
        Some((path, class))
    }

    /// Read-only animation state of `id`: candidate sources and every channel. `None` when the
    /// object is not live; an object with no channels yields empty channels (the host samples the
    /// bind pose). This is the whole API a host renderer needs; it does not mutate the VM.
    pub fn actor_animation(&self, id: ObjectId) -> Option<ActorAnimation> {
        let o = self.objects.get(id as usize)?;
        let channels = o
            .anim
            .channels
            .iter()
            .map(|(&channel, c)| AnimChannelState {
                channel,
                sequence: c.sequence.clone(),
                frame: c.frame,
                rate: c.rate,
                frames: c.frames,
                looping: c.looping,
                active: c.active,
                tween_source: c.tween_source.clone(),
                tween_remaining: c.tween_remaining,
                tween_duration: c.tween_duration,
                tween_only: c.tween_only,
                blend_alpha: o
                    .anim
                    .blend_params
                    .get(&i32::from(channel))
                    .map_or(if channel == 0 { 1.0 } else { 0.0 }, |p| p.blend_alpha),
                blend_in: o
                    .anim
                    .blend_params
                    .get(&i32::from(channel))
                    .map_or(0.0, |p| p.in_time),
                blend_out: o
                    .anim
                    .blend_params
                    .get(&i32::from(channel))
                    .map_or(0.0, |p| p.out_time),
                blend_bone: o
                    .anim
                    .blend_params
                    .get(&i32::from(channel))
                    .and_then(|p| p.bone_name.clone()),
            })
            .collect();
        Some(ActorAnimation {
            sources: self.animation_sources(id),
            channels,
        })
    }

    /// `Pawn.SpineYawControl`: store the parameters for the renderer.
    pub(crate) fn set_spine_control(
        &mut self,
        id: ObjectId,
        is_controlled: bool,
        max_value: i32,
        rotation_speed: f32,
    ) {
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.bone.spine = Some(SpineControl {
                is_controlled,
                max_value,
                rotation_speed,
            });
        }
    }

    /// `Actor.SetBoneDirection`: record the request for the renderer.
    pub(crate) fn add_bone_direction(
        &mut self,
        id: ObjectId,
        bone: String,
        turn: [i32; 3],
        trans: [f32; 3],
        alpha: f32,
    ) {
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.bone
                .directions
                .retain(|c| !(c.bone.eq_ignore_ascii_case(&bone)));
            o.bone.directions.push(BoneDirection {
                bone,
                turn,
                trans,
                alpha,
            });
        }
    }

    /// `Actor.SetBoneScalePerAxis`: record the request for the renderer.
    pub(crate) fn add_bone_scale(
        &mut self,
        id: ObjectId,
        slot: i32,
        scale: [f32; 3],
        bone: String,
    ) {
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.bone.scales.retain(|c| c.slot != slot);
            o.bone.scales.push(BoneScale { slot, scale, bone });
        }
    }

    /// `Actor.SetBoneRotation`: record the request for the renderer.
    pub(crate) fn add_bone_rotation(
        &mut self,
        id: ObjectId,
        bone: String,
        turn: [i32; 3],
        space: i32,
        alpha: f32,
    ) {
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.bone
                .rotations
                .retain(|c| !(c.bone.eq_ignore_ascii_case(&bone)));
            o.bone.rotations.push(BoneRotation {
                bone,
                turn,
                space,
                alpha,
            });
        }
    }

    /// `Actor.SetBoneLocation`: record the request for the renderer.
    pub(crate) fn add_bone_location(
        &mut self,
        id: ObjectId,
        bone: String,
        trans: [f32; 3],
        alpha: f32,
    ) {
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.bone
                .locations
                .retain(|c| !(c.bone.eq_ignore_ascii_case(&bone)));
            o.bone.locations.push(BoneLocation { bone, trans, alpha });
        }
    }

    /// Clears the per-actor bone-control state (used by `IAController.HalteAuFeu`).
    pub(crate) fn reset_bone_state(&mut self, id: ObjectId) {
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.bone = BoneState::default();
        }
    }

    /// Writes a property by name and element.
    pub fn set_property(&mut self, id: ObjectId, name: &str, elem: usize, v: Value) -> bool {
        if std::env::var_os("XIII_WATCH_PLAYER_HEALTH").is_some()
            && self.objects.get(id as usize).is_some_and(|o| {
                o.is_actor && o.layout.chain_names.iter().any(|c| c == "xiiiplayerpawn")
            })
            && name.eq_ignore_ascii_case("health")
        {
            let writer = self.stack.last().map_or("<host>", |s| s.function.as_str());
            let offset = self.stack.last().map_or(0, |s| s.offset);
            eprintln!(
                "[vm-health-write] t={:.6} pawn={} writer={} offset=0x{:04X} value={}",
                self.time,
                self.objects[id as usize].name,
                writer,
                offset,
                self.value_text(&v)
            );
        }
        let Some(o) = self.objects.get_mut(id as usize) else {
            return false;
        };
        let Some(s) = o.layout.slot_by_name(name) else {
            return false;
        };
        if elem >= s.dim {
            return false;
        }
        let i = s.base + elem;
        o.props[i] = v;
        true
    }

    /// Adjust the `value` member of the `MusicVars` entry named `name` (case-insensitive) by
    /// `delta`, returning the new value. `None` when the property or entry is absent — the
    /// `LevelInfo.{Inc,Dec}{Attente,Alerte}` counters live there.
    pub(crate) fn adjust_music_var(&mut self, id: ObjectId, name: &str, delta: i32) -> Option<i32> {
        let arr = match self.get_property(id, "MusicVars")? {
            Value::Array(a) => a.clone(),
            _ => return None,
        };
        let mut new_value = None;
        let mut out = Vec::with_capacity(arr.len());
        for v in arr {
            if let Value::Struct(mut fields) = v {
                let is_entry = fields.iter().any(|(n, fv)| {
                    n.eq_ignore_ascii_case("name")
                        && matches!(fv, Value::Str(s) if s.eq_ignore_ascii_case(name))
                });
                if is_entry {
                    for (n, fv) in fields.iter_mut() {
                        if n.eq_ignore_ascii_case("value")
                            && let Value::Int(i) = fv
                        {
                            *i = i.wrapping_add(delta);
                            new_value = Some(*i);
                        }
                    }
                }
                out.push(Value::Struct(fields));
            } else {
                out.push(v);
            }
        }
        self.set_property(id, "MusicVars", 0, Value::Array(out));
        new_value
    }

    /// Engine.dll 0x103e4750/0x103e4870: indexed MusicVars[2].Value, not a name search.
    /// The native uses integer wrapping and does not clamp unbalanced decrements.
    pub(crate) fn adjust_attack_music_var(&mut self, id: ObjectId, delta: i32) -> VmResult<()> {
        let Some(Value::Array(mut entries)) = self.get_property(id, "MusicVars").cloned() else {
            return Err(self.err(VmErrorKind::Other(
                "LevelInfo attack counter requires MusicVars[2].Value".into(),
            )));
        };
        let value = entries.get_mut(2).and_then(|entry| match entry {
            Value::Struct(fields) => fields.iter_mut().find_map(|(name, value)| {
                if name.eq_ignore_ascii_case("value") {
                    Some(value)
                } else {
                    None
                }
            }),
            _ => None,
        });
        let Some(Value::Int(value)) = value else {
            return Err(self.err(VmErrorKind::Other(
                "LevelInfo attack counter requires integer MusicVars[2].Value".into(),
            )));
        };
        *value = value.wrapping_add(delta);
        self.set_property(id, "MusicVars", 0, Value::Array(entries));
        self.note(TraceKind::Note("LevelInfo attack MusicVars updated; audio-device SetMusicVar/attack-mode transition not bridged".into()));
        Ok(())
    }

    /// Name of the current state.
    pub fn state_name(&self, id: ObjectId) -> Option<String> {
        self.objects
            .get(id as usize)?
            .state
            .map(|g| self.object_name(g).to_owned())
    }

    /// `Object.IsInState`: true when the object's current state, or one of its super states,
    /// has the given name. `None` matches an object with no state.
    pub fn is_in_state(&self, id: ObjectId, name: &str) -> bool {
        let mut st = self.objects.get(id as usize).and_then(|o| o.state);
        if st.is_none() {
            return name.eq_ignore_ascii_case("None");
        }
        let mut guard = 0;
        while let Some(s) = st {
            if self.object_name(s).eq_ignore_ascii_case(name) {
                return true;
            }
            guard += 1;
            if guard > 64 {
                break;
            }
            st = self
                .struct_header(s)
                .and_then(|h| self.set.resolve(s.package, h.field.super_field));
        }
        false
    }

    fn default_object(&mut self, class: GlobalRef) -> VmResult<ObjectId> {
        if let Some(id) = self.default_objects.get(&class) {
            return Ok(*id);
        }
        let name = format!("Default__{}", self.object_name(class));
        let id = self.spawn(class, &name)?;
        self.default_objects.insert(class, id);
        Ok(id)
    }

    // ------------------------------------------------------------------ class queries

    /// True when `id`'s class chain contains a class named `name`.
    pub fn is_a(&self, id: ObjectId, name: &str) -> bool {
        self.objects.get(id as usize).is_some_and(|o| {
            o.layout
                .chain_names
                .iter()
                .any(|n| n.eq_ignore_ascii_case(name))
        })
    }

    fn is_child_of(&self, class: GlobalRef, base: GlobalRef) -> bool {
        self.class_chain(class).contains(&base)
    }

    /// Class-chain test used by `ClassIsChildOf` (both operands are classes).
    pub fn is_child_of_class(&self, class: GlobalRef, base: GlobalRef) -> bool {
        self.class_chain(class).contains(&base)
    }

    /// Class path string of an export as recorded in its package (`Core.Class`,
    /// `Engine.StaticMesh`, ...), or `None` when the package/export is unknown.
    pub fn class_path_of(&self, g: GlobalRef) -> Option<String> {
        self.set
            .packages
            .get(g.package)?
            .package
            .export_class_path(g.export as usize)
            .map(str::to_owned)
    }

    /// Loaded object by a `Package.Object.Path` (or bare `Class`/`Object`) name, for the subset
    /// of `DynamicLoadObject` that resolves names already present in the script set. Returns the
    /// export even when it is not instantiated.
    pub fn find_loaded_object(&self, name: &str) -> Option<GlobalRef> {
        let (package, path) = match name.split_once('.') {
            Some((p, rest)) => (Some(p), rest),
            None => (None, name),
        };
        if let Some(pkg) = package {
            let pi = self.set.package_index(pkg)?;
            let export = self.set.packages[pi].export_by_path(path)?;
            return Some(GlobalRef {
                package: pi,
                export,
            });
        }
        for pi in 0..self.set.packages.len() {
            if let Some(export) = self.set.packages[pi].export_by_path(path) {
                return Some(GlobalRef {
                    package: pi,
                    export,
                });
            }
        }
        None
    }

    fn find_function_in(&self, scope: GlobalRef, name: &str) -> Option<GlobalRef> {
        let p = &self.set.packages[scope.package];
        let h = self.struct_header(scope)?;
        let mut child = h.children;
        let mut guard = 0;
        while let ObjectRef::Export(e) = child {
            guard += 1;
            if guard > 65_536 {
                return None;
            }
            let obj = p.objects.get(&e)?;
            if matches!(obj, ScriptObject::Function(_))
                && p.ref_name(child).eq_ignore_ascii_case(name)
            {
                return Some(GlobalRef {
                    package: scope.package,
                    export: e,
                });
            }
            child = obj.field().next;
        }
        None
    }

    /// Virtual lookup: current state (and its super states), then the class chain.
    fn find_function(&self, id: ObjectId, name: &str, use_state: bool) -> Option<GlobalRef> {
        let o = &self.objects[id as usize];
        if use_state {
            let mut st = o.state;
            let mut guard = 0;
            while let Some(s) = st {
                guard += 1;
                if guard > 64 {
                    break;
                }
                if let Some(f) = self.find_function_in(s, name) {
                    return Some(f);
                }
                st = self
                    .struct_header(s)
                    .and_then(|h| self.set.resolve(s.package, h.field.super_field));
            }
        }
        o.layout
            .chain
            .iter()
            .find_map(|c| self.find_function_in(*c, name))
    }

    fn find_state(&self, id: ObjectId, name: &str) -> Option<GlobalRef> {
        let o = &self.objects[id as usize];
        let auto = name.eq_ignore_ascii_case("Auto");
        for c in &o.layout.chain {
            let p = &self.set.packages[c.package];
            let Some(h) = self.struct_header(*c) else {
                continue;
            };
            let mut child = h.children;
            let mut guard = 0;
            while let ObjectRef::Export(e) = child {
                guard += 1;
                if guard > 65_536 {
                    break;
                }
                let Some(obj) = p.objects.get(&e) else { break };
                if let ScriptObject::State(s) = obj {
                    let hit = if auto {
                        s.state.state_flags & 2 != 0
                    } else {
                        p.ref_name(child).eq_ignore_ascii_case(name)
                    };
                    if hit {
                        return Some(GlobalRef {
                            package: c.package,
                            export: e,
                        });
                    }
                }
                child = obj.field().next;
            }
        }
        None
    }

    fn find_label(&self, state: GlobalRef, label: &str) -> Option<(GlobalRef, u32)> {
        let mut st = Some(state);
        let mut guard = 0;
        while let Some(s) = st {
            guard += 1;
            if guard > 64 {
                return None;
            }
            let h = self.struct_header(s)?;
            let p = &self.set.packages[s.package];
            for l in h.script.labels() {
                if p.name_text(l.name).eq_ignore_ascii_case(label) {
                    return Some((s, l.offset));
                }
            }
            st = self.set.resolve(s.package, h.field.super_field);
        }
        None
    }

    // ------------------------------------------------------------------ public execution

    /// Calls a script event on an object as the engine would (honours `Disable`). Returns
    /// `Ok(None)` when the probe is disabled or the class has no handler.
    ///
    /// A `bDeleteMe` actor receives no `ProcessEvent` at all: `Core.dll ?ProcessEvent@UObject`
    /// (0x1011e880) calls the object's `IsPendingKill` vtable slot (`vtable+0x38`, 0x10101e60 for
    /// `UObject`; `Engine.dll ?IsPendingKill@AActor` 0x10304b40 returns `AActor+0x2e & 1`, the
    /// `bDeleteMe` byte) and returns without running the function when it is non-zero. A probe
    /// disabled for this event, or a `Disable('All')` probe set (see [`Vm::disable_probe`]), also
    /// drops the event visibly.
    pub fn send_event(
        &mut self,
        id: ObjectId,
        event: &str,
        args: Vec<Value>,
    ) -> VmResult<Option<Value>> {
        self.steps = 0;
        let actor = self.objects[id as usize].name.clone();
        if self.objects[id as usize].probes_disabled
            || self.objects[id as usize].disabled.contains(&lower(event))
        {
            self.note(TraceKind::ProbeDisabled {
                actor,
                probe: event.to_owned(),
            });
            return Ok(None);
        }
        if self.objects[id as usize].deleted && !self.objects[id as usize].destroying {
            // ProcessEvent on a bDeleteMe actor is a no-op (no handler even runs).
            self.note(TraceKind::Note(format!(
                "{actor}.{event}: skipped, actor has bDeleteMe (ProcessEvent drops deleted actors)"
            )));
            return Ok(None);
        }
        // The native `AMapInfo::PostBeginPlay` licence layer runs when the event is delivered
        // to a live MapInfo instance, with or without a script handler (the retail native is
        // itself the C++ event; the table write follows the super call in the disassembly).
        let mapinfo_licence = event.eq_ignore_ascii_case("PostBeginPlay")
            && self.is_live_actor(id)
            && self.is_a(id, "mapinfo");
        let Some(f) = self.find_function(id, event, true) else {
            if mapinfo_licence {
                self.write_native_tgs_dummy(id)?;
            }
            self.note(TraceKind::NoHandler {
                actor,
                event: event.to_owned(),
            });
            return Ok(None);
        };
        let texts = args.iter().map(|a| self.value_text(a)).collect();
        self.note(TraceKind::Event {
            target: actor,
            function: self.short_path(f),
            args: texts,
        });
        let result = self.call_values(f, id, args);
        if mapinfo_licence {
            self.write_native_tgs_dummy(id)?;
        }
        result.map(Some)
    }

    /// The Xiii.dll `AMapInfo::PostBeginPlay` licence write: sets `TGSDummy` from the
    /// per-map table ([`native_tgs_dummy`], keyed by the instance's `iLoadSpecificValue`).
    /// Never traced for maps outside the table (no write, no note), so traces of maps that
    /// do not use the flag are byte-identical to before.
    fn write_native_tgs_dummy(&mut self, id: ObjectId) -> VmResult<()> {
        let load_specific = match self.get_property(id, "iLoadSpecificValue") {
            Some(Value::Int(n)) => *n,
            _ => return Ok(()),
        };
        let Some(value) = native_tgs_dummy(load_specific) else {
            return Ok(());
        };
        if !self.set_property(id, "TGSDummy", 0, Value::Int(value)) {
            return Err(self.err(VmErrorKind::Other(format!(
                "{}: native licence write: no TGSDummy property",
                self.objects[id as usize].name
            ))));
        }
        self.note(TraceKind::Note(format!(
            "{}.PostBeginPlay: native licence table sets TGSDummy={} (iLoadSpecificValue {})",
            self.objects[id as usize].name, value, load_specific
        )));
        Ok(())
    }

    /// Calls a function by global reference with argument values (no out parameters).
    pub fn call_function(
        &mut self,
        func: GlobalRef,
        this: ObjectId,
        args: Vec<Value>,
    ) -> VmResult<Value> {
        self.steps = 0;
        self.call_values(func, this, args)
    }

    /// Invokes a delegate property from outside script (the host's GUI render loop calls the
    /// page/control `__OnPreDraw__`/`__OnDraw__` delegates this way).
    ///
    /// Reads the delegate `property` from `context`. A bound delegate calls its own
    /// `(object, function)`; an unbound/absent one calls `declared` on `context` — the same
    /// fallback as the `DelegateFunction` (`0x43`) opcode. Fails explicitly when the target
    /// function does not exist; it never silently draws nothing.
    pub fn call_delegate(
        &mut self,
        context: ObjectId,
        property: &str,
        declared: &str,
        args: Vec<Value>,
    ) -> VmResult<Value> {
        self.steps = 0;
        let bound = match self.get_property(context, property) {
            Some(Value::Delegate(Some(d))) => Some(d.clone()),
            Some(Value::Delegate(None)) | None => None,
            Some(other) => return Err(self.type_err("delegate", other)),
        };
        match bound {
            Some(d) => {
                let obj = match d.object {
                    Some(ObjRef::Instance(i)) => i,
                    Some(ObjRef::Static(g)) => self.default_object(g)?,
                    Some(ObjRef::External(_)) | None => context,
                };
                let f = self.find_function(obj, &d.function, true).ok_or_else(|| {
                    self.err(VmErrorKind::NoSuchFunction {
                        object: self.objects[obj as usize].name.clone(),
                        name: d.function.clone(),
                    })
                })?;
                self.call_values(f, obj, args)
            }
            None => {
                let f = self.find_function(context, declared, true).ok_or_else(|| {
                    self.err(VmErrorKind::NoSuchFunction {
                        object: self.objects[context as usize].name.clone(),
                        name: declared.to_owned(),
                    })
                })?;
                self.call_values(f, context, args)
            }
        }
    }

    /// `GotoState` from outside script (harness/tests).
    pub fn goto_state(&mut self, id: ObjectId, state: &str, label: Option<&str>) -> VmResult<()> {
        self.steps = 0;
        self.do_goto_state(id, state, label.unwrap_or("Begin"))
    }

    /// One fixed step: timers, then state code of every active object (in id order).
    pub fn tick(&mut self, dt: f32) -> VmResult<()> {
        self.tick_count += 1;
        self.time += f64::from(dt);
        self.steps = 0;
        for id in 0..self.objects.len() as ObjectId {
            if !self.objects[id as usize].active {
                continue;
            }
            for (slot, event) in TIMER_EVENTS.iter().enumerate() {
                let mut fire = false;
                if let Some(t) = self.objects[id as usize].timers[slot].as_mut() {
                    t.remaining -= dt;
                    if t.remaining <= 0.0 {
                        fire = true;
                        if t.repeat {
                            t.remaining += t.rate;
                        }
                    }
                }
                if fire {
                    if !self.objects[id as usize].timers[slot]
                        .as_ref()
                        .is_some_and(|t| t.repeat)
                    {
                        self.objects[id as usize].timers[slot] = None;
                    }
                    let actor = self.objects[id as usize].name.clone();
                    self.note(TraceKind::Timer { actor });
                    if let Some(f) = self.find_function(id, event, true) {
                        self.call_values(f, id, Vec::new())?;
                    }
                }
            }
        }
        // Animation playback advances before state code, so a `FinishAnim` waiter resumes in
        // the same tick its channel ends (UE2 `AActor::Tick` order).
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active {
                self.advance_animation(id, dt)?;
            }
        }
        // Movers advance in the same pre-state slice (UE2 `physInterpolating` runs in Tick), so
        // a `FinishInterpolation` waiter resumes in the tick its `bInterpolating` clears.
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active {
                self.advance_interpolation(id, dt)?;
            }
        }
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active {
                self.process_state(id, dt)?;
            }
        }
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active && self.objects[id as usize].is_actor {
                self.dispatch_tick(id, dt)?;
            }
        }
        self.dispatch_player_ticks(dt)?;
        self.detect_server_travel();
        Ok(())
    }

    /// Reports a level-travel request when the script called `LevelInfo.ServerTravel` and the
    /// engine field `NextURL` became non-empty. The real engine's tick consumes `NextURL` and
    /// loads the map; the headless VM only reports it ([`Vm::take_travel_request`]). `ClientTravel`
    /// requests take precedence (the `pending_travel` guard), and each distinct URL is reported
    /// once.
    fn detect_server_travel(&mut self) {
        let Some(level) = self.level_info else {
            return;
        };
        let url = match self.get_property(level, "NextURL") {
            Some(Value::Str(s)) if !s.is_empty() => s.clone(),
            _ => return,
        };
        if url == self.last_next_url {
            return;
        }
        self.last_next_url = url.clone();
        if self.pending_travel.is_some() {
            return;
        }
        let items = matches!(
            self.get_property(level, "bNextItems"),
            Some(Value::Bool(true))
        );
        let actor = self.objects[level as usize].name.clone();
        let time = self.time;
        self.request_travel(TravelRequest {
            actor,
            url,
            mode: 0,
            items,
            source: TravelSource::ServerTravel,
            time,
        });
    }

    /// Like [`Vm::tick`], but a failing actor is **suspended** (`active = false`) and the failure
    /// is returned instead of aborting the whole world. The remaining actors still run their
    /// timers, animation and state code in the same tick. The Bevy host uses this so one
    /// unimplemented native cannot freeze the play window.
    ///
    /// The suspended actor is the **innermost object on the error stack** (the code that actually
    /// failed), not necessarily the actor whose tick called it. This matters for trigger chains:
    /// a triggered actor's failure must not suspend the triggerer. When the innermost object
    /// cannot be resolved, the ticked actor is suspended instead. The returned vector is
    /// `(suspended actor, error)` per failure, in processing order; the error is never silently
    /// swallowed.
    pub fn tick_suspending(&mut self, dt: f32) -> Vec<(ObjectId, VmError)> {
        self.tick_count += 1;
        self.time += f64::from(dt);
        self.steps = 0;
        let mut errors = Vec::new();
        let profiling = self.profile.enabled;
        let t0 = Instant::now();
        for id in 0..self.objects.len() as ObjectId {
            if !self.objects[id as usize].active {
                continue;
            }
            for (slot, event) in TIMER_EVENTS.iter().enumerate() {
                let mut fire = false;
                if let Some(t) = self.objects[id as usize].timers[slot].as_mut() {
                    t.remaining -= dt;
                    if t.remaining <= 0.0 {
                        fire = true;
                        if t.repeat {
                            t.remaining += t.rate;
                        }
                    }
                }
                if fire {
                    if !self.objects[id as usize].timers[slot]
                        .as_ref()
                        .is_some_and(|t| t.repeat)
                    {
                        self.objects[id as usize].timers[slot] = None;
                    }
                    let actor = self.objects[id as usize].name.clone();
                    self.note(TraceKind::Timer { actor });
                    if let Some(f) = self.find_function(id, event, true)
                        && let Err(e) = self.call_values(f, id, Vec::new())
                    {
                        let suspended = self.suspend_for_error(id, &e);
                        errors.push((suspended, e));
                    }
                }
            }
        }
        if profiling {
            self.profile.timers_micros += t0.elapsed().as_micros() as u64;
        }
        let t0 = Instant::now();
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active
                && let Err(e) = self.advance_animation(id, dt)
            {
                let suspended = self.suspend_for_error(id, &e);
                errors.push((suspended, e));
            }
        }
        if profiling {
            self.profile.animation_micros += t0.elapsed().as_micros() as u64;
        }
        let t0 = Instant::now();
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active
                && let Err(e) = self.advance_interpolation(id, dt)
            {
                let suspended = self.suspend_for_error(id, &e);
                errors.push((suspended, e));
            }
        }
        if profiling {
            self.profile.movers_micros += t0.elapsed().as_micros() as u64;
        }
        let t0 = Instant::now();
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active
                && let Err(e) = self.process_state(id, dt)
            {
                let suspended = self.suspend_for_error(id, &e);
                errors.push((suspended, e));
            }
        }
        if profiling {
            self.profile.state_micros += t0.elapsed().as_micros() as u64;
        }
        let t0 = Instant::now();
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active
                && self.objects[id as usize].is_actor
                && let Err(e) = self.dispatch_tick(id, dt)
            {
                let suspended = self.suspend_for_error(id, &e);
                errors.push((suspended, e));
            }
        }
        if profiling {
            self.profile.player_tick_dispatch_micros += t0.elapsed().as_micros() as u64;
        }
        // item53: engine physics integration for script-driven actors, after the script Tick
        // phase (the engine integrates inside the actor's native Tick, after the script
        // callbacks) and before the focus/view rotation updates.
        let t0 = Instant::now();
        for (id, e) in self.advance_scripted_physics(dt) {
            errors.push((id, e));
        }
        if profiling {
            self.profile.scripted_physics_micros += t0.elapsed().as_micros() as u64;
        }
        self.update_focus_rotations(dt);
        let t0 = Instant::now();
        for id in 0..self.objects.len() as ObjectId {
            if self.player_tick_overridden(id)
                && let Err(e) = self.dispatch_player_tick(id, dt)
            {
                let suspended = self.suspend_for_error(id, &e);
                errors.push((suspended, e));
            }
        }
        if profiling {
            self.profile.player_tick_dispatch_micros += t0.elapsed().as_micros() as u64;
        }
        self.detect_server_travel();
        errors
    }

    /// `Class.Tick` label for the per-function dispatch profile (`--perf-natives`).
    fn tick_fn_key(&self, id: ObjectId) -> String {
        let cls = self.objects[id as usize]
            .layout
            .chain_names
            .first()
            .cloned()
            .unwrap_or_default();
        format!("{cls}.Tick")
    }

    /// `Tick` function resolved on `id`, memoised per instance and keyed by the current state.
    ///
    /// The result depends only on the instance's class chain (fixed for its life) and its
    /// current state; `do_goto_state` clears the memo whenever the state changes, so a memo hit
    /// is guaranteed to equal a fresh [`Vm::find_function`]. On a miss the fresh lookup is stored
    /// and returned. The first lookup for an instance with no state stores `None` so a class with
    /// no `Tick` handler is not re-scanned every frame either.
    fn tick_function(&mut self, id: ObjectId) -> Option<GlobalRef> {
        if let Some((state, f)) = self.objects[id as usize].tick_fn
            && state == self.objects[id as usize].state
        {
            if self.profile.enabled {
                self.profile.tick_lookups += 1;
                self.profile.tick_cache_hits += 1;
            }
            return f;
        }
        if self.profile.enabled {
            self.profile.tick_lookups += 1;
        }
        let state = self.objects[id as usize].state;
        let f = self.find_function(id, "Tick", true);
        self.objects[id as usize].tick_fn = Some((state, f));
        f
    }

    /// Fires the per-frame `Tick(DeltaTime)` event on one active actor. UE2's engine calls
    /// `AActor::Tick` (the script `event Tick`) each frame; the VM runs state code latently but
    /// must also dispatch `Tick` or per-frame script (the `CineController2` sequence interpreter,
    /// `XIIIBaseHud.Tick`, pawn controllers) never runs. `Tick` is looked up in the actor's
    /// current state first, then the class chain.
    fn dispatch_tick(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        let trace_cine = cine_trace_enabled() && self.is_a(id, "CineController2");
        let action_before =
            trace_cine.then(|| match self.get_property(id, "ScriptedActionIndex") {
                Some(Value::Int(index)) => Some(*index),
                _ => None,
            });
        let f = self.tick_function(id);
        if let Some(f) = f {
            let t0 = self.profile.enabled.then(Instant::now);
            self.call_values(f, id, vec![Value::Float(dt)])?;
            if let Some(t0) = t0 {
                let key = self.tick_fn_key(id);
                *self.profile.tick_fns.entry(key).or_default() += t0.elapsed().as_micros() as u64;
            }
        }
        if trace_cine {
            let action_after = match self.get_property(id, "ScriptedActionIndex") {
                Some(Value::Int(index)) => Some(*index),
                _ => None,
            };
            let phase = if action_before.flatten() != action_after {
                "advance"
            } else {
                "blocked/current"
            };
            self.trace_cinematic_controller(id, phase);
        }
        Ok(())
    }

    fn focus_rotation_complete(&self, controller: ObjectId) -> bool {
        let Some(pawn) = self.obj_prop(controller, "Pawn") else {
            return true;
        };
        let Some(target) = self
            .obj_prop(controller, "Focus")
            .and_then(|focus| self.vector_prop(focus, "Location"))
            .or_else(|| self.vector_prop(controller, "FocalPoint"))
        else {
            return true;
        };
        let location = self.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let dx = target[0] - location[0];
        let dy = target[1] - location[1];
        let dz = target[2] - location[2];
        let scale = 65536.0 / std::f32::consts::TAU;
        let desired = [
            (dz.atan2(dx.hypot(dy)) * scale).round() as i32,
            (dy.atan2(dx) * scale).round() as i32,
            0,
        ];
        let current = self.rotation_prop(pawn).unwrap_or([0; 3]);
        current
            .iter()
            .zip(desired)
            .all(|(a, b)| rotation_delta(*a, b).abs() <= 1)
    }

    /// Mirror focus-derived controller rotation and the pawn's Engine.dll physicsRotation.
    /// UpdateRotation supplies the controller's facing; APawn::physicsRotation then advances
    /// the pawn toward Controller.Rotation through FixedTurn and RotationRate.
    fn update_focus_rotations(&mut self, dt: f32) {
        if !dt.is_finite() || dt <= 0.0 {
            return;
        }
        let controllers: Vec<ObjectId> = self
            .objects
            .iter()
            .enumerate()
            .filter(|(i, o)| {
                o.is_actor && o.active && !o.deleted && self.is_a(*i as ObjectId, "controller")
            })
            .map(|(i, _)| i as ObjectId)
            .collect();
        for controller in controllers {
            // Combat focus steering is owned by AAIController/its game subclasses. CineController2
            // extends Controller directly (xidcine.u), but its PlayingSequence.Tick drives
            // FocalPoint (from rWantedRotation or the move direction) and sets
            // Pawn.RotationRate.Yaw itself (tick 0x066A), so the same rotateToward facing applies
            // to its pawn — the XIII demonstrator cutscenes rely on it. Player and scripted
            // controllers also use FinishRotation for view/cinematic work; their view rotation
            // follows player input and must not be treated as AI focus steering here.
            if !self.is_a(controller, "aicontroller") && !self.is_a(controller, "cinecontroller2") {
                continue;
            }
            let Some(pawn) = self.obj_prop(controller, "Pawn") else {
                continue;
            };
            if !self.bool_prop(pawn, "bRotateToDesired") {
                continue;
            }
            let focus_point = self
                .obj_prop(controller, "Focus")
                .and_then(|focus| self.vector_prop(focus, "Location"))
                .or_else(|| self.vector_prop(controller, "FocalPoint"));
            let Some(target) = focus_point else {
                continue;
            };
            let location = self.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
            let dx = target[0] - location[0];
            let dy = target[1] - location[1];
            let dz = target[2] - location[2];
            if dx == 0.0 && dy == 0.0 && dz == 0.0 {
                continue;
            }
            let horizontal = dx.hypot(dy);
            let scale = 65536.0 / std::f32::consts::TAU;
            let desired = [
                (dz.atan2(horizontal) * scale).round() as i32,
                (dy.atan2(dx) * scale).round() as i32,
                0,
            ];
            let _ = self.set_property(pawn, "DesiredRotation", 0, Value::Rotator(desired));
            let current = self.rotation_prop(pawn).unwrap_or([0; 3]);
            let rate = match self.get_property(pawn, "RotationRate") {
                Some(Value::Rotator(rate)) => *rate,
                _ => [0; 3],
            };
            let next = [
                rotation_step(current[0], desired[0], rate[0], dt),
                rotation_step(current[1], desired[1], rate[1], dt),
                rotation_step(current[2], desired[2], rate[2], dt),
            ];
            let _ = self.set_property(pawn, "Rotation", 0, Value::Rotator(next));
            // AController::Tick invokes APawn::rotateToward(FocalPoint), then copies the
            // pawn's current rotation back to the controller before pawn physics advances.
            let _ = self.set_property(controller, "Rotation", 0, Value::Rotator(current));
        }
    }

    /// Temporary, opt-in diagnostic for the authored XIDCine action interpreter. Kept in the VM
    /// so it observes the same actor state and decoded action table that `Interpret` consumes.
    fn trace_cinematic_controller(&self, id: ObjectId, phase: &str) {
        let obj = &self.objects[id as usize];
        let action_index = match self.get_property(id, "ScriptedActionIndex") {
            Some(Value::Int(i)) => *i,
            _ => -1,
        };
        // CineController2 increments ScriptedActionIndex after Interpret. The preceding entry is
        // the action just executed and, while paused, the action whose wait bit is still set.
        let pawn = self.obj_prop(id, "MyPawn");
        let controlled_pawn = self.obj_prop(id, "Pawn");
        let tab = pawn.and_then(|p| match self.get_property(p, "CurrentTabActionIndex") {
            Some(Value::Int(i)) => Some(*i),
            _ => None,
        });
        let list = pawn.and_then(|p| {
            let name = match tab.unwrap_or(0) {
                2 => "tabActions2",
                3 => "tabActions3",
                _ => "tabActions",
            };
            match self.get_property(p, name) {
                Some(Value::Array(items)) => Some(items),
                _ => None,
            }
        });
        let selected = action_index.saturating_sub(1);
        let action = list
            .and_then(|items| usize::try_from(selected).ok().and_then(|i| items.get(i)))
            .map_or_else(
                || "<action unavailable>".to_owned(),
                |v| match v {
                    Value::Str(s) | Value::Name(s) => s.clone(),
                    _ => format!("{v}"),
                },
            );
        let state = self.state_name(id).unwrap_or_else(|| "<no state>".into());
        let flags = match self.get_property(id, "flagsPaused") {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let mut waits = Vec::new();
        for (mask, label) in [
            (1, "player"),
            (2, "event"),
            (4, "warning"),
            (8, "speech/dial"),
            (16, "move/sequence"),
            (32, "see-player"),
            (64, "seen-by-player"),
            (128, "time"),
            (256, "animation"),
            (512, "not-seen-by-player"),
            (1024, "player-away"),
            (2048, "cadaver"),
        ] {
            if flags & mask != 0 {
                waits.push(label.to_owned());
            }
        }
        if flags & 2 != 0 {
            waits.push(format!(
                "event-name={}",
                self.get_property(id, "Tag")
                    .map_or_else(|| "<none>".into(), |v| format!("{v}"))
            ));
        }
        if flags & 4 != 0 {
            waits.push(format!(
                "WarnMemory={:?} warning-jump={:?}",
                self.get_property(id, "WarnMemory"),
                self.get_property_elem(id, "nOnJump", 2)
            ));
        }
        if flags & 16 != 0 {
            waits.push(format!("bMoving={:?}", self.get_property(id, "bMoving")));
        }
        if flags & 256 != 0 {
            waits.push(format!(
                "bAnimOnce={:?} bSubAnim={:?}",
                self.get_property(id, "bAnimOnce"),
                self.get_property(id, "bSubAnim")
            ));
        }
        if let Some(pawn) = pawn {
            let location = self.vector_prop(pawn, "Location");
            waits.push(format!("pawn-location={location:?}"));
            for property in ["Target", "NextTarget"] {
                if let Some(target) = self.obj_prop(id, property) {
                    let target_name = self.objects[target as usize].name.clone();
                    let target_location = self.vector_prop(target, "Location");
                    let distance = location.zip(target_location).map(|(from, to)| {
                        let delta = [to[0] - from[0], to[1] - from[1], to[2] - from[2]];
                        (delta[0] * delta[0] + delta[1] * delta[1] + delta[2] * delta[2]).sqrt()
                    });
                    waits.push(format!(
                        "{property}={target_name}@{target_location:?} distance={distance:?}"
                    ));
                }
            }
            for (&channel, animation) in &self.objects[pawn as usize].anim.channels {
                if animation.active {
                    waits.push(format!(
                        "channel{channel}={} frame={:.3}/{},rate={:.3}fps,loop={}",
                        animation.sequence,
                        animation.frame,
                        animation.frames,
                        animation.rate,
                        animation.looping
                    ));
                }
            }
        }
        if let Some(code) = obj.state_code.as_ref()
            && let Some(latent) = &code.latent
        {
            waits.push(format!("latent={latent:?}"));
        }
        println!(
            "[cine-trace] t={:.3}s tick={} phase={} actor={} pawn={} mypawn={} state={} label/tag={} action[{}]={:?} flagsPaused=0x{:X} wait={}",
            self.time,
            self.tick_count,
            phase,
            obj.name,
            controlled_pawn.map_or_else(
                || "<none>".into(),
                |p| self.objects[p as usize].name.clone()
            ),
            pawn.map_or_else(
                || "<none>".into(),
                |p| self.objects[p as usize].name.clone()
            ),
            state,
            self.get_property(id, "Tag")
                .map_or_else(|| "<none>".into(), |v| format!("{v}")),
            selected,
            action,
            flags,
            if waits.is_empty() {
                "<none>".into()
            } else {
                waits.join(",")
            }
        );
    }

    /// item18: per-frame `PlayerTick` dispatch to the local player controllers, after the actor
    /// `Tick` pass (UE2 `ULevel::Tick` order; [`Self::tick`] calls this at the same point).
    ///
    /// UE2's engine calls `APlayerController::PlayerTick(DeltaTime)` every frame, which runs the
    /// controller's `PlayerTick` event; a state can override it (`PlayingVideo.PlayerTick` ends the
    /// level-end video, `GameEndedDeath.PlayerTick` drives the death cam). The host owns the player
    /// pawn's movement (see `Session::step`), and the class-level
    /// `PlayerController.PlayerTick`/`PlayerWalking.PlayerMove` script **is** that movement: it
    /// reaches the engine movement natives `CheckBob` (#504) and `FindStairRotation` (#524), which
    /// the port replaces and does not register. Running it would double-move the pawn and suspend
    /// the controller. So this dispatches `PlayerTick` only when the controller's **current state**
    /// defines it — exactly the script the host does not own.
    fn dispatch_player_ticks(&mut self, dt: f32) -> VmResult<()> {
        for id in 0..self.objects.len() as ObjectId {
            if self.player_tick_overridden(id) {
                self.dispatch_player_tick(id, dt)?;
            }
        }
        Ok(())
    }

    /// True when `id` is an active, live `PlayerController` actor whose current state (or a
    /// super-state) defines `PlayerTick`. See [`Self::dispatch_player_ticks`].
    fn player_tick_overridden(&self, id: ObjectId) -> bool {
        let o = &self.objects[id as usize];
        o.active
            && o.is_actor
            && !o.deleted
            && self.is_a(id, "playercontroller")
            && self.state_defines_function(id, "PlayerTick")
    }

    /// Fires `PlayerTick(DeltaTime)` on one controller (the caller has already checked it is a
    /// state override).
    fn dispatch_player_tick(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        if let Some(f) = self.find_function(id, "PlayerTick", true) {
            self.call_values(f, id, vec![Value::Float(dt)])?;
        }
        Ok(())
    }

    /// True when the object's current state or one of its super-states defines `name` (so the
    /// lookup would resolve to a state function rather than the class chain).
    fn state_defines_function(&self, id: ObjectId, name: &str) -> bool {
        let mut st = self.objects[id as usize].state;
        let mut guard = 0;
        while let Some(s) = st {
            guard += 1;
            if guard > 64 {
                break;
            }
            if self.find_function_in(s, name).is_some() {
                return true;
            }
            st = self
                .struct_header(s)
                .and_then(|h| self.set.resolve(s.package, h.field.super_field));
        }
        false
    }

    /// Suspends the actor that should stop after a failing tick: the innermost object on the
    /// error stack when it can be resolved, otherwise the actor being ticked. Returns the id.
    fn suspend_for_error(&mut self, ticked: ObjectId, e: &VmError) -> ObjectId {
        #[cfg(test)]
        eprintln!("[item47 temporary suspension diagnostic] {e}");
        let id = e
            .stack
            .last()
            .and_then(|s| self.find_live_object(&s.object))
            .unwrap_or(ticked);
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.active = false;
            o.suspended = true;
        }
        id
    }

    /// Diagnostic-scope calls dropped on suspended objects. Normal dispatch never drops
    /// calls for tick suspension, so gameplay leaves this counter at zero.
    pub fn suspended_deferred_calls(&self) -> u64 {
        self.suspended_deferred_calls
    }

    // ------------------------------------------------------------------ state code

    pub(crate) fn do_goto_state(&mut self, id: ObjectId, state: &str, label: &str) -> VmResult<()> {
        let actor = self.objects[id as usize].name.clone();
        let new_state = if state.eq_ignore_ascii_case("None") {
            None
        } else {
            match self.find_state(id, state) {
                Some(s) => Some(s),
                None => {
                    self.note(TraceKind::StateNotFound {
                        actor,
                        state: state.to_owned(),
                    });
                    return Ok(());
                }
            }
        };
        let old = self.objects[id as usize].state;
        if old != new_state
            && old.is_some()
            && let Some(f) = self.find_function(id, "EndState", true)
        {
            self.call_values(f, id, Vec::new())?;
        }
        let code = match new_state {
            Some(s) => match self.find_label(s, label) {
                Some((owner, offset)) => {
                    let script = &self.struct_header(owner).expect("state").script;
                    let map = self.stmt_map(owner, script);
                    let pc = *map
                        .get(&offset)
                        .ok_or_else(|| self.err(VmErrorKind::BadJumpTarget { offset }))?;
                    Some(StateCode {
                        owner,
                        pc,
                        latent: None,
                    })
                }
                None => None,
            },
            None => None,
        };
        let has_code = code.is_some();
        {
            let o = &mut self.objects[id as usize];
            o.state = new_state;
            o.state_code = code;
            // Invalidate the memoised `Tick` resolution: the state is the only input to the
            // virtual lookup, and it just changed. A stale entry would call the old state's
            // `Tick`, exactly the wrong behaviour.
            o.tick_fn = None;
            o.generation += 1;
        }
        self.note(TraceKind::StateChange {
            actor,
            from: old.map(|g| self.object_name(g).to_owned()),
            to: new_state.map(|g| self.object_name(g).to_owned()),
            label: has_code.then(|| label.to_owned()),
        });
        if old != new_state
            && new_state.is_some()
            && let Some(f) = self.find_function(id, "BeginState", true)
        {
            self.call_values(f, id, Vec::new())?;
        }
        Ok(())
    }

    fn process_state(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        let set = self.set;
        let mut rounds = 0;
        loop {
            rounds += 1;
            if rounds > 10_000 {
                let mut error = self.err(VmErrorKind::BudgetExceeded {
                    limit: self.limits.max_steps,
                });
                if let Some(code) = &self.objects[id as usize].state_code {
                    let offset = self
                        .struct_header(code.owner)
                        .and_then(|h| h.script.statements.get(code.pc))
                        .map_or(0, |t| t.offset);
                    error.stack.push(StackEntry {
                        function: self.set.path(code.owner),
                        object: self.objects[id as usize].name.clone(),
                        offset,
                    });
                }
                return Err(error);
            }
            let Some(code) = self.objects[id as usize].state_code.clone() else {
                return Ok(());
            };
            match code.latent {
                Some(Latent::Sleep {
                    seconds,
                    remaining,
                    started,
                }) => {
                    // UE2 AActor::execPollSleep: finished once the remaining time drops below
                    // half a tick.
                    let left = remaining - dt;
                    if left >= 0.5 * dt {
                        if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                            c.latent = Some(Latent::Sleep {
                                seconds,
                                remaining: left,
                                started,
                            });
                        }
                        return Ok(());
                    }
                    if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                        c.latent = None;
                    }
                    let actor = self.objects[id as usize].name.clone();
                    self.note(TraceKind::LatentResume {
                        actor,
                        native: "Actor.Sleep".into(),
                        started,
                    });
                }
                Some(Latent::Interp { started }) => {
                    // `Actor.FinishInterpolation`: resume once `bInterpolating` has cleared
                    // (advanced by `advance_interpolation` earlier this tick).
                    if self.bool_prop(id, "bInterpolating") {
                        return Ok(());
                    }
                    if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                        c.latent = None;
                    }
                    let actor = self.objects[id as usize].name.clone();
                    self.note(TraceKind::LatentResume {
                        actor,
                        native: "Actor.FinishInterpolation".into(),
                        started,
                    });
                }
                Some(Latent::Landing { remaining, started }) => {
                    // Retail poll 528: losing possession does NOT release this latent.
                    let landed = self
                        .obj_prop(id, "Pawn")
                        .is_some_and(|p| self.byte_prop(p, "Physics") != PHYS_FALLING);
                    if !landed {
                        let remaining = remaining - dt;
                        if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                            c.latent = Some(Latent::Landing { remaining, started });
                        }
                        if remaining < 0.0 {
                            self.send_event(id, "LongFall", Vec::new())?;
                        }
                        return Ok(());
                    }
                    if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                        c.latent = None;
                    }
                    self.note(TraceKind::LatentResume {
                        actor: self.objects[id as usize].name.clone(),
                        native: "Controller.WaitForLanding".into(),
                        started,
                    });
                }
                Some(Latent::Rotation { started }) => {
                    if !self.focus_rotation_complete(id) {
                        return Ok(());
                    }
                    if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                        c.latent = None;
                    }
                    let actor = self.objects[id as usize].name.clone();
                    self.note(TraceKind::LatentResume {
                        actor,
                        native: "Controller.FinishRotation".into(),
                        started,
                    });
                }
                Some(Latent::AnimEnd { channel, started }) => {
                    // `Actor.FinishAnim`: resume once the channel stops animating.
                    if self.anim_channel_active(id, channel) {
                        return Ok(());
                    }
                    if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                        c.latent = None;
                    }
                    let actor = self.objects[id as usize].name.clone();
                    self.note(TraceKind::LatentResume {
                        actor,
                        native: "Actor.FinishAnim".into(),
                        started,
                    });
                }
                Some(Latent::Move {
                    pawn,
                    destination,
                    speed,
                    remaining,
                    started,
                    native,
                }) => {
                    // `Controller.MoveTo`/`MoveToward`: move the pawn each tick; resume when it
                    // arrives or the budget runs out (upstream `MoveTimer`).
                    if self.obj_prop(id, "Pawn") != Some(pawn)
                        || (native == "Controller.MoveToward"
                            && self.obj_prop(id, "MoveTarget").is_none())
                    {
                        if let Some(code) = self.objects[id as usize].state_code.as_mut() {
                            code.latent = None;
                        }
                        continue;
                    }
                    let destination = if native == "Controller.MoveToward" {
                        self.obj_prop(id, "MoveTarget")
                            .and_then(|target| self.vector_prop(target, "Location"))
                            .unwrap_or(destination)
                    } else {
                        self.vector_prop(id, "Destination").unwrap_or(destination)
                    };
                    self.set_property(id, "Destination", 0, Value::Vector(destination));
                    // execPollMoveTo/MoveToward call UpdateTactics after a strict 0.5 s
                    // interval, unless steering toward AdjustLoc or preparing a path move.
                    if self.bool_prop(id, "bAdvancedTactics")
                        && !self.bool_prop(id, "bAdjusting")
                        && !self.bool_prop(id, "bPreparingMove")
                        && self.time - 0.5 > f64::from(self.f32_prop(id, "TacticalOffset"))
                    {
                        self.set_property(id, "TacticalOffset", 0, Value::Float(self.time as f32));
                        let generation = self.objects[id as usize].generation;
                        self.send_event(id, "UpdateTactics", Vec::new())?;
                        if self.objects[id as usize].generation != generation {
                            return Ok(());
                        }
                    }
                    let destination = self.vector_prop(id, "Destination").unwrap_or(destination);
                    let arrived = if self.bool_prop(id, "bPreparingMove") {
                        false
                    } else {
                        self.controller_move_step(id, pawn, destination, dt)?
                    };
                    let remaining = match self.get_property(id, "MoveTimer") {
                        Some(Value::Float(timer)) => *timer,
                        _ => remaining,
                    };
                    let left = remaining - dt;
                    self.set_property(id, "MoveTimer", 0, Value::Float(left));
                    if !arrived && left >= 0.5 * dt {
                        if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                            c.latent = Some(Latent::Move {
                                pawn,
                                destination,
                                speed,
                                remaining: left,
                                started,
                                native,
                            });
                        }
                        return Ok(());
                    }
                    if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                        c.latent = None;
                    }
                    self.set_property(pawn, "bWalking", 0, Value::Bool(false));
                    let actor = self.objects[id as usize].name.clone();
                    self.note(TraceKind::LatentResume {
                        actor,
                        native: native.into(),
                        started,
                    });
                }
                None => {}
            }
            let Some(h) = set.object(code.owner).and_then(ScriptObject::struct_header) else {
                return Err(self.err(VmErrorKind::Unresolved {
                    what: "state code owner".into(),
                }));
            };
            let map = self.stmt_map(code.owner, &h.script);
            let start_gen = self.objects[id as usize].generation;
            let mut frame = Frame {
                pkg: code.owner.package,
                this: id,
                locals: Vec::new(),
                layout: None,
                script: &h.script,
                map,
                iters: Vec::new(),
                state_of: Some(id),
                pending_latent: None,
            };
            self.stack.push(StackEntry {
                function: self.set.path(code.owner),
                object: self.objects[id as usize].name.clone(),
                offset: 0,
            });
            let exit = self.run(&mut frame, code.pc, Some(start_gen));
            self.stack.pop();
            match exit? {
                Exit::Latent(next) => {
                    let (latent, native) = frame.pending_latent.take().expect("latent set");
                    let actor = self.objects[id as usize].name.clone();
                    match &latent {
                        Latent::Sleep { seconds, .. } => self.note(TraceKind::LatentStart {
                            actor,
                            native,
                            seconds: *seconds,
                        }),
                        Latent::AnimEnd { .. } => {
                            self.note(TraceKind::AnimSuspend { actor, native })
                        }
                        Latent::Interp { .. } => self.note(TraceKind::LatentStart {
                            actor,
                            native,
                            seconds: 0.0,
                        }),
                        Latent::Rotation { .. } => self.note(TraceKind::LatentStart {
                            actor,
                            native,
                            seconds: 0.0,
                        }),
                        Latent::Landing { .. } => self.note(TraceKind::LatentStart {
                            actor,
                            native,
                            seconds: 4.0,
                        }),
                        Latent::Move {
                            pawn,
                            destination,
                            speed,
                            ..
                        } => {
                            let loc = self.vector_prop(*pawn, "Location").unwrap_or([0.0; 3]);
                            let dx = destination[0] - loc[0];
                            let dy = destination[1] - loc[1];
                            let seconds = if *speed > 0.0 {
                                (dx * dx + dy * dy).sqrt() / *speed
                            } else {
                                0.0
                            };
                            self.note(TraceKind::LatentStart {
                                actor,
                                native,
                                seconds,
                            })
                        }
                    }
                    if let Some(c) = self.objects[id as usize].state_code.as_mut() {
                        c.pc = next;
                        c.latent = Some(latent);
                    }
                    return Ok(());
                }
                Exit::Stop => {
                    self.objects[id as usize].state_code = None;
                    let actor = self.objects[id as usize].name.clone();
                    self.note(TraceKind::StateStop { actor });
                    return Ok(());
                }
                Exit::Restart => continue,
                Exit::End | Exit::Return(_) => {
                    return Err(self.err(VmErrorKind::StateCodeEnded));
                }
            }
        }
    }

    // ------------------------------------------------------------------ calls

    fn call_values(
        &mut self,
        func: GlobalRef,
        this: ObjectId,
        args: Vec<Value>,
    ) -> VmResult<Value> {
        let set = self.set;
        let Some(ScriptObject::Function(f)) = set.object(func) else {
            return Err(self.err(VmErrorKind::Unresolved {
                what: format!("{} is not a function", set.path(func)),
            }));
        };
        if f.is_native() {
            let mut a = args;
            let omitted = vec![false; a.len()];
            return self
                .invoke_native(func, None, this, &mut a, &omitted, false)
                .and_then(|o| match o {
                    NativeOutcome::Value(v) => Ok(v),
                    _ => Err(self.err(VmErrorKind::Other(
                        "latent/iterator native called outside script".into(),
                    ))),
                });
        }
        let layout = self.func_layout(func);
        let mut locals = Vec::with_capacity(layout.size);
        for s in &layout.slots {
            for _ in 0..s.dim {
                locals.push(s.ty.zero());
            }
        }
        for (i, v) in args.into_iter().enumerate() {
            if let Some(p) = layout.params.get(i) {
                locals[p.slot] = v;
            }
        }
        let (v, _) = self.run_function(func, this, locals, layout)?;
        Ok(v)
    }

    fn run_function(
        &mut self,
        func: GlobalRef,
        this: ObjectId,
        locals: Vec<Value>,
        layout: Rc<FuncLayout>,
    ) -> VmResult<(Value, Vec<Value>)> {
        let set = self.set;
        if self.stack.len() >= self.limits.max_call_depth {
            return Err(self.err(VmErrorKind::CallDepthExceeded {
                limit: self.limits.max_call_depth,
            }));
        }
        self.step()?;
        let h = set
            .object(func)
            .and_then(ScriptObject::struct_header)
            .expect("function");
        let map = self.stmt_map(func, &h.script);
        let mut frame = Frame {
            pkg: func.package,
            this,
            locals,
            layout: Some(layout.clone()),
            script: &h.script,
            map,
            iters: Vec::new(),
            state_of: None,
            pending_latent: None,
        };
        self.stack.push(StackEntry {
            function: set.path(func),
            object: self.objects[this as usize].name.clone(),
            offset: 0,
        });
        let exit = self.run(&mut frame, 0, None);
        let result = match exit {
            Ok(Exit::Return(v)) => Ok(v),
            Ok(Exit::End) => Ok(match &layout.ret {
                Some((slot, _)) => frame.locals[*slot].clone(),
                None => Value::Void,
            }),
            Ok(Exit::Latent(_)) => Err(self.err(VmErrorKind::LatentOutsideState {
                path: frame
                    .pending_latent
                    .as_ref()
                    .map_or_else(String::new, |l| l.1.clone()),
            })),
            Ok(Exit::Stop | Exit::Restart) => Ok(Value::Void),
            Err(e) => Err(e),
        };
        self.stack.pop();
        result.map(|v| (v, frame.locals))
    }

    /// Calls a script or native function from a call token. `target` receives the call.
    fn invoke(
        &mut self,
        frame: &mut Frame<'s>,
        func: GlobalRef,
        call: &Call,
        target: ObjectId,
        index: Option<u16>,
    ) -> VmResult<Value> {
        let set = self.set;
        let Some(ScriptObject::Function(f)) = set.object(func) else {
            return Err(self.err(VmErrorKind::Unresolved {
                what: format!("{} is not a function", set.path(func)),
            }));
        };
        if f.is_native() {
            return match self.native_from_tokens(frame, func, call, target, index)? {
                NativeOutcome::Value(v) => Ok(v),
                NativeOutcome::Iterate(_) => Err(self.err(VmErrorKind::Other(
                    "iterator native outside a foreach".into(),
                ))),
            };
        }
        let layout = self.func_layout(func);
        // Core.dll execVirtualFunction (0x10117490) / execFinalFunction (0x101174d0)
        // dispatch directly to CallFunction (0x1011e650), which does not test actor tick
        // activity, state latency, probes or bDeleteMe. The active-set restriction below is
        // exclusively the partial-execution diagnostic harness's opt-in policy.
        let destroyed_target =
            self.objects[target as usize].deleted || self.objects[target as usize].destroying;
        if self.diagnostic_call_scope
            && !self.objects[target as usize].active
            && !self.objects[target as usize].name.starts_with("Default__")
            && !f.is_static()
            && !destroyed_target
        {
            // item14c: a **suspended** actor (cleared by `suspend_for_error`) is not the same as
            // a placed actor outside the executed scope. Dropping its call silently would hide a
            // real script failure, so every drop on a suspended target is counted and traced.
            let suspended = self.objects[target as usize].suspended;
            let o = &self.objects[target as usize];
            let (tname, class) = (o.name.clone(), set.path(o.class));
            if suspended {
                self.suspended_deferred_calls += 1;
                self.note(TraceKind::Note(format!(
                    "suspended actor {tname} dropped {}.{}: suspended after a script error",
                    class,
                    self.short_path(func)
                )));
            }
            if layout.ret.is_some() {
                return Err(self.err(VmErrorKind::DeferredWithReturnValue {
                    target: tname,
                    function: set.path(func),
                }));
            }
            // Arguments are still evaluated (side effects, Accessed None) as in a real call.
            for a in &call.args {
                self.eval(frame, a)?;
            }
            self.note(TraceKind::Deferred {
                target: tname,
                class,
                function: self.short_path(func),
            });
            return Ok(Value::Void);
        }
        let mut locals = Vec::with_capacity(layout.size);
        for s in &layout.slots {
            for _ in 0..s.dim {
                locals.push(s.ty.zero());
            }
        }
        let mut outs = Vec::new();
        for (i, p) in layout.params.iter().enumerate() {
            let tok = call.args.get(i);
            match tok {
                None => {}
                Some(t) if matches!(t.kind, TokenKind::Nothing) => {}
                Some(t) if p.out => {
                    let place = self.place(frame, t, frame.this)?;
                    if let Some(pl) = &place {
                        locals[p.slot] = self.read(frame, pl)?;
                    }
                    outs.push((p.slot, place));
                }
                Some(t) => locals[p.slot] = self.eval(frame, t)?,
            }
        }
        if target != frame.this {
            let texts = layout
                .params
                .iter()
                .map(|p| self.value_text(&locals[p.slot]))
                .collect();
            self.note(TraceKind::Event {
                target: self.objects[target as usize].name.clone(),
                function: self.short_path(func),
                args: texts,
            });
        }
        let (v, final_locals) = self.run_function(func, target, locals, layout)?;
        for (slot, place) in outs {
            if let Some(pl) = place {
                self.write(frame, &pl, final_locals[slot].clone())?;
            }
        }
        Ok(v)
    }

    fn resolve_native_index(&self, index: u16, nargs: usize) -> VmResult<GlobalRef> {
        let cands = self.set.native_functions(index);
        match cands {
            [] => Err(self.err(VmErrorKind::UnregisteredNative { index })),
            [one] => Ok(*one),
            many => {
                // Duplicate declarations (e.g. 203, 472-476 in this build): pick the one whose
                // parameter count accepts the argument count.
                let fits: Vec<GlobalRef> = many
                    .iter()
                    .copied()
                    .filter(|g| {
                        let set = self.set;
                        let Some(ScriptObject::Function(_)) = set.object(*g) else {
                            return false;
                        };
                        let params = crate::natives::function_params(
                            set,
                            *g,
                            match set.object(*g) {
                                Some(ScriptObject::Function(f)) => f,
                                _ => unreachable!(),
                            },
                        );
                        let total = params.iter().filter(|p| !p.is_return()).count();
                        let required = params
                            .iter()
                            .filter(|p| {
                                !p.is_return() && p.flags & property_flags::OPTIONAL_PARM == 0
                            })
                            .count();
                        nargs >= required && nargs <= total
                    })
                    .collect();
                match fits.as_slice() {
                    [one] => Ok(*one),
                    _ => Err(self.err(VmErrorKind::AmbiguousNative {
                        index,
                        candidates: many.iter().map(|g| self.set.path(*g)).collect(),
                    })),
                }
            }
        }
    }

    fn native_def(&self, func: GlobalRef, index: Option<u16>) -> VmResult<&NativeDef> {
        let key = lower(&self.short_path(func));
        self.registry.get(&key).ok_or_else(|| {
            self.err(VmErrorKind::UnimplementedNative {
                path: self.short_path(func),
                index,
            })
        })
    }

    /// Survey mode: records an unimplemented native, traces it and returns `ret` so the run can
    /// continue past it. Never called outside survey mode.
    fn survey_missing(
        &mut self,
        path: String,
        declared: Option<u16>,
        this: ObjectId,
        args: &[Value],
        ret: Value,
        iterator: bool,
    ) -> VmResult<NativeOutcome> {
        let stack = self.stack.clone();
        match self.missing_natives.get_mut(&path) {
            Some(m) => m.calls += 1,
            None => {
                self.missing_natives.insert(
                    path.clone(),
                    MissingNative {
                        path: path.clone(),
                        index: declared,
                        calls: 1,
                        first_stack: stack,
                    },
                );
            }
        }
        if self.trace_natives {
            self.note(TraceKind::Native {
                path,
                index: declared,
                this: self.objects[this as usize].name.clone(),
                args: args.iter().map(|a| self.value_text(a)).collect(),
                result: "<survey: unimplemented>".into(),
            });
        }
        // An iterator native must yield items, so a foreach over it skips its body rather than
        // failing; a value native yields its return type's zero.
        if iterator {
            return Ok(NativeOutcome::Iterate(Vec::new()));
        }
        Ok(NativeOutcome::Value(ret))
    }

    fn native_from_tokens(
        &mut self,
        frame: &mut Frame<'s>,
        func: GlobalRef,
        call: &Call,
        target: ObjectId,
        index: Option<u16>,
    ) -> VmResult<NativeOutcome> {
        if self.survey && self.registry.get(&lower(&self.short_path(func))).is_none() {
            // Arguments are not evaluated in survey mode: their evaluation can itself fail
            // (e.g. an unresolved class literal passed to DynamicLoadObject) and would stop the
            // survey. Only the missing native is counted.
            let ret = self
                .func_layout(func)
                .ret
                .as_ref()
                .map_or(Value::Void, |(_, ty)| ty.zero());
            let iterator = matches!(
                self.set.object(func),
                Some(ScriptObject::Function(f)) if f.flags & function_flags::ITERATOR != 0
            );
            let outcome =
                self.survey_missing(self.short_path(func), index, target, &[], ret, iterator)?;
            if matches!(outcome, NativeOutcome::Iterate(_)) {
                // The `foreach` driver needs an (empty) iterator frame; the loop body is skipped
                // when a missing iterator native yields no items.
                frame.iters.push(IterState {
                    items: Vec::new(),
                    idx: 0,
                    places: Vec::new(),
                    body: 0,
                    touching: None,
                });
            }
            return Ok(outcome);
        }
        let def = self.native_def(func, index)?;
        let short = def.short_circuit;
        let layout = self.func_layout(func);
        let mut args = Vec::with_capacity(layout.params.len());
        let mut places = Vec::with_capacity(layout.params.len());
        let mut omitted = Vec::with_capacity(layout.params.len());
        for (i, p) in layout.params.iter().enumerate() {
            let tok = call.args.get(i);
            if i == 1
                && let Some(stop) = short
                && args.first() == Some(&Value::Bool(stop))
            {
                // UE2 `&&` / `||`: the right operand (wrapped in Skip) is not evaluated.
                let mut a = vec![Value::Bool(stop)];
                let r = self.finish_native(
                    func,
                    index,
                    target,
                    &mut a,
                    &[false],
                    frame.state_of.is_some(),
                    Some(Value::Bool(stop)),
                )?;
                return Ok(r);
            }
            match tok {
                None => {
                    args.push(p.ty.zero());
                    places.push(None);
                    omitted.push(true);
                }
                Some(t) if matches!(t.kind, TokenKind::Nothing) => {
                    args.push(p.ty.zero());
                    places.push(None);
                    omitted.push(true);
                }
                Some(t) if p.out => {
                    let place = self.place(frame, t, frame.this)?;
                    let v = match &place {
                        Some(pl) => self.read(frame, pl)?,
                        None => p.ty.zero(),
                    };
                    args.push(v);
                    places.push(place);
                    omitted.push(false);
                }
                Some(t) => {
                    args.push(self.eval(frame, t)?);
                    places.push(None);
                    omitted.push(false);
                }
            }
        }
        let in_state = frame.state_of == Some(target);
        let outcome =
            self.finish_native(func, index, target, &mut args, &omitted, in_state, None)?;
        for (i, p) in layout.params.iter().enumerate() {
            if p.out
                && let Some(Some(pl)) = places.get(i)
                && !matches!(outcome, NativeOutcome::Iterate(_))
            {
                self.write(frame, pl, args[i].clone())?;
            }
        }
        if let NativeOutcome::Iterate(items) = outcome {
            // The out parameter receives each item; remember its place for the loop.
            let places = layout
                .params
                .iter()
                .enumerate()
                .filter(|(_, p)| p.out)
                .map(|(i, _)| places.get(i).cloned().flatten())
                .collect();
            frame.iters.push(IterState {
                items: items.clone(),
                idx: 0,
                places,
                body: 0,
                touching: if self
                    .short_path(func)
                    .eq_ignore_ascii_case("Actor.TouchingActors")
                {
                    let base = match args.first() {
                        Some(Value::Object(Some(ObjRef::Static(base)))) => Some(*base),
                        _ => None,
                    };
                    let mut cursor = 0;
                    self.next_touching_actor(target, base, &mut cursor);
                    Some((target, base, cursor))
                } else {
                    None
                },
            });
            return Ok(NativeOutcome::Iterate(items));
        }
        if let Some(latent) = self.take_latent() {
            if !in_state {
                return Err(self.err(VmErrorKind::LatentOutsideState {
                    path: self.short_path(func),
                }));
            }
            frame.pending_latent = Some((latent, self.short_path(func)));
        }
        Ok(outcome)
    }

    fn take_latent(&mut self) -> Option<Latent> {
        self.pending_latent.take()
    }

    fn invoke_native(
        &mut self,
        func: GlobalRef,
        index: Option<u16>,
        this: ObjectId,
        args: &mut [Value],
        omitted: &[bool],
        in_state: bool,
    ) -> VmResult<NativeOutcome> {
        self.finish_native(func, index, this, args, omitted, in_state, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn finish_native(
        &mut self,
        func: GlobalRef,
        index: Option<u16>,
        this: ObjectId,
        args: &mut [Value],
        omitted: &[bool],
        in_state: bool,
        preset: Option<Value>,
    ) -> VmResult<NativeOutcome> {
        self.step()?;
        let path = self.short_path(func);
        let f = if self.survey && self.registry.get(&path.to_ascii_lowercase()).is_none() {
            // `preset` short-circuits `&&`/`||` before this point; otherwise no registry entry
            // means unimplemented. Arguments are not inspected (see `native_from_tokens`).
            let _ = args;
            let ret = self
                .func_layout(func)
                .ret
                .as_ref()
                .map_or(Value::Void, |(_, ty)| ty.zero());
            let iterator = matches!(
                self.set.object(func),
                Some(ScriptObject::Function(f)) if f.flags & function_flags::ITERATOR != 0
            );
            return self.survey_missing(path, index, this, &[], ret, iterator);
        } else {
            self.native_def(func, index)?.f
        };
        let declared = match self.set.object(func) {
            Some(ScriptObject::Function(fun)) if fun.native_index != 0 => Some(fun.native_index),
            _ => index,
        };
        let e = self
            .natives_used
            .entry(path.clone())
            .or_insert((declared, 0));
        e.1 += 1;
        if !self.natives_first_caller.contains_key(&path) {
            self.natives_first_caller
                .insert(path.clone(), self.stack.clone());
        }
        if self.collect_combat_natives
            && self.stack.iter().any(|s| {
                s.function.ends_with("XIIIWeapon.Fire") || s.function.ends_with("NotifyFiring")
            })
        {
            let entry = self
                .combat_natives
                .entry(path.clone())
                .or_insert_with(|| (0, self.stack.clone()));
            entry.0 += 1;
        }
        let before: Vec<String> = if self.trace_natives {
            args.iter().map(|a| self.value_text(a)).collect()
        } else {
            Vec::new()
        };
        let outcome = match preset {
            Some(v) => NativeOutcome::Value(v),
            None => {
                let ctx = NativeCtx {
                    this,
                    in_state_code: in_state,
                    path: path.clone(),
                    omitted: omitted.to_vec(),
                };
                let t0 = self.profile.enabled.then(Instant::now);
                let result = f(self, &ctx, args);
                if let Some(t0) = t0 {
                    let micros = t0.elapsed().as_micros() as u64;
                    *self.profile.micros.entry(path.clone()).or_default() += micros;
                    *self.profile.calls.entry(path.clone()).or_default() += 1;
                    self.profile.natives_micros += micros;
                }
                result?
            }
        };
        if self.trace_natives {
            let result = match &outcome {
                NativeOutcome::Value(Value::Void) => "void".to_owned(),
                NativeOutcome::Value(v) => self.value_text(v),
                NativeOutcome::Iterate(items) => format!("{} items", items.len()),
            };
            self.note(TraceKind::Native {
                path,
                index: declared,
                this: self.objects[this as usize].name.clone(),
                args: before,
                result,
            });
        }
        Ok(outcome)
    }

    // ------------------------------------------------------------------ statements

    fn goto_offset(&self, frame: &Frame<'s>, offset: u32) -> VmResult<usize> {
        frame
            .map
            .get(&offset)
            .copied()
            .ok_or_else(|| self.err(VmErrorKind::BadJumpTarget { offset }))
    }

    fn run(
        &mut self,
        frame: &mut Frame<'s>,
        start: usize,
        state_gen: Option<u64>,
    ) -> VmResult<Exit> {
        let script = frame.script;
        let mut pc = start;
        loop {
            let Some(stmt) = script.statements.get(pc) else {
                return Ok(Exit::End);
            };
            self.step()?;
            if let Some(top) = self.stack.last_mut() {
                top.offset = stmt.offset;
            }
            let flow = self.exec(frame, stmt, pc)?;
            if let Some(start_gen) = state_gen
                && self.objects[frame.this as usize].generation != start_gen
            {
                // GotoState/goto from inside state code: continue with the new code.
                return Ok(Exit::Restart);
            }
            match flow {
                Flow::Next => pc += 1,
                Flow::Goto(i) => pc = i,
                Flow::Return(v) => return Ok(Exit::Return(v)),
                Flow::Stop => return Ok(Exit::Stop),
                Flow::Latent => return Ok(Exit::Latent(pc + 1)),
            }
        }
    }

    fn exec(&mut self, frame: &mut Frame<'s>, t: &Token, pc: usize) -> VmResult<Flow> {
        use TokenKind as K;
        let flow = match &t.kind {
            K::Jump { target } => Flow::Goto(self.goto_offset(frame, u32::from(*target))?),
            K::JumpIfNot { target, cond } => {
                let c = self.eval(frame, cond)?;
                if self.truthy(&c)? {
                    Flow::Next
                } else {
                    Flow::Goto(self.goto_offset(frame, u32::from(*target))?)
                }
            }
            K::Return(e) => {
                let v = match e.kind {
                    K::Nothing => match frame.layout.as_ref().and_then(|l| l.ret.clone()) {
                        Some((slot, _)) => frame.locals[slot].clone(),
                        None => Value::Void,
                    },
                    _ => self.eval(frame, e)?,
                };
                Flow::Return(v)
            }
            K::Stop => Flow::Stop,
            K::Nothing | K::LabelTable { .. } => Flow::Next,
            K::Case { .. } => Flow::Next, // fall-through: UE2 skips the case expression
            K::Switch { expr, .. } => {
                let v = self.eval(frame, expr)?;
                let mut idx = pc + 1;
                loop {
                    let Some(s) = frame.script.statements.get(idx) else {
                        return Err(self.err(VmErrorKind::Other("switch without cases".into())));
                    };
                    match &s.kind {
                        K::Case { value: None, .. } => break Flow::Goto(idx + 1),
                        K::Case {
                            target,
                            value: Some(cv),
                        } => {
                            let c = self.eval(frame, cv)?;
                            if values_equal(&v, &c) {
                                break Flow::Goto(idx + 1);
                            }
                            idx = self.goto_offset(frame, u32::from(*target))?;
                        }
                        _ => break Flow::Goto(idx),
                    }
                }
            }
            K::Assert { line, cond } => {
                let c = self.eval(frame, cond)?;
                if !self.truthy(&c)? {
                    return Err(self.err(VmErrorKind::AssertionFailed { line: *line }));
                }
                Flow::Next
            }
            K::Iterator { expr, end } => {
                // `foreach Obj.AllActors(...)`: the iterator call can sit inside a `Context`
                // token (e.g. `XIIIPlayerInteraction.MyPCPostRender` iterates
                // `MyPC.AllActors`); the native then runs with the context object as `this`.
                let (call_expr, context) = match &expr.kind {
                    K::Context(c) => (c.member.as_ref(), Some(c.object.as_ref())),
                    _ => (expr.as_ref(), None),
                };
                let (func, call, index) = match &call_expr.kind {
                    K::NativeCall { index, call } => (
                        self.resolve_native_index(*index, call.args.len())?,
                        call,
                        Some(*index),
                    ),
                    K::FinalFunction { function, call } => (
                        self.set.resolve(frame.pkg, *function).ok_or_else(|| {
                            self.err(VmErrorKind::Unresolved {
                                what: "iterator function".into(),
                            })
                        })?,
                        call,
                        None,
                    ),
                    _ => {
                        return Err(self.err(VmErrorKind::UnsupportedToken {
                            opcode: call_expr.opcode,
                            name: "Iterator over a non-native call",
                        }));
                    }
                };
                let this = match context {
                    None => frame.this,
                    Some(object) => match self.eval(frame, object)? {
                        Value::Object(Some(ObjRef::Instance(id)))
                            if self.objects.get(id as usize).is_some_and(|o| !o.deleted) =>
                        {
                            id
                        }
                        _ => {
                            // Accessed None: no iteration. Push an empty iterator frame so the
                            // `IteratorPop` at `end` stays balanced.
                            frame.iters.push(IterState {
                                items: Vec::new(),
                                idx: 0,
                                places: Vec::new(),
                                body: pc + 1,
                                touching: None,
                            });
                            return Ok(Flow::Goto(self.goto_offset(frame, u32::from(*end))?));
                        }
                    },
                };
                match self.native_from_tokens(frame, func, call, this, index)? {
                    NativeOutcome::Iterate(items) => {
                        let found = items
                            .iter()
                            .map(|row| {
                                row.first()
                                    .map_or_else(|| "()".into(), |v| self.value_text(v))
                            })
                            .collect();
                        self.note(TraceKind::Iterator {
                            native: self.short_path(func),
                            found,
                        });
                        let st = frame.iters.last_mut().expect("pushed");
                        st.body = pc + 1;
                        if items.is_empty() {
                            if st.touching.is_some() {
                                let places = st.places.clone();
                                for place in places.into_iter().flatten() {
                                    self.write(frame, &place, Value::Object(None))?;
                                }
                            }
                            Flow::Goto(self.goto_offset(frame, u32::from(*end))?)
                        } else {
                            let values = st.items[0].clone();
                            let places = st.places.clone();
                            for (place, value) in places.into_iter().zip(values) {
                                if let Some(place) = place {
                                    self.write(frame, &place, value)?;
                                }
                            }
                            Flow::Next
                        }
                    }
                    NativeOutcome::Value(_) => {
                        return Err(self.err(VmErrorKind::Other(format!(
                            "{} is not an iterator",
                            self.short_path(func)
                        ))));
                    }
                }
            }
            K::IteratorNext => {
                let live = frame.iters.last().and_then(|st| st.touching);
                if let Some((this, base, mut cursor)) = live {
                    let next = self.next_touching_actor(this, base, &mut cursor);
                    let st = frame.iters.last_mut().expect("live iterator exists");
                    st.touching = Some((this, base, cursor));
                    let (places, body) = (st.places.clone(), st.body);
                    let value = Value::Object(next.map(ObjRef::Instance));
                    for place in places.into_iter().flatten() {
                        self.write(frame, &place, value.clone())?;
                    }
                    return Ok(if next.is_some() {
                        Flow::Goto(body)
                    } else {
                        Flow::Next
                    });
                }
                let Some(st) = frame.iters.last_mut() else {
                    return Err(
                        self.err(VmErrorKind::Other("IteratorNext without iterator".into()))
                    );
                };
                st.idx += 1;
                if st.idx < st.items.len() {
                    let (values, places, body) =
                        (st.items[st.idx].clone(), st.places.clone(), st.body);
                    for (place, value) in places.into_iter().zip(values) {
                        if let Some(place) = place {
                            self.write(frame, &place, value)?;
                        }
                    }
                    Flow::Goto(body)
                } else {
                    Flow::Next
                }
            }
            K::IteratorPop => {
                if frame.iters.pop().is_none() {
                    return Err(self.err(VmErrorKind::Other("IteratorPop without iterator".into())));
                }
                Flow::Next
            }
            K::GotoLabel(e) => {
                let v = self.eval(frame, e)?;
                let Value::Name(label) = v else {
                    return Err(self.type_err("name", &v));
                };
                let Some(id) = frame.state_of else {
                    return Err(
                        self.err(VmErrorKind::Other("goto label outside state code".into()))
                    );
                };
                let state = self.objects[id as usize].state;
                let found = state.and_then(|s| self.find_label(s, &label));
                match found {
                    Some((owner, offset)) => {
                        let script = &self.struct_header(owner).expect("state").script;
                        let map = self.stmt_map(owner, script);
                        let pc2 = *map
                            .get(&offset)
                            .ok_or_else(|| self.err(VmErrorKind::BadJumpTarget { offset }))?;
                        let o = &mut self.objects[id as usize];
                        o.state_code = Some(StateCode {
                            owner,
                            pc: pc2,
                            latent: None,
                        });
                        o.generation += 1;
                    }
                    None => {
                        return Err(
                            self.err(VmErrorKind::Other(format!("label '{label}' not found")))
                        );
                    }
                }
                Flow::Next
            }
            _ => {
                self.eval(frame, t)?;
                if frame.pending_latent.is_some() {
                    Flow::Latent
                } else {
                    Flow::Next
                }
            }
        };
        Ok(flow)
    }

    // ------------------------------------------------------------------ expressions

    fn type_err(&self, expected: &'static str, v: &Value) -> VmError {
        self.err(VmErrorKind::TypeMismatch {
            expected,
            found: v.type_name(),
        })
    }

    fn truthy(&self, v: &Value) -> VmResult<bool> {
        match v {
            Value::Bool(b) => Ok(*b),
            other => Err(self.type_err("bool", other)),
        }
    }

    fn eval(&mut self, frame: &mut Frame<'s>, t: &Token) -> VmResult<Value> {
        let this = frame.this;
        self.eval_in(frame, t, this)
    }

    fn accessed_none(&mut self) {
        let (function, offset) = self
            .stack
            .last()
            .map_or((String::new(), 0), |s| (s.function.clone(), s.offset));
        self.note(TraceKind::AccessedNone { function, offset });
    }

    /// Object id behind a context expression the VM would otherwise resolve to `None` because the
    /// actor is destroyed, when the engine would not. Core.dll `UObject::execContext`
    /// (`0x101173a0..0x1011740d`, the null check at `0x101173d3`) only reports Accessed None for a
    /// NULL context; it does **not** test `bDeleteMe`.
    ///
    /// The engine's own bytecode paths are `execContext` (for `P.member`) and
    /// `execVirtualFunction`/`execFinalFunction`, which call `UObject::CallFunction`; that runs the
    /// body directly and never routes through `UObject::ProcessEvent`. So a plain variable read,
    /// a write and a function call through a just-destroyed actor all still work in the engine;
    /// `ProcessEvent` (the engine-delivered `event`/probe path, [Vm::send_event]) is what a
    /// `bDeleteMe` actor skips. `ULevel::DestroyActor` runs `Destroyed` **before** setting
    /// `bDeleteMe` (`0x1038965a`), and `ULevel::CleanupDestroyed` (`0x10387ae0`) only nulls
    /// references once at least 128 (`0x80` at `0x10387b63`) destroyed actors are pending, so a
    /// just-destroyed actor stays readable/writable — this is why `XIIIGameInfo.EndGame` +0x0322
    /// `P = P.nextController` still walks a destroyed AI controller.
    ///
    /// The `member` operand is kept for the callers' clarity; the bypass is per-actor, so the same
    /// rule applies to reads, writes and calls.
    fn bypass_context_none(&self, v: &Value, _member: &Token) -> Option<ObjectId> {
        let Value::Object(Some(ObjRef::Instance(i))) = v else {
            return None;
        };
        let o = self.objects.get(*i as usize)?;
        (o.is_actor && (o.destroying || o.deleted)).then_some(*i)
    }

    /// Target object from an already-evaluated context object expression. `None` = UE2
    /// Accessed-None (a null/deleted/native-only object).
    fn context_value(&mut self, v: Value) -> VmResult<Option<ObjectId>> {
        match v {
            Value::Object(Some(ObjRef::Instance(i))) if self.objects[i as usize].deleted => {
                Ok(None)
            }
            Value::Object(Some(ObjRef::Instance(i))) => Ok(Some(i)),
            Value::Object(Some(ObjRef::Static(g))) => {
                if matches!(self.set.object(g), Some(ScriptObject::Class(_))) {
                    Ok(Some(self.default_object(g)?))
                } else {
                    Err(self.err(VmErrorKind::UnsupportedValue {
                        desc: format!("context on uninstantiated object {}", self.set.path(g)),
                    }))
                }
            }
            Value::Object(None) => Ok(None),
            // A native-only meta-class has no instance in the VM; accessing through it is
            // Accessed-None, same as a null object.
            Value::NativeClass(_) => Ok(None),
            // An object in a non-script package has no VM instance and no property layout:
            // property access on it is an explicit unsupported error, never a silent success.
            Value::Object(Some(ObjRef::External(id))) => {
                Err(self.err(VmErrorKind::UnsupportedValue {
                    desc: format!(
                        "property access on external object {}",
                        self.external_path(&ObjRef::External(id))
                            .unwrap_or_else(|| format!("external#{id}"))
                    ),
                }))
            }
            Value::Unsupported(d) => Err(self.err(VmErrorKind::UnsupportedValue { desc: d })),
            other => Err(self.type_err("object", &other)),
        }
    }

    /// A property of a non-script object through the host [`ExternalObjectData`] provider. The
    /// member must be a plain variable (`Texture.USize`); `None` when there is no provider, the
    /// member is not a variable, or the host does not know the property.
    fn external_member(&self, frame: &Frame<'s>, member: &Token, id: u32) -> Option<Value> {
        use TokenKind as K;
        let name = match &member.kind {
            K::InstanceVariable(r) | K::DefaultVariable(r) | K::LocalVariable(r) => self
                .set
                .resolve(frame.pkg, *r)
                .map(|g| self.object_name(g).to_ascii_lowercase()),
            _ => None,
        }?;
        let path = self.external_object(id)?.path.clone();
        self.external_data.as_ref()?.property(&path, &name)
    }

    fn zero_for(&mut self, frame: &Frame<'s>, t: &Token, target: ObjectId) -> Value {
        use TokenKind as K;
        let g = match &t.kind {
            K::InstanceVariable(r) | K::DefaultVariable(r) | K::LocalVariable(r) => {
                self.set.resolve(frame.pkg, *r)
            }
            K::BoolVariable(_) => return Value::Bool(false),
            // `None.ArrayProp[i]` continues the Accessed-None chain with the element type's zero
            // (UE2 logs Accessed None and reads the element zero). Without this the chain
            // produced `void`, and the next context raised TypeMismatch instead — measured:
            // `self.Tatata.Emitters[0].RespawnDeadParticles = true` with `Tatata == None`
            // (`xidcine.ScriptedImpacts.Burst.Timer2` 0x0000) suspended the whole scripted
            // machine-gun chain that ends the Plage01 intro.
            K::DynArrayElement { array, .. } | K::ArrayElement { array, .. } => {
                return match self.token_property_ty(frame, array) {
                    Some(Ty::Array(inner)) => inner.zero(),
                    _ => Value::Void,
                };
            }
            K::FinalFunction { function, .. } => {
                let g = self.set.resolve(frame.pkg, *function);
                return g.map_or(Value::Void, |g| {
                    self.func_layout(g)
                        .ret
                        .as_ref()
                        .map_or(Value::Void, |(_, ty)| ty.zero())
                });
            }
            K::VirtualFunction { name, .. } => {
                let n = self.set.packages[frame.pkg].name_text(*name).to_owned();
                let g = self.find_function(target, &n, true);
                return g.map_or(Value::Void, |g| {
                    self.func_layout(g)
                        .ret
                        .as_ref()
                        .map_or(Value::Void, |(_, ty)| ty.zero())
                });
            }
            // Accessing a member of `None` yields the member's zero (UE2 Accessed None). When
            // the object expression has a known static class, resolve the member there (its
            // declared type can be found) instead of on `self`, which usually has no such
            // member. This keeps chained contexts returning the leaf type rather than `void`.
            K::Context(c) | K::ClassContext(c) => return self.zero_of_context(frame, c, target),
            K::NativeCall { index, call } => {
                return match self.resolve_native_index(*index, call.args.len()) {
                    Ok(g) => self
                        .func_layout(g)
                        .ret
                        .as_ref()
                        .map_or(Value::Void, |(_, ty)| ty.zero()),
                    Err(_) => Value::Void,
                };
            }
            _ => None,
        };
        match g.and_then(|g| self.set.object(g)) {
            Some(ScriptObject::Property(p)) => self.ty_of(frame.pkg, &p.kind, 0).zero(),
            _ => Value::Void,
        }
    }

    /// Property object referenced directly by a variable token.
    fn member_property(&self, pkg: usize, member: &Token) -> Option<GlobalRef> {
        use TokenKind as K;
        match &member.kind {
            K::InstanceVariable(r) | K::DefaultVariable(r) | K::LocalVariable(r) => {
                self.set.resolve(pkg, *r)
            }
            _ => None,
        }
    }

    /// Declared `Ty` of the property referenced by a variable token or a context member token.
    /// Used to initialise dynamic-array elements grown by `Length = n` / out-of-range writes with
    /// the element type's zero instead of guessing from the assigned scalar.
    fn token_property_ty(&self, frame: &Frame<'s>, t: &Token) -> Option<Ty> {
        use TokenKind as K;
        let g = match &t.kind {
            K::LocalVariable(r) | K::InstanceVariable(r) | K::DefaultVariable(r) => {
                self.set.resolve(frame.pkg, *r)?
            }
            K::Context(c) => self.member_property(frame.pkg, &c.member)?,
            _ => return None,
        };
        match self.set.object(g) {
            Some(ScriptObject::Property(p)) => Some(self.ty_of(g.package, &p.kind, 0)),
            _ => None,
        }
    }

    /// Declared element type of the dynamic-array expression `t`, if it is an array property.
    fn array_elem_ty(&self, frame: &Frame<'s>, t: &Token) -> Option<Ty> {
        match self.token_property_ty(frame, t)? {
            Ty::Array(inner) => Some(*inner),
            _ => None,
        }
    }

    /// Static class of an object-typed token (`self`, a class literal, a variable whose declared
    /// type is an object/class, or a chained context), when it can be determined.
    pub(crate) fn context_object_class(
        &self,
        pkg: usize,
        this: ObjectId,
        t: &Token,
    ) -> Option<GlobalRef> {
        use TokenKind as K;
        match &t.kind {
            K::SelfRef => Some(self.objects[this as usize].class),
            K::ObjectConst(r) => {
                let g = self.set.resolve(pkg, *r)?;
                matches!(self.set.object(g), Some(ScriptObject::Class(_))).then_some(g)
            }
            K::Context(c) => {
                // The context object must itself be an object; then the member's declared type
                // is the result class.
                self.context_object_class(pkg, this, &c.object)?;
                self.property_class(self.member_property(pkg, &c.member)?)
            }
            // `Pawn(Other)`-style class cast: the result's static type is the cast target, so a
            // function call through a `None` cast (a failed dynamic cast) must resolve its return
            // type there. Without this, `Pawn(Other).IsPlayerPawn()` on a non-pawn `Other` fell
            // back to the calling actor's class, found no such function and yielded `void`,
            // suspending `xiii.ZigouillateurTrigger.Touch` / `engine.Ammunition.AddAmmo`.
            K::DynamicCast { class, .. } => {
                let g = self.set.resolve(pkg, *class)?;
                matches!(self.set.object(g), Some(ScriptObject::Class(_))).then_some(g)
            }
            _ => self.property_class(self.member_property(pkg, t)?),
        }
    }

    /// Class an object/class-typed property refers to.
    fn property_class(&self, prop: GlobalRef) -> Option<GlobalRef> {
        let Some(ScriptObject::Property(p)) = self.set.object(prop) else {
            return None;
        };
        let class = match p.kind {
            PropertyKind::Object { class } | PropertyKind::Class { class, .. } => class,
            _ => return None,
        };
        self.set.resolve(prop.package, class)
    }

    /// Zero value of a member accessed through `None`: the declared property type, or the
    /// return type of a called function found in the object expression's static class.
    fn zero_of_context(&mut self, frame: &Frame<'s>, c: &Context, target: ObjectId) -> Value {
        use TokenKind as K;
        if let Some(g) = self.member_property(frame.pkg, &c.member)
            && let Some(ScriptObject::Property(p)) = self.set.object(g)
        {
            return self.ty_of(g.package, &p.kind, 0).zero();
        }
        let name = match &c.member.kind {
            K::VirtualFunction { name, .. } | K::GlobalFunction { name, .. } => {
                self.set.packages[frame.pkg].name_text(*name).to_owned()
            }
            K::FinalFunction { function, .. } => {
                let Some(g) = self.set.resolve(frame.pkg, *function) else {
                    return Value::Void;
                };
                return self
                    .func_layout(g)
                    .ret
                    .as_ref()
                    .map_or(Value::Void, |(_, ty)| ty.zero());
            }
            _ => return self.zero_for(frame, &c.member, target),
        };
        let class = self
            .context_object_class(frame.pkg, frame.this, &c.object)
            .or_else(|| Some(self.objects[target as usize].class));
        let f = class.and_then(|c| {
            self.class_chain(c)
                .into_iter()
                .find_map(|sc| self.find_function_in(sc, &name))
        });
        f.map_or(Value::Void, |g| {
            self.func_layout(g)
                .ret
                .as_ref()
                .map_or(Value::Void, |(_, ty)| ty.zero())
        })
    }

    fn resolve_ref(&self, frame: &Frame<'s>, r: ObjectRef) -> VmResult<GlobalRef> {
        self.set.resolve(frame.pkg, r).ok_or_else(|| {
            self.err(VmErrorKind::Unresolved {
                what: self.set.packages[frame.pkg].ref_path(r),
            })
        })
    }

    fn eval_in(&mut self, frame: &mut Frame<'s>, t: &Token, target: ObjectId) -> VmResult<Value> {
        use TokenKind as K;
        let v = match &t.kind {
            K::LocalVariable(_)
            | K::InstanceVariable(_)
            | K::DefaultVariable(_)
            | K::ArrayElement { .. }
            | K::DynArrayElement { .. }
            | K::BoolVariable(_) => match self.place(frame, t, target)? {
                Some(p) => self.read(frame, &p)?,
                None => self.zero_for(frame, t, target),
            },
            K::StructMember { property, expr } => {
                // Read a struct member off any rvalue, not only a place: XIII's door code reads
                // the `.Z` of a native `Cross(...)` result. `place` on the same token would try
                // to use the native call as an lvalue and fail with `NotAPlace`.
                let g = self.resolve_ref(frame, *property)?;
                let name = lower(self.object_name(g));
                let base = self.eval_in(frame, expr, target)?;
                let v = member_get(&base, &name).ok_or_else(|| {
                    self.err(VmErrorKind::Other(format!("no struct member {name}")))
                })?;
                if let Value::Unsupported(d) = &v {
                    return Err(self.err(VmErrorKind::UnsupportedValue { desc: d.clone() }));
                }
                v
            }
            K::Context(c) => {
                let v = self.eval_in(frame, &c.object, target)?;
                if let Value::Object(Some(ObjRef::External(id))) = &v {
                    // A property of an object in a non-script package. The host provider answers
                    // known native properties (for example `Texture.USize`/`VSize`, read by
                    // `HudState.DrawStt`); anything else stays the explicit error below.
                    match self.external_member(frame, &c.member, *id) {
                        Some(value) => value,
                        None => {
                            return Err(self.err(VmErrorKind::UnsupportedValue {
                                desc: format!(
                                    "property access on external object {}",
                                    self.external_path(&ObjRef::External(*id))
                                        .unwrap_or_else(|| format!("external#{id}"))
                                ),
                            }));
                        }
                    }
                } else if let Some(obj) = self.bypass_context_none(&v, &c.member) {
                    self.eval_in(frame, &c.member, obj)?
                } else {
                    match self.context_value(v)? {
                        Some(obj) => self.eval_in(frame, &c.member, obj)?,
                        None => {
                            self.accessed_none();
                            self.zero_of_context(frame, c, target)
                        }
                    }
                }
            }
            K::ClassContext(c) => {
                let v = self.eval_in(frame, &c.object, target)?;
                let class = match v {
                    Value::Object(Some(ObjRef::Static(g))) => Some(g),
                    Value::Object(Some(ObjRef::Instance(i))) => {
                        Some(self.objects[i as usize].class)
                    }
                    Value::Object(None) | Value::NativeClass(_) => None,
                    other => return Err(self.type_err("class", &other)),
                };
                match class {
                    Some(g) => {
                        let d = self.default_object(g)?;
                        self.eval_in(frame, &c.member, d)?
                    }
                    None => {
                        self.accessed_none();
                        self.zero_for(frame, &c.member, target)
                    }
                }
            }
            K::New {
                outer,
                name,
                flags,
                class,
            } => {
                // UE2 `FFrame::execNew`: `New (Outer, Name, Flags) Class` — operands in that
                // order. Actors may not be constructed with `new`.
                let outer_v = self.eval_in(frame, outer, target)?;
                let name_v = self.eval_in(frame, name, target)?;
                let flags_v = self.eval_in(frame, flags, target)?;
                let class_v = self.eval_in(frame, class, target)?;
                self.new_object(outer_v, name_v, flags_v, class_v)?
            }
            K::Let { lhs, rhs } | K::LetBool { lhs, rhs } => {
                let place = self.place(frame, lhs, target)?;
                let v = self.eval(frame, rhs)?;
                match place {
                    Some(p) => self.write(frame, &p, v)?,
                    None => self.accessed_none(),
                }
                Value::Void
            }
            K::VirtualFunction { name, call } => {
                let n = self.set.packages[frame.pkg].name_text(*name).to_owned();
                let f = self.find_function(target, &n, true).ok_or_else(|| {
                    self.err(VmErrorKind::NoSuchFunction {
                        object: self.objects[target as usize].name.clone(),
                        name: n.clone(),
                    })
                })?;
                self.invoke(frame, f, call, target, None)?
            }
            K::GlobalFunction { name, call } => {
                let n = self.set.packages[frame.pkg].name_text(*name).to_owned();
                let f = self.find_function(target, &n, false).ok_or_else(|| {
                    self.err(VmErrorKind::NoSuchFunction {
                        object: self.objects[target as usize].name.clone(),
                        name: n.clone(),
                    })
                })?;
                self.invoke(frame, f, call, target, None)?
            }
            K::FinalFunction { function, call } => {
                let f = self.resolve_ref(frame, *function)?;
                self.invoke(frame, f, call, target, None)?
            }
            K::NativeCall { index, call } => {
                let f = self.resolve_native_index(*index, call.args.len())?;
                self.invoke(frame, f, call, target, Some(*index))?
            }
            K::IntConst(v) => Value::Int(*v),
            K::IntConstByte(v) => Value::Int(i32::from(*v)),
            K::IntZero => Value::Int(0),
            K::IntOne => Value::Int(1),
            K::FloatConst(v) => Value::Float(*v),
            K::ByteConst(v) => Value::Byte(*v),
            K::True => Value::Bool(true),
            K::False => Value::Bool(false),
            K::NoObject => Value::Object(None),
            K::SelfRef => Value::Object(Some(ObjRef::Instance(frame.this))),
            K::StringConst(b) => Value::Str(b.iter().map(|&c| char::from(c)).collect()),
            K::UnicodeStringConst(u) => Value::Str(String::from_utf16_lossy(u)),
            K::NameConst(n) => Value::Name(self.set.packages[frame.pkg].name_text(*n).to_owned()),
            K::VectorConst(v) => Value::Vector(*v),
            K::RotationConst(r) => Value::Rotator(*r),
            K::ObjectConst(r) => {
                let pkg = frame.pkg;
                self.resolve_value_ref(pkg, *r)
            }
            K::Nothing => Value::Void,
            K::Skip { expr, .. } => self.eval_in(frame, expr, target)?,
            K::EatString(e) => {
                self.eval(frame, e)?;
                Value::Void
            }
            K::Conditional { cond, a, b, .. } => {
                let c = self.eval(frame, cond)?;
                if self.truthy(&c)? {
                    self.eval(frame, a)?
                } else {
                    self.eval(frame, b)?
                }
            }
            K::DynamicCast { class, expr } => {
                // A class reference may name a native-only meta-class with no export
                // (`Engine.SkeletalMesh`, `Engine.Mesh`, ...): fall back to its leaf name
                // instead of failing. Static assets keep their reference when the decoded
                // class leaf matches the cast target.
                let target_leaf = match self.set.resolve(frame.pkg, *class) {
                    Some(g) => self.object_name(g).to_owned(),
                    None => self.set.packages[frame.pkg].ref_name(*class).to_owned(),
                };
                let v = self.eval(frame, expr)?;
                match v {
                    Value::Object(Some(ObjRef::Instance(i))) => {
                        if self.objects[i as usize]
                            .layout
                            .chain_names
                            .iter()
                            .any(|n| n.eq_ignore_ascii_case(&target_leaf))
                        {
                            Value::Object(Some(ObjRef::Instance(i)))
                        } else {
                            Value::Object(None)
                        }
                    }
                    Value::Object(Some(ObjRef::Static(g))) => {
                        let ok = self.class_path_of(g).is_some_and(|p| {
                            p.rsplit('.')
                                .next()
                                .is_some_and(|l| l.eq_ignore_ascii_case(&target_leaf))
                        });
                        if ok {
                            Value::Object(Some(ObjRef::Static(g)))
                        } else {
                            Value::Object(None)
                        }
                    }
                    // An external object keeps its reference when the recorded class (from the
                    // referencing import table, or verified against the external package) is the
                    // cast target or derives from it under the native-class table from item3h.
                    Value::Object(Some(ObjRef::External(id))) => {
                        if self.external_is_a(id, &target_leaf) {
                            Value::Object(Some(ObjRef::External(id)))
                        } else {
                            Value::Object(None)
                        }
                    }
                    Value::Object(None) | Value::NativeClass(_) => Value::Object(None),
                    other => return Err(self.type_err("object", &other)),
                }
            }
            K::MetaCast { class, expr } => {
                let class = self.resolve_ref(frame, *class)?;
                let v = self.eval(frame, expr)?;
                match v {
                    Value::Object(Some(ObjRef::Static(g))) if self.is_child_of(g, class) => {
                        Value::Object(Some(ObjRef::Static(g)))
                    }
                    Value::Object(_) | Value::NativeClass(_) => Value::Object(None),
                    other => return Err(self.type_err("class", &other)),
                }
            }
            K::PrimitiveCast { cast, expr } => {
                let v = self.eval(frame, expr)?;
                self.primitive_cast(*cast, v)?
            }
            K::DynArrayLength(e) => match self.eval_in(frame, e, target)? {
                Value::Array(a) => Value::Int(a.len() as i32),
                other => return Err(self.type_err("array", &other)),
            },
            K::DynArrayInsert {
                array,
                index,
                count,
            }
            | K::DynArrayRemove {
                array,
                index,
                count,
            } => {
                let place = self.place(frame, array, target)?;
                let idx = self.int(frame, index)?;
                let n = self.int(frame, count)?;
                if let Some(pl) = place {
                    let mut arr = match self.read(frame, &pl)? {
                        Value::Array(a) => a,
                        other => return Err(self.type_err("array", &other)),
                    };
                    let (i, n) = (idx.max(0) as usize, n.max(0) as usize);
                    if t.opcode == 0x40 {
                        if i > arr.len() {
                            return Err(self.err(VmErrorKind::ArrayIndex {
                                index: i as i64,
                                len: arr.len(),
                            }));
                        }
                        let zero = self.array_inner_zero(frame, array);
                        for _ in 0..n {
                            arr.insert(i, zero.clone());
                        }
                    } else {
                        if i + n > arr.len() {
                            return Err(self.err(VmErrorKind::ArrayIndex {
                                index: (i + n) as i64,
                                len: arr.len(),
                            }));
                        }
                        arr.drain(i..i + n);
                    }
                    self.write(frame, &pl, Value::Array(arr))?;
                }
                Value::Void
            }
            K::StructCmpEq { a, b, .. } | K::StructCmpNe { a, b, .. } => {
                let x = self.eval(frame, a)?;
                let y = self.eval(frame, b)?;
                Value::Bool(values_equal(&x, &y) == (t.opcode == 0x32))
            }
            // `0x44` in an assignment right-hand side: a fresh delegate bound to the current
            // context object (`self.__OnDraw__Delegate = delegateprop InternalOnDraw`).
            K::DelegateProperty(name) => {
                let function = self.set.packages[frame.pkg].name_text(*name).to_owned();
                Value::Delegate(Some(Delegate {
                    object: Some(ObjRef::Instance(target)),
                    function,
                }))
            }
            // `0x3F`: an explicitly empty delegate (`delegate(D) = none`).
            K::EmptyDelegate => Value::Delegate(None),
            // `0x45`: delegate assignment, evaluated like a normal `Let` but storing a delegate
            // value (the destination slot's declared type is `delegate`).
            K::LetDelegate { lhs, rhs } => {
                let place = self.place(frame, lhs, target)?;
                let v = self.eval(frame, rhs)?;
                match place {
                    Some(p) => self.write(frame, &p, v)?,
                    None => self.accessed_none(),
                }
                Value::Void
            }
            // `0x43`: `Object.delegate <DelegateProperty>:<Function>(args)`. Read the delegate
            // property from the context object; a bound delegate calls its own object/function,
            // an unbound one calls `<Function>` on the context (UE2 semantics).
            K::DelegateFunction {
                property,
                name,
                call,
            } => {
                let prop_name = self
                    .set
                    .resolve(frame.pkg, *property)
                    .map(|g| self.object_name(g).to_owned())
                    .unwrap_or_else(|| self.set.packages[frame.pkg].ref_name(*property).to_owned());
                let bound = match self.get_property(target, &prop_name) {
                    Some(Value::Delegate(Some(d))) => Some(d.clone()),
                    Some(Value::Delegate(None)) | None => None,
                    Some(other) => {
                        return Err(self.type_err("delegate", other));
                    }
                };
                let declared = self.set.packages[frame.pkg].name_text(*name).to_owned();
                match bound {
                    Some(d) => {
                        let obj = match d.object {
                            Some(ObjRef::Instance(i)) => i,
                            Some(ObjRef::Static(g)) => self.default_object(g)?,
                            Some(ObjRef::External(_)) => {
                                return Err(self.err(VmErrorKind::UnsupportedValue {
                                    desc: format!(
                                        "delegate {prop_name} on a non-script external object"
                                    ),
                                }));
                            }
                            None => target,
                        };
                        let f = self.find_function(obj, &d.function, true).ok_or_else(|| {
                            self.err(VmErrorKind::NoSuchFunction {
                                object: self.objects[obj as usize].name.clone(),
                                name: d.function.clone(),
                            })
                        })?;
                        self.invoke(frame, f, call, obj, None)?
                    }
                    None => {
                        let f = self.find_function(target, &declared, true).ok_or_else(|| {
                            self.err(VmErrorKind::NoSuchFunction {
                                object: self.objects[target as usize].name.clone(),
                                name: declared.clone(),
                            })
                        })?;
                        self.invoke(frame, f, call, target, None)?
                    }
                }
            }
            // `0x3B..=0x3E`: delegate equality/inequality; `0x3B`/`0x3D` are the `==` forms.
            K::DelegateCompare { a, b, .. } => {
                let x = self.eval(frame, a)?;
                let y = self.eval(frame, b)?;
                Value::Bool(values_equal(&x, &y) == matches!(t.opcode, 0x3B | 0x3D))
            }
            _ => {
                return Err(self.err(VmErrorKind::UnsupportedToken {
                    opcode: t.opcode,
                    name: opcode_name(t.opcode),
                }));
            }
        };
        if let Value::Unsupported(d) = &v {
            return Err(self.err(VmErrorKind::UnsupportedValue { desc: d.clone() }));
        }
        Ok(v)
    }

    fn array_inner_zero(&mut self, frame: &Frame<'s>, array: &Token) -> Value {
        match self.zero_for(frame, array, frame.this) {
            Value::Array(_) => {}
            _ => return Value::Int(0),
        }
        let g = match &array.kind {
            TokenKind::InstanceVariable(r) | TokenKind::LocalVariable(r) => {
                self.set.resolve(frame.pkg, *r)
            }
            _ => None,
        };
        if let Some(ScriptObject::Property(p)) = g.and_then(|g| self.set.object(g))
            && let Ty::Array(inner) = self.ty_of(frame.pkg, &p.kind, 0)
        {
            return inner.zero();
        }
        Value::Int(0)
    }

    fn int(&mut self, frame: &mut Frame<'s>, t: &Token) -> VmResult<i32> {
        match self.eval(frame, t)? {
            Value::Int(i) => Ok(i),
            Value::Byte(b) => Ok(i32::from(b)),
            other => Err(self.type_err("int", &other)),
        }
    }

    pub(crate) fn primitive_cast(&self, cast: u8, v: Value) -> VmResult<Value> {
        // UE2 conversion codes (ECastToken), confirmed on corpus uses (0x3F int->float on
        // ints, 0x44 float->int, 0x53/0x56/0x57 to string on int/object/name operands).
        let bad = |s: &Self, v: &Value| s.type_err("castable value", v);
        Ok(match (cast, &v) {
            // UE2 `RotationToVector` (ECastToken 0x39): unit vector from a rotator.
            (0x39, Value::Rotator(r)) => {
                let to_rad = |u: i32| (u as f32) * std::f32::consts::TAU / 65536.0;
                let (pitch, yaw) = (to_rad(r[0]), to_rad(r[1]));
                Value::Vector([
                    pitch.cos() * yaw.cos(),
                    pitch.cos() * yaw.sin(),
                    pitch.sin(),
                ])
            }
            // UE2 `VectorToRotator` (ECastToken 0x50): `FVector::Rotation()` (yaw from XY, pitch
            // from Z, roll 0; rotator units, rounded).
            (0x50, Value::Vector(v)) => {
                let units = 65536.0 / std::f32::consts::TAU;
                let yaw = v[1].atan2(v[0]);
                let pitch = v[2].atan2((v[0] * v[0] + v[1] * v[1]).sqrt());
                Value::Rotator([
                    (pitch * units).round() as i32,
                    (yaw * units).round() as i32,
                    0,
                ])
            }
            (0x3A, Value::Byte(b)) => Value::Int(i32::from(*b)),
            (0x3B, Value::Byte(b)) => Value::Bool(*b != 0),
            (0x3C, Value::Byte(b)) => Value::Float(f32::from(*b)),
            (0x3D, Value::Int(i)) => Value::Byte(*i as u8),
            (0x3E, Value::Int(i)) => Value::Bool(*i != 0),
            (0x3F, Value::Int(i)) => Value::Float(*i as f32),
            (0x40, Value::Bool(b)) => Value::Byte(u8::from(*b)),
            (0x41, Value::Bool(b)) => Value::Int(i32::from(*b)),
            (0x42, Value::Bool(b)) => Value::Float(if *b { 1.0 } else { 0.0 }),
            (0x43, Value::Float(f)) => Value::Byte(*f as u8),
            (0x44, Value::Float(f)) => Value::Int(*f as i32),
            (0x45, Value::Float(f)) => Value::Bool(*f != 0.0),
            (0x47, Value::Object(o)) => Value::Bool(o.is_some()),
            (0x47, Value::NativeClass(_)) => Value::Bool(true),
            (0x48, Value::Name(n)) => Value::Bool(!n.eq_ignore_ascii_case("None")),
            (0x4A, Value::Str(s)) => Value::Int(s.trim().parse().unwrap_or(0)),
            (0x4B, Value::Str(s)) => Value::Bool(
                s.eq_ignore_ascii_case("true") || s.trim().parse::<i32>().is_ok_and(|i| i != 0),
            ),
            (0x4C, Value::Str(s)) => Value::Float(s.trim().parse().unwrap_or(0.0)),
            (0x52, Value::Byte(b)) => Value::Str(b.to_string()),
            (0x53, Value::Int(i)) => Value::Str(i.to_string()),
            (0x54, Value::Bool(b)) => Value::Str(if *b { "True" } else { "False" }.into()),
            (0x55, Value::Float(f)) => Value::Str(format!("{f:.2}")),
            (0x56, Value::Object(None)) => Value::Str("None".into()),
            (0x56, Value::Object(Some(r))) => Value::Str(self.obj_label(r)),
            (0x56, Value::NativeClass(n)) => Value::Str(n.clone()),
            (0x57, Value::Name(n)) => Value::Str(n.clone()),
            // UE2 `VectorToString` (0x58): comma-separated X,Y,Z with the same 2-decimal float
            // format as `FloatToString`; decoded at `xidmaps.Spads01.FirstFrame` 0x0045
            // (`Log("SpotOffset" @ string(vector))`). BeyondUnreal "Typecast".
            (0x58, Value::Vector(v)) => Value::Str(format!("{:.2},{:.2},{:.2}", v[0], v[1], v[2])),
            // UE2 `RotatorToString` (0x59): Pitch,Yaw,Roll each reduced to the 0..65535 range.
            (0x59, Value::Rotator(r)) => Value::Str(format!(
                "{},{},{}",
                r[0] & 0xffff,
                r[1] & 0xffff,
                r[2] & 0xffff
            )),
            // UE2 `StringToName` (ECastToken 0x5A): intern the string as an FName. The cine
            // interpreter casts the `GetFirstWord` action tag to `name` for StartDialogue.
            (0x5A, Value::Str(s)) => Value::Name(s.clone()),
            (c, _) if !(0x39..=0x5B).contains(&c) => return Err(bad(self, &v)),
            _ => {
                return Err(self.err(VmErrorKind::Other(format!(
                    "primitive cast 0x{cast:02X} on {} not implemented",
                    v.type_name()
                ))));
            }
        })
    }

    // ------------------------------------------------------------------ places

    fn var_slot(
        &mut self,
        frame: &Frame<'s>,
        r: ObjectRef,
        target: ObjectId,
        default: bool,
    ) -> VmResult<Place> {
        let g = self.resolve_ref(frame, r)?;
        if let Some(l) = &frame.layout
            && let Some(i) = l.by_prop.get(&g)
        {
            return Ok(Place::Local(l.slots[*i].base));
        }
        let obj = if default {
            let class = self.objects[target as usize].class;
            self.default_object(class)?
        } else {
            target
        };
        let layout = self.objects[obj as usize].layout.clone();
        match layout.by_prop.get(&g) {
            Some(i) => Ok(Place::Slot(obj, layout.slots[*i].base)),
            None => Err(self.err(VmErrorKind::Unresolved {
                what: format!(
                    "property {} not in class {} of {}",
                    self.set.path(g),
                    self.set.path(layout.class),
                    self.objects[obj as usize].name
                ),
            })),
        }
    }

    fn slot_dim(&self, frame: &Frame<'s>, place: &Place) -> usize {
        match place {
            Place::Local(base) => frame
                .layout
                .as_ref()
                .and_then(|l| l.slots.iter().find(|s| s.base == *base))
                .map_or(1, |s| s.dim),
            Place::Slot(obj, base) => self.objects[*obj as usize]
                .layout
                .slots
                .iter()
                .find(|s| s.base == *base)
                .map_or(1, |s| s.dim),
            _ => 1,
        }
    }

    fn place(
        &mut self,
        frame: &mut Frame<'s>,
        t: &Token,
        target: ObjectId,
    ) -> VmResult<Option<Place>> {
        use TokenKind as K;
        Ok(Some(match &t.kind {
            K::LocalVariable(r) => self.var_slot(frame, *r, target, false)?,
            K::InstanceVariable(r) => self.var_slot(frame, *r, target, false)?,
            K::DefaultVariable(r) => self.var_slot(frame, *r, target, true)?,
            K::BoolVariable(e) => return self.place(frame, e, target),
            K::Context(c) => {
                // Writes through a destroyed-but-uncleaned actor still land: `execContext` has no
                // `bDeleteMe` test (see `bypass_context_none`), so `P.NextController = x` updates
                // the stale memory the engine would update.
                let v = self.eval_in(frame, &c.object, target)?;
                if let Some(obj) = self.bypass_context_none(&v, &c.member) {
                    return self.place(frame, &c.member, obj);
                }
                match self.context_value(v)? {
                    Some(obj) => return self.place(frame, &c.member, obj),
                    None => return Ok(None),
                }
            }
            K::ArrayElement { index, array } => {
                let i = self.int(frame, index)?;
                let Some(base) = self.place(frame, array, target)? else {
                    return Ok(None);
                };
                let dim = self.slot_dim(frame, &base);
                if i < 0 || i as usize >= dim {
                    return Err(self.err(VmErrorKind::ArrayIndex {
                        index: i64::from(i),
                        len: dim,
                    }));
                }
                match base {
                    Place::Local(b) => Place::Local(b + i as usize),
                    Place::Slot(o, b) => Place::Slot(o, b + i as usize),
                    _ => {
                        return Err(self.err(VmErrorKind::NotAPlace { opcode: t.opcode }));
                    }
                }
            }
            K::DynArrayElement { index, array } => {
                let i = self.int(frame, index)?;
                let elem_ty = self.array_elem_ty(frame, array);
                let Some(base) = self.place(frame, array, target)? else {
                    return Ok(None);
                };
                if i < 0 {
                    return Err(self.err(VmErrorKind::ArrayIndex {
                        index: i64::from(i),
                        len: 0,
                    }));
                }
                Place::Elem(Box::new(base), i as usize, elem_ty)
            }
            K::StructMember { property, expr } => {
                let g = self.resolve_ref(frame, *property)?;
                let name = lower(self.object_name(g));
                let Some(base) = self.place(frame, expr, target)? else {
                    return Ok(None);
                };
                Place::Member(Box::new(base), name)
            }
            // `Array.Length = n` is the UE2 dynamic-array resize idiom; it is the only
            // assignable use of `DynArrayLength`.
            K::DynArrayLength(e) => {
                let elem_ty = self.array_elem_ty(frame, e);
                let Some(base) = self.place(frame, e, target)? else {
                    return Ok(None);
                };
                Place::ArrayLen(Box::new(base), elem_ty)
            }
            _ => return Err(self.err(VmErrorKind::NotAPlace { opcode: t.opcode })),
        }))
    }

    fn read(&self, frame: &Frame<'s>, p: &Place) -> VmResult<Value> {
        let v = match p {
            Place::Local(i) => frame.locals.get(*i).cloned(),
            Place::Slot(o, i) => self.objects[*o as usize].props.get(*i).cloned(),
            Place::Elem(base, i, _) => match self.read(frame, base)? {
                Value::Array(a) => match a.get(*i) {
                    Some(v) => Some(v.clone()),
                    None => {
                        return Err(self.err(VmErrorKind::ArrayIndex {
                            index: *i as i64,
                            len: a.len(),
                        }));
                    }
                },
                other => return Err(self.type_err("array", &other)),
            },
            Place::Member(base, m) => Some(
                member_get(&self.read(frame, base)?, m)
                    .ok_or_else(|| self.err(VmErrorKind::Other(format!("no struct member {m}"))))?,
            ),
            Place::ArrayLen(base, _) => match self.read(frame, base)? {
                Value::Array(a) => Some(Value::Int(a.len() as i32)),
                other => return Err(self.type_err("array", &other)),
            },
        };
        let v = v.ok_or_else(|| self.err(VmErrorKind::Other("bad slot".into())))?;
        if let Value::Unsupported(d) = &v {
            return Err(self.err(VmErrorKind::UnsupportedValue { desc: d.clone() }));
        }
        Ok(v)
    }

    fn write(&mut self, frame: &mut Frame<'s>, p: &Place, v: Value) -> VmResult<()> {
        match p {
            Place::Local(i) => {
                let slot = frame
                    .locals
                    .get_mut(*i)
                    .ok_or_else(|| self.err(VmErrorKind::Other("bad local".into())))?;
                *slot = v;
            }
            Place::Slot(o, i) => {
                let len = self.objects[*o as usize].props.len();
                if *i >= len {
                    return Err(self.err(VmErrorKind::Other("bad slot".into())));
                }
                if std::env::var_os("XIII_WATCH_PLAYER_HEALTH").is_some()
                    && self.objects[*o as usize].is_actor
                    && self.is_a(*o, "XIIIPlayerPawn")
                    && self.objects[*o as usize]
                        .layout
                        .slots
                        .iter()
                        .find(|s| s.base == *i)
                        .is_some_and(|s| s.name.eq_ignore_ascii_case("health"))
                {
                    let writer = self.stack.last().map_or("<host>", |s| s.function.as_str());
                    let offset = self.stack.last().map_or(0, |s| s.offset);
                    eprintln!(
                        "[vm-health-write] t={:.6} pawn={} writer={} offset=0x{:04X} value={}",
                        self.time,
                        self.objects[*o as usize].name,
                        writer,
                        offset,
                        self.value_text(&v)
                    );
                }
                if std::env::var_os("XIII_WATCH_PLAYER_HEALTH").is_some()
                    && self.objects[*o as usize].is_actor
                    && self.objects[*o as usize]
                        .layout
                        .slots
                        .iter()
                        .find(|s| s.base == *i)
                        .is_some_and(|s| s.name.eq_ignore_ascii_case("startspotevent"))
                {
                    let writer = self.stack.last().map_or("<host>", |s| s.function.as_str());
                    let offset = self.stack.last().map_or(0, |s| s.offset);
                    eprintln!(
                        "[vm-sse-write] t={:.6} obj={} writer={} offset=0x{:04X} value={}",
                        self.time,
                        self.objects[*o as usize].name,
                        writer,
                        offset,
                        self.value_text(&v)
                    );
                }
                self.objects[*o as usize].props[*i] = v;
            }
            Place::Elem(base, i, elem_ty) => {
                let mut arr = match self.read(frame, base)? {
                    Value::Array(a) => a,
                    other => return Err(self.type_err("array", &other)),
                };
                if *i >= arr.len() {
                    // UE2 grows a dynamic array on assignment past its end and initialises the new
                    // elements to the element type's default (a zero struct, not the assigned
                    // scalar). The declared element type is known when the array expression is a
                    // property; otherwise fall back to the assigned value's zero.
                    let zero = elem_ty.as_ref().map_or_else(
                        || match &v {
                            Value::Int(_) => Value::Int(0),
                            Value::Float(_) => Value::Float(0.0),
                            Value::Object(_) => Value::Object(None),
                            Value::Name(_) => Value::Name("None".into()),
                            other => other.clone(),
                        },
                        Ty::zero,
                    );
                    arr.resize(*i + 1, zero);
                }
                arr[*i] = v;
                self.write(frame, base, Value::Array(arr))?;
            }
            Place::Member(base, m) => {
                let mut s = self.read(frame, base)?;
                if !member_set(&mut s, m, v) {
                    return Err(self.err(VmErrorKind::Other(format!("no struct member {m}"))));
                }
                self.write(frame, base, s)?;
            }
            Place::ArrayLen(base, elem_ty) => {
                let n = match v {
                    Value::Int(i) => i.max(0) as usize,
                    other => return Err(self.type_err("int", &other)),
                };
                let mut arr = match self.read(frame, base)? {
                    Value::Array(a) => a,
                    other => return Err(self.type_err("array", &other)),
                };
                // UE2 `Array.Length = n` initialises the grown elements to the element type's
                // default. When the property's declared element type is known, use it (a zero
                // struct carries all its members); otherwise infer from an existing element.
                let zero = elem_ty.as_ref().map_or_else(
                    || match arr.last() {
                        Some(Value::Float(_)) => Value::Float(0.0),
                        Some(Value::Object(_)) => Value::Object(None),
                        Some(Value::Name(_)) => Value::Name("None".into()),
                        Some(Value::Bool(_)) => Value::Bool(false),
                        Some(Value::Byte(_)) => Value::Byte(0),
                        _ => Value::Int(0),
                    },
                    Ty::zero,
                );
                arr.resize(n, zero);
                self.write(frame, base, Value::Array(arr))?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ helpers for natives

    pub(crate) fn disable_probe(&mut self, id: ObjectId, probe: &str, disable: bool) {
        // UE2 `Disable('All')` sets the actor-wide `bProbesDisabled` bit; a named probe is
        // recorded in the actor's disabled-probe set (UE2 `FObject::DisableProbe`; not separately
        // disassembled here). `Enable('All')` clears the bit. The VM keeps both: the whole-actor
        // flag suppresses every event, so `Destroyed` is dropped too (the engine's probe check).
        if probe.eq_ignore_ascii_case("all") {
            self.objects[id as usize].probes_disabled = disable;
            self.set_property(id, "bProbesDisabled", 0, Value::Bool(disable));
        }
        let o = &mut self.objects[id as usize];
        if disable {
            o.disabled.insert(lower(probe));
        } else {
            o.disabled.remove(&lower(probe));
        }
    }

    pub(crate) fn set_timer(&mut self, id: ObjectId, rate: f32, repeat: bool) {
        self.set_timer_named(id, rate, repeat, "Timer");
    }

    /// `SetTimer2`/`Controller.SetTimer3`: a timer that dispatches `Timer2`/`Timer3` instead of
    /// `Timer`. UE2 keeps the three timers independent; the VM keeps one active slot per actor
    /// (the AI states use them one at a time; a second `SetTimer*` replaces the first, as
    /// `AActor::execSetTimer` does for its own slot).
    pub(crate) fn set_timer_named(
        &mut self,
        id: ObjectId,
        rate: f32,
        repeat: bool,
        name: &'static str,
    ) {
        let slot = TIMER_EVENTS
            .iter()
            .position(|e| e.eq_ignore_ascii_case(name))
            .unwrap_or(0);
        self.objects[id as usize].timers[slot] = (rate > 0.0).then_some(Timer {
            rate,
            remaining: rate,
            repeat,
        });
    }

    /// A display name that does not collide with any existing object (`Name`, `Name1`, ...).
    fn unique_name(&self, base: &str) -> String {
        if !self
            .objects
            .iter()
            .any(|o| o.name.eq_ignore_ascii_case(base))
        {
            return base.to_owned();
        }
        for n in 1.. {
            let candidate = format!("{base}{n}");
            if !self
                .objects
                .iter()
                .any(|o| o.name.eq_ignore_ascii_case(&candidate))
            {
                return candidate;
            }
        }
        base.to_owned()
    }

    /// True when a class itself carries the UE2 `abstract` class flag. `abstract` is not
    /// inherited (a concrete subclass of an abstract class clears the bit), so only the class's
    /// own flags are tested.
    pub fn class_is_abstract(&self, class: GlobalRef) -> bool {
        match self.set.object(class) {
            Some(ScriptObject::Class(cl)) => cl.class_flags & CLASS_FLAG_ABSTRACT != 0,
            _ => false,
        }
    }

    /// UE2 `execNew`: construct a non-actor object of `class` with its class defaults. A `None`
    /// class returns `None`; an `Actor` subclass is an explicit error (upstream forbids `new`
    /// on actors). `Outer`/`Name` are honoured; the object gets no lifecycle events.
    fn new_object(
        &mut self,
        outer: Value,
        name: Value,
        _flags: Value,
        class: Value,
    ) -> VmResult<Value> {
        let class = match class {
            Value::Object(Some(ObjRef::Static(g)))
                if matches!(self.set.object(g), Some(ScriptObject::Class(_))) =>
            {
                g
            }
            Value::Object(None) => return Ok(Value::Object(None)),
            Value::NativeClass(n) => {
                return Err(self.err(VmErrorKind::UnsupportedValue {
                    desc: format!("new class'{n}' has no export to instantiate"),
                }));
            }
            other => return Err(self.type_err("class", &other)),
        };
        if self
            .class_layout(class)?
            .chain_names
            .iter()
            .any(|n| n == "actor")
        {
            return Err(self.err(VmErrorKind::NewOnActor {
                class: self.set.path(class),
            }));
        }
        let class_name = self.object_name(class).to_owned();
        let obj_name = match name {
            Value::Name(n) if !n.eq_ignore_ascii_case("None") => self.unique_name(&n),
            _ => self.unique_name(&class_name),
        };
        let id = self.spawn(class, &obj_name)?;
        if let Value::Object(Some(ObjRef::Instance(o))) = outer
            && self.objects.get(o as usize).is_some_and(|x| !x.deleted)
        {
            self.set_property(id, "Outer", 0, Value::Object(Some(ObjRef::Instance(o))));
        }
        // A `new`-ed object is in the executed scope (no lifecycle events for non-actors).
        self.objects[id as usize].active = true;
        self.note(TraceKind::NewObject {
            object: obj_name,
            class: self.set.path(class),
        });
        Ok(Value::Object(Some(ObjRef::Instance(id))))
    }

    /// `Actor.Spawn` semantics: instantiate `class`, set `Owner`/`Tag`/`Location`/`Rotation`,
    /// mark the actor in scope and run [`RUNTIME_SPAWN_LIFECYCLE`]. `None` class returns `None`;
    /// an abstract class is refused with a trace (`SpawnRefused`) and returns `None`.
    #[allow(clippy::too_many_arguments)]
    pub fn spawn_actor(
        &mut self,
        spawner: ObjectId,
        class: Option<GlobalRef>,
        owner: Option<ObjectId>,
        tag: Option<&str>,
        location: Option<[f32; 3]>,
        rotation: Option<[i32; 3]>,
    ) -> VmResult<Option<ObjectId>> {
        let Some(id) = self.spawn_actor_inner(spawner, class, owner, tag, location, rotation)?
        else {
            return Ok(None);
        };
        self.run_lifecycle(id, RUNTIME_SPAWN_LIFECYCLE)?;
        Ok(Some(id))
    }

    /// Creates an actor like [`Vm::spawn_actor`] but without running the runtime spawn
    /// lifecycle, for level start where the grouped [`Vm::begin_play`] covers it.
    pub fn spawn_level_actor(
        &mut self,
        spawner: ObjectId,
        class: GlobalRef,
        owner: Option<ObjectId>,
        tag: Option<&str>,
    ) -> VmResult<Option<ObjectId>> {
        self.spawn_actor_inner(spawner, Some(class), owner, tag, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    fn spawn_actor_inner(
        &mut self,
        spawner: ObjectId,
        class: Option<GlobalRef>,
        owner: Option<ObjectId>,
        tag: Option<&str>,
        location: Option<[f32; 3]>,
        rotation: Option<[i32; 3]>,
    ) -> VmResult<Option<ObjectId>> {
        let Some(class) = class else {
            return Ok(None);
        };
        if self.class_is_abstract(class) {
            self.note(TraceKind::SpawnRefused {
                reason: format!("{} is abstract", self.set.path(class)),
            });
            return Ok(None);
        }
        let class_name = self.object_name(class).to_owned();
        let name = self.unique_name(&class_name);
        let id = self.spawn(class, &name)?;
        // UE2 `SpawnActor` defaults Location/Rotation to the *spawning* actor's (the script
        // `this`), not the new actor's Owner.
        let (loc, rot) = (
            location.or_else(|| self.vector_prop(spawner, "Location")),
            rotation.or_else(|| self.rotator_prop(spawner, "Rotation")),
        );
        self.set_property(id, "Owner", 0, Value::Object(owner.map(ObjRef::Instance)));
        // Engine.dll execSpawn 0x103e5785 passes this->Instigator (+0x88), and
        // ULevel::SpawnActor 0x10388d91..0x10388d96 stores it before lifecycle callbacks.
        // Owner is independent: ammo spawned by a pawn must retain that pawn as Instigator
        // so its Transfer can unlink from the corpse before GiveTo changes ownership.
        let instigator = self
            .get_property(spawner, "Instigator")
            .cloned()
            .unwrap_or(Value::Object(None));
        self.set_property(id, "Instigator", 0, instigator);
        self.set_property(
            id,
            "Tag",
            0,
            Value::Name(tag.map_or_else(|| class_name.clone(), str::to_owned)),
        );
        if let Some(v) = loc {
            self.set_property(id, "Location", 0, Value::Vector(v));
        }
        if let Some(r) = rot {
            self.set_property(id, "Rotation", 0, Value::Rotator(r));
        }
        // UE2 `ULevel::SpawnActor` sets `Actor->Level = Level` for every spawned actor. The VM
        // represents `Level` as the map's `LevelInfo`; without this a spawned controller reads
        // `Level.Game` as None and `Actor.PreBeginPlay` destroys it (`CheckRelevance`).
        if let Some(level) = self.obj_prop(spawner, "Level") {
            self.set_property(id, "Level", 0, Value::Object(Some(ObjRef::Instance(level))));
        }
        // item61: the engine's per-actor `InitExecution` runs before any lifecycle event.
        self.apply_native_init_execution(id);
        self.objects[id as usize].active = true;
        self.note(TraceKind::Spawned {
            actor: name,
            class: self.set.path(class),
        });
        Ok(Some(id))
    }

    /// item61: the anti-piracy values the retail engine's `InitExecution` overrides store on
    /// fresh actors before any script lifecycle runs, applied at the same points the VM
    /// instantiates actors (runtime [`Vm::spawn_actor`]/[`Vm::spawn_level_actor`] and
    /// [`Vm::load_level`]):
    ///
    /// - Engine.dll `?InitExecution@AGameInfo@@UAEXXZ` (0x103e0c80): after the
    ///   `AActor::InitExecution` call at 0x103e0cad it stores `0xC3A3228F` (= -326.27f) at
    ///   `this+0x2c0` (0x103e0cb6) = `GameInfo.DummyStuff1` (float) and `0x337` (= 823) at
    ///   `this+0x2c4` (0x103e0cc0) = `GameInfo.DummyStuff2` (int). `xidpawn.IAController.Init`
    ///   state `TurnIntoSoldierInit` (code 0x0010) gives every soldier
    ///   `BaseS.Skill = 5; Pawn.Health *= 5` unless `Level.Game.DummyStuff1` carries -326.27, so
    ///   without this write all campaign soldiers are 5x-health skill-5 soldiers.
    /// - XIDPawn.dll `?InitExecution@AGenAlerte@@UAEXXZ` (VA 0x119015c0, RVA 0x15c0): after its
    ///   `AActor::InitExecution` IAT call it stores `0x7d2` (= 2002) at `this+0x21c` (0x119015c9)
    ///   = `GenAlerte.dummy` (int). `GenAlerte.PoteBeugle` (4 sites) applies
    ///   `BaseS.Skill = 5; Pawn.Health *= 10` to every alerted soldier unless `dummy` is inside
    ///   (1940, 2003) — the retail value 2002 suppresses it.
    ///
    /// Deliberately untraced: the retail write produces no script-visible effect by itself and
    /// the Plage00 trace baseline must stay byte-identical.
    fn apply_native_init_execution(&mut self, id: ObjectId) {
        let Some(chain) = self
            .objects
            .get(id as usize)
            .map(|o| o.layout.chain_names.clone())
        else {
            return;
        };
        if chain.iter().any(|n| n == "gameinfo") {
            self.set_property(id, "DummyStuff1", 0, Value::Float(-326.27));
            self.set_property(id, "DummyStuff2", 0, Value::Int(823));
        }
        if chain.iter().any(|n| n == "genalerte") {
            self.set_property(id, "dummy", 0, Value::Int(2002));
        }
    }

    /// Vector property value, or `None` when the property is absent/another type.
    pub fn vector_prop(&self, id: ObjectId, name: &str) -> Option<[f32; 3]> {
        match self.get_property(id, name)? {
            Value::Vector(v) => Some(*v),
            _ => None,
        }
    }

    fn rotator_prop(&self, id: ObjectId, name: &str) -> Option<[i32; 3]> {
        match self.get_property(id, name)? {
            Value::Rotator(v) => Some(*v),
            _ => None,
        }
    }

    /// Vector property value at an array element (`None` when absent/another type).
    fn vector_prop_elem(&self, id: ObjectId, name: &str, elem: usize) -> Option<[f32; 3]> {
        match self.get_property_elem(id, name, elem)? {
            Value::Vector(v) => Some(*v),
            _ => None,
        }
    }

    /// Rotator property value at an array element (`None` when absent/another type).
    fn rotator_prop_elem(&self, id: ObjectId, name: &str, elem: usize) -> Option<[i32; 3]> {
        match self.get_property_elem(id, name, elem)? {
            Value::Rotator(v) => Some(*v),
            _ => None,
        }
    }

    /// Byte property value (`0` when absent; `Int` is accepted for a byte-typed slot).
    pub(crate) fn byte_prop(&self, id: ObjectId, name: &str) -> u8 {
        match self.get_property(id, name) {
            Some(Value::Byte(b)) => *b,
            Some(Value::Int(i)) => *i as u8,
            _ => 0,
        }
    }

    fn run_lifecycle(&mut self, id: ObjectId, events: &[&str]) -> VmResult<()> {
        for ev in events {
            if self.objects.get(id as usize).is_none_or(|o| o.deleted) {
                break;
            }
            self.send_event(id, ev, Vec::new())?;
        }
        Ok(())
    }

    /// Every live `Mover` actor with its current pose and interpolation state (for the host's
    /// dynamic collision). Ordered by object id, so the result is deterministic.
    pub fn mover_states(&self) -> Vec<MoverState> {
        let mut out = Vec::new();
        for (i, object) in self.objects.iter().enumerate() {
            if let Some(mut s) = self.mover_state_of(i as ObjectId) {
                s.name = object.name.clone();
                out.push(s);
            }
        }
        out
    }

    /// A live `Mover` actor's state by display name (case-insensitive).
    pub fn mover_state(&self, name: &str) -> Option<MoverState> {
        let id = self.find_object(name)?;
        let mut s = self.mover_state_of(id)?;
        s.name = self.objects[id as usize].name.clone();
        Some(s)
    }

    /// True when `id` is a live instance deriving from `Mover`.
    pub fn is_mover(&self, id: ObjectId) -> bool {
        self.is_live_actor(id) && self.is_a(id, "mover")
    }

    /// Pose/state of a mover actor without allocating its name.
    fn mover_state_of(&self, id: ObjectId) -> Option<MoverState> {
        let o = self.objects.get(id as usize)?;
        if !o.is_actor || o.deleted || o.name.starts_with("Default__") || !o.layout.is_mover_class {
            return None;
        }
        Some(MoverState {
            name: String::new(),
            location: self.location_prop(id).unwrap_or([0.0; 3]),
            rotation: self.rotation_prop(id).unwrap_or([0; 3]),
            base_pos: self.vector_prop(id, "BasePos").unwrap_or([0.0; 3]),
            base_rot: self.rotator_prop(id, "BaseRot").unwrap_or([0; 3]),
            key_num: self.byte_prop(id, "KeyNum"),
            phys_alpha: self.f32_prop(id, "PhysAlpha"),
            phys_rate: self.f32_prop(id, "PhysRate"),
            interpolating: self.bool_prop(id, "bInterpolating"),
            blocks_players: self.bool_prop(id, "bCollideActors")
                && self.bool_prop(id, "bBlockPlayers"),
        })
    }

    /// item40e: propagates every mover's collision state to the world-physics provider. A mover
    /// that was destroyed (e.g. `BreakableMover.Breaked` -> `Destroy`) or whose `bCollideActors`
    /// was cleared (`SetCollision`) leaves UE2's collision hash (`AActor::SetCollision` @
    /// 0x103527D0 removes it; `FCollisionHash::AddActor` @ 0x10349980 requires
    /// `bCollideActors`), so the VM's own `Move`/`Trace` must stop hitting its geometry; setting
    /// the flag again restores it. Only changes are sent. Call once per tick after the VM ran.
    pub fn sync_mover_collision(&mut self) {
        if self.physics.is_none() {
            return;
        }
        let mut changes: Vec<(String, bool)> = Vec::new();
        for (i, o) in self.objects.iter().enumerate() {
            if !o.is_actor || !o.layout.is_mover_class || o.name.starts_with("Default__") {
                continue;
            }
            let id = i as ObjectId;
            let enabled = !o.deleted && self.bool_prop(id, "bCollideActors");
            if self.mover_collision_sent.get(&id) != Some(&enabled) {
                self.mover_collision_sent.insert(id, enabled);
                changes.push((o.name.clone(), enabled));
            }
        }
        if let Some(p) = self.physics.as_mut() {
            for (name, enabled) in changes {
                p.set_mover_collision(&name, enabled);
            }
        }
    }

    /// First live `LevelInfo` instance, if the map has one (it is normally not in the executed
    /// scope, so the harness does not supply it).
    pub fn find_level_info(&self) -> Option<ObjectId> {
        (0..self.objects.len() as ObjectId).find(|&id| {
            let o = &self.objects[id as usize];
            !o.deleted && o.is_actor && o.layout.chain_names.iter().any(|n| n == "levelinfo")
        })
    }

    /// Level start with a GameInfo: spawns `game_class` (no lifecycle yet), points
    /// `LevelInfo.Game` at it, sets `Level` on every actor, then runs [`Vm::begin_play`] over
    /// `map_ids` and the GameInfo together. Returns the GameInfo id.
    pub fn begin_play_with_game_info(
        &mut self,
        map_ids: &[ObjectId],
        game_class: GlobalRef,
    ) -> VmResult<ObjectId> {
        let level_info = self.find_level_info();
        let level = level_info.unwrap_or(0);
        // UE2 ULevel::SpawnActor sets the new actor's Level; the engine sets it for every actor
        // at load, and Actor.PreBeginPlay reads `Level.Game.BaseMutator`.
        if level_info.is_some() {
            for id in 0..self.objects.len() as ObjectId {
                if self.objects[id as usize].is_actor && !self.objects[id as usize].deleted {
                    self.set_property(id, "Level", 0, Value::Object(Some(ObjRef::Instance(level))));
                }
            }
        }
        let Some(info) = self.spawn_level_actor(level, game_class, None, None)? else {
            return Err(self.err(VmErrorKind::Other(format!(
                "GameInfo class {} was refused (None or abstract)",
                self.set.path(game_class)
            ))));
        };
        if let Some(li) = level_info {
            self.set_property(li, "Game", 0, Value::Object(Some(ObjRef::Instance(info))));
            self.set_property(info, "Level", 0, Value::Object(Some(ObjRef::Instance(li))));
        }
        self.note(TraceKind::GameInfo {
            actor: self.objects[info as usize].name.clone(),
            class: self.set.path(game_class),
        });
        // UE2 UGameEngine::InitGame spawns the GameInfo, then calls GameInfo.InitGame (which
        // builds GameInfo.BaseMutator and the other helpers) before any actor begins play.
        if let Some(f) = self.find_function(info, "InitGame", false) {
            // UE2 `UGameEngine::LoadMap` passes the map URL's options string to
            // `GameInfo.InitGame(Options, Error)`; the runtime owns it (`Vm::set_local_url`).
            let options = self.url_options.clone();
            self.call_values(
                f,
                info,
                vec![Value::Str(options), Value::Str(String::new())],
            )?;
        }
        // UE2 `UGameEngine::LoadMap` marks the level as "startup" before actors begin play:
        // `LevelInfo.bStartup` is the gate `Pawn.PostBeginPlay` reads before spawning its
        // `ControllerClass` and calling `Controller.Possess`. Without it, level-placed pawns
        // never get a controller (the no-agent case reported in item3d). `bBegunPlay` is set
        // once the level-start lifecycle has run (upstream sets it at the end of `LoadMap`;
        // the touch engine reads it).
        if let Some(li) = level_info {
            self.set_property(li, "bStartup", 0, Value::Bool(true));
        }
        let mut ids = map_ids.to_vec();
        ids.push(info);
        self.begin_play(&ids)?;
        // Upstream clears `bStartup` again once the level-start events have run (hypothesis
        // for XIII); leaving it set would make every later runtime spawn look like a
        // level-start spawn (e.g. auto-possession in `Pawn.PostBeginPlay`).
        if let Some(li) = level_info {
            self.set_property(li, "bStartup", 0, Value::Bool(false));
            self.set_property(li, "bBegunPlay", 0, Value::Bool(true));
        }
        Ok(info)
    }

    /// Runs the level-start lifecycle ([`LEVEL_START_LIFECYCLE`]) for every actor in `ids`.
    pub fn begin_play(&mut self, ids: &[ObjectId]) -> VmResult<()> {
        for ev in LEVEL_START_LIFECYCLE {
            for &id in ids {
                if self.objects.get(id as usize).is_none_or(|o| o.deleted) {
                    continue;
                }
                self.send_event(id, ev, Vec::new())?;
            }
        }
        Ok(())
    }

    /// `Actor.Destroy` in the engine's `ULevel::DestroyActor` order (`Engine.dll`
    /// `?DestroyActor@ULevel` RVA 0x890e0): clear the actor's state and latent action, run the
    /// `Destroyed` event, then set `bDeleteMe` and remove it from the level lists.
    ///
    /// `deleted` (the "acts as `None`" flag) is set **after** `Destroyed`, exactly as the engine
    /// sets `bDeleteMe` after the event; `destroying` is the in-progress guard that makes a nested
    /// `Destroy` from inside `Destroyed` a no-op (the engine's re-entry returns 1). A plain
    /// variable read/write and a call through the actor still work during and after the event
    /// (execContext/CallFunction have no delete gate; see `bypass_context_none`), while
    /// `ProcessEvent`-delivered events stop once `deleted` is set.
    pub fn destroy(&mut self, id: ObjectId) -> VmResult<bool> {
        if self
            .objects
            .get(id as usize)
            .is_none_or(|o| o.deleted || o.destroying)
        {
            return Ok(true);
        }
        // `ULevel::DestroyActor` clears the state (and its latent action) before `Destroyed`.
        self.objects[id as usize].destroying = true;
        self.objects[id as usize].state = None;
        self.objects[id as usize].state_code = None;
        self.objects[id as usize].generation += 1;
        // A `bProbesDisabled` actor receives no `ProcessEvent`, so `Destroyed` never runs.
        let event = if !self.objects[id as usize].probes_disabled {
            self.find_function(id, "Destroyed", true)
        } else {
            None
        };
        let result = match event {
            Some(f) => self.call_values(f, id, Vec::new()).map(|_| ()),
            None => Ok(()),
        };
        // `bDeleteMe` is now set; the actor stops executing.
        self.objects[id as usize].destroying = false;
        self.objects[id as usize].deleted = true;
        result?;
        self.objects[id as usize].active = false;
        self.objects[id as usize].timers = [None, None, None];
        let actor = self.objects[id as usize].name.clone();
        self.note(TraceKind::Destroyed {
            actor,
            result: true,
        });
        Ok(true)
    }

    /// First live (not deleted) object with a name (case-insensitive).
    pub fn find_live_object(&self, name: &str) -> Option<ObjectId> {
        self.objects
            .iter()
            .position(|o| !o.deleted && o.name.eq_ignore_ascii_case(name))
            .map(|i| i as ObjectId)
    }

    // ------------------------------------------------------------------ world collision / touch

    /// A live map/actor instance (skips deleted objects and class-default objects).
    fn is_live_actor(&self, id: ObjectId) -> bool {
        self.objects
            .get(id as usize)
            .is_some_and(|o| o.is_actor && !o.deleted && !o.name.starts_with("Default__"))
    }

    /// `float` property value, or `0.0` when the property is absent/another type.
    pub(crate) fn f32_prop(&self, id: ObjectId, name: &str) -> f32 {
        match self.get_property(id, name) {
            Some(Value::Float(v)) => *v,
            _ => 0.0,
        }
    }

    /// `bool` property value (false when absent/another type).
    pub(crate) fn bool_prop(&self, id: ObjectId, name: &str) -> bool {
        matches!(self.get_property(id, name), Some(Value::Bool(true)))
    }

    /// `Location` read through the class layout's precomputed slot (no name lookup). Public for
    /// the host's per-tick render sync and pawn placement.
    pub fn location_prop(&self, id: ObjectId) -> Option<[f32; 3]> {
        let o = self.objects.get(id as usize)?;
        match o.props.get(o.layout.location_slot?) {
            Some(Value::Vector(v)) => Some(*v),
            _ => None,
        }
    }

    /// `Rotation` read through the class layout's precomputed slot (no name lookup).
    pub fn rotation_prop(&self, id: ObjectId) -> Option<[i32; 3]> {
        let o = self.objects.get(id as usize)?;
        match o.props.get(o.layout.rotation_slot?) {
            Some(Value::Rotator(v)) => Some(*v),
            _ => None,
        }
    }

    /// `bCollideActors` read through the precomputed slot (the touch-refresh pre-filter).
    pub(crate) fn collides(&self, id: ObjectId) -> bool {
        let Some(o) = self.objects.get(id as usize) else {
            return false;
        };
        o.layout
            .collide_slot
            .is_some_and(|i| matches!(o.props.get(i), Some(Value::Bool(true))))
    }

    /// `bInterpolating` read through the precomputed slot (the mover-tick pre-filter).
    pub(crate) fn interpolating(&self, id: ObjectId) -> bool {
        let Some(o) = self.objects.get(id as usize) else {
            return false;
        };
        o.layout
            .interp_slot
            .is_some_and(|i| matches!(o.props.get(i), Some(Value::Bool(true))))
    }

    /// Object property value as a live instance id.
    pub(crate) fn obj_prop(&self, id: ObjectId, name: &str) -> Option<ObjectId> {
        match self.get_property(id, name) {
            Some(Value::Object(Some(ObjRef::Instance(i))))
                if self.objects.get(*i as usize).is_some_and(|o| !o.deleted) =>
            {
                Some(*i)
            }
            _ => None,
        }
    }

    /// `(center, radius, half-height)` of an actor's collision cylinder.
    pub(crate) fn actor_cylinder(&self, id: ObjectId) -> ([f32; 3], f32, f32) {
        (
            self.vector_prop(id, "Location").unwrap_or([0.0; 3]),
            self.f32_prop(id, "CollisionRadius"),
            self.f32_prop(id, "CollisionHeight"),
        )
    }

    /// Half-size extent box of an actor: `(CollisionRadius, CollisionRadius, CollisionHeight)`.
    pub(crate) fn actor_extent(&self, id: ObjectId) -> [f32; 3] {
        let (_, r, h) = self.actor_cylinder(id);
        [r, r, h]
    }

    /// `A`'s own or transitive `Base` chain contains `B` (UE1 `AActor::IsBasedOn`).
    pub(crate) fn based_on(&self, id: ObjectId, other: ObjectId) -> bool {
        let mut cur = self.obj_prop(id, "Base");
        let mut guard = 0;
        while let Some(c) = cur {
            if c == other {
                return true;
            }
            guard += 1;
            if guard > 4096 {
                break;
            }
            cur = self.obj_prop(c, "Base");
        }
        false
    }

    /// XIII has no `bIsPlayerPawn`/`bIsProjectile` script fields (measured); classify by class
    /// name so the upstream player/projectile `bBlockPlayers` pairing can be approximated.
    pub(crate) fn is_player_or_projectile(&self, id: ObjectId) -> bool {
        self.objects.get(id as usize).is_some_and(|o| {
            o.layout.chain_names.iter().any(|n| {
                let n = n.to_ascii_lowercase();
                n.contains("projectile") || n.ends_with("playerpawn")
            })
        })
    }

    /// UE2 vertical-cylinder overlap, exactly as decoded from `engine.u Actor.TouchingActor`
    /// (final simulated): `|dz| <= h1 + h2` and `sqrt(dx^2+dy^2) <= r1 + r2` (both inclusive).
    pub(crate) fn actors_overlap(&self, a: ObjectId, b: ObjectId) -> bool {
        if a == b {
            return false;
        }
        let (la, ra, ha) = self.actor_cylinder(a);
        let (lb, rb, hb) = self.actor_cylinder(b);
        if (la[2] - lb[2]).abs() > ha + hb {
            return false;
        }
        let (dx, dy) = (la[0] - lb[0], la[1] - lb[1]);
        dx * dx + dy * dy <= (ra + rb) * (ra + rb)
    }

    /// Whether `other` blocks the movement of `mover` (UE1/UE2 pairwise blocking rule).
    pub(crate) fn blocks_pair(&self, mover: ObjectId, other: ObjectId) -> bool {
        let e = self.actor_extent(mover);
        let nonzero = e[0] + e[1] + e[2] > 0.0;
        let gate = if nonzero {
            self.bool_prop(other, "bBlockNonZeroExtentTraces")
        } else {
            self.bool_prop(other, "bBlockZeroExtentTraces")
        };
        if !gate {
            return false;
        }
        if self.based_on(mover, other) || self.based_on(other, mover) {
            return false;
        }
        let (mp, op) = (
            self.is_player_or_projectile(mover),
            self.is_player_or_projectile(other),
        );
        let (a, b) = if mp || op {
            ("bBlockPlayers", "bBlockPlayers")
        } else {
            ("bBlockActors", "bBlockActors")
        };
        self.bool_prop(mover, a) && self.bool_prop(other, b)
    }

    /// Earliest fraction of `start -> end` at which the mover's cylinder first touches a
    /// blocking actor's cylinder (inclusive contact), if any.
    fn sweep_blocking_actor(
        &self,
        mover: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
    ) -> Option<(f32, ObjectId)> {
        let (_, rm, hm) = self.actor_cylinder(mover);
        let d = sub3(end, start);
        let mut best: Option<(f32, ObjectId)> = None;
        for b in 0..self.objects.len() as ObjectId {
            if b == mover || !self.is_live_actor(b) || !self.blocks_pair(mover, b) {
                continue;
            }
            let (lb, rb, hb) = self.actor_cylinder(b);
            if let Some(t) = segment_cylinder_contact(start, d, lb, rm + rb, hm + hb)
                && best.is_none_or(|(bt, _)| t < bt)
            {
                best = Some((t, b));
            }
        }
        best
    }

    /// Actors in `id`'s `Touching` array (live ones only).
    pub(crate) fn touching_list(&self, id: ObjectId) -> Vec<ObjectId> {
        match self.get_property(id, "Touching") {
            Some(Value::Array(items)) => items
                .iter()
                .filter_map(|v| match v {
                    Value::Object(Some(ObjRef::Instance(i)))
                        if self.objects.get(*i as usize).is_some_and(|o| !o.deleted) =>
                    {
                        Some(*i)
                    }
                    _ => None,
                })
                .collect(),
            _ => Vec::new(),
        }
    }

    /// Native TouchingActors selection. Deliberately does not call touching_list:
    /// the retail iterator tests only null and IsA, including retained deleted actors.
    pub(crate) fn next_touching_actor(
        &self,
        id: ObjectId,
        base: Option<GlobalRef>,
        cursor: &mut usize,
    ) -> Option<ObjectId> {
        let Some(Value::Array(items)) = self.get_property(id, "Touching") else {
            return None;
        };
        while let Some(value) = items.get(*cursor) {
            *cursor += 1;
            if let Value::Object(Some(ObjRef::Instance(actor))) = value
                && let Some(object) = self.objects.get(*actor as usize)
                && base.map_or(object.is_actor, |class| {
                    object.layout.chain.contains(&class)
                })
            {
                return Some(*actor);
            }
        }
        None
    }

    fn set_touching_list(&mut self, id: ObjectId, list: Vec<ObjectId>) {
        let items = list
            .into_iter()
            .map(|i| Value::Object(Some(ObjRef::Instance(i))))
            .collect();
        self.set_property(id, "Touching", 0, Value::Array(items));
    }

    fn touching_add(&mut self, id: ObjectId, other: ObjectId) {
        let mut list = self.touching_list(id);
        if !list.contains(&other) {
            list.push(other);
            self.set_touching_list(id, list);
        }
    }

    fn touching_remove(&mut self, id: ObjectId, other: ObjectId) {
        let mut list = self.touching_list(id);
        if list.contains(&other) {
            list.retain(|x| *x != other);
            self.set_touching_list(id, list);
        }
    }

    /// Delivers a `Touch`/`UnTouch` event to `target` with `other` as the argument. Skips
    /// deleted targets; records the usual `EVENT`/`NO HANDLER`/`PROBE` trace otherwise.
    fn deliver_touch_event(
        &mut self,
        target: ObjectId,
        event: &str,
        other: ObjectId,
    ) -> VmResult<()> {
        if self.objects.get(target as usize).is_none_or(|o| o.deleted) {
            return Ok(());
        }
        let arg = Value::Object(Some(ObjRef::Instance(other)));
        self.send_event(target, event, vec![arg])?;
        Ok(())
    }

    /// Sets up a touch pair: links first (so a recursive call sees the binding), then sends
    /// `Touch` to `a` and to `b` (upstream `AActor::Touch`: both sides are notified).
    fn begin_touch(&mut self, a: ObjectId, b: ObjectId) -> VmResult<()> {
        self.touching_add(a, b);
        self.touching_add(b, a);
        self.deliver_touch_event(a, "Touch", b)?;
        self.deliver_touch_event(b, "Touch", a)?;
        Ok(())
    }

    /// Ends a touch pair: clears both links, then sends `UnTouch` to both sides.
    fn end_touch(&mut self, a: ObjectId, b: ObjectId) -> VmResult<()> {
        self.touching_remove(a, b);
        self.touching_remove(b, a);
        self.deliver_touch_event(a, "UnTouch", b)?;
        self.deliver_touch_event(b, "UnTouch", a)?;
        Ok(())
    }

    /// Public wrapper for the engine's touch refresh after an **external** (host) move of `id`.
    /// Uses `SetLocation` semantics (`skip_blocking = false`): a blocking actor does not suppress
    /// the touch. The Bevy host calls this after writing the player pawn's `Location` from the
    /// movement simulation, so walking into a trigger volume delivers the `Touch` the VM would
    /// otherwise only deliver from a script `Move`/`SetLocation`.
    pub fn refresh_touching_of(&mut self, id: ObjectId) -> VmResult<()> {
        self.refresh_touching(id, false)
    }

    /// Recomputes the touching relations of `id` after it moved or its collision changed:
    /// begins overlap with actors it now touches, ends overlap it no longer has. `skip_blocking`
    /// mirrors upstream `TryMove` (an actor cannot touch what blocks it); `SetLocation` does
    /// not skip blocking actors.
    pub(crate) fn refresh_touching(&mut self, id: ObjectId, skip_blocking: bool) -> VmResult<()> {
        if !self.is_live_actor(id) {
            return Ok(());
        }
        let collide = self.collides(id);
        let current = self.touching_list(id);
        let mut valid: Vec<ObjectId> = Vec::new();
        for b in 0..self.objects.len() as ObjectId {
            if b == id || !self.is_live_actor(b) {
                continue;
            }
            let touches = collide
                && self.collides(b)
                && self.actors_overlap(id, b)
                && !self.based_on(id, b)
                && !self.based_on(b, id)
                && !(skip_blocking && self.blocks_pair(id, b));
            if touches {
                valid.push(b);
            }
        }
        for old in current {
            if !valid.contains(&old) {
                self.end_touch(id, old)?;
            }
        }
        for new in valid {
            if !self.touching_list(id).contains(&new) {
                self.begin_touch(id, new)?;
            }
        }
        Ok(())
    }

    /// `Actor.Move`: world swept move then blocking-actor stop, then touch maintenance.
    /// Returns true when the whole delta was applied (no blocking hit), false when stopped.
    pub(crate) fn vm_move(&mut self, id: ObjectId, delta: [f32; 3]) -> VmResult<bool> {
        let start = self.vector_prop(id, "Location").unwrap_or([0.0; 3]);
        let extent = self.actor_extent(id);
        let mut end = add3(start, delta);
        let mut world_hit = false;
        if self.bool_prop(id, "bCollideWorld") {
            let out = match self.physics.as_mut() {
                Some(p) => p.move_box(start, delta, extent),
                None => {
                    return Err(self.err(VmErrorKind::NoPhysicsProvider {
                        native: "Actor.Move".into(),
                    }));
                }
            };
            end = out.end;
            world_hit = out.hit.is_some();
        }
        let mut blocked_actor = None;
        if self.bool_prop(id, "bCollideActors")
            && let Some((t, other)) = self.sweep_blocking_actor(id, start, end)
        {
            end = lerp3(start, end, t);
            blocked_actor = Some(other);
        }
        self.set_property(id, "Location", 0, Value::Vector(end));
        // ULevel::MoveActor's captured disassembly is truncated before its Bump event dispatch,
        // so do not infer a recipient or event order from this incomplete artifact.
        self.refresh_touching(id, true)?;
        Ok(!world_hit && blocked_actor.is_none())
    }

    /// Engine.dll `AActor::execMakeNoise` (0x103aff40): `CheckNoiseHearing(Loudness)` runs only
    /// when `Level.NetMode != NM_Client` (3) and the actor has an `Instigator`. No loudness
    /// validation happens anywhere on the engine path (a NaN or negative loudness flows into the
    /// same comparisons), so the VM performs none either.
    pub(crate) fn vm_make_noise(&mut self, source: ObjectId, loudness: f32) -> VmResult<()> {
        if !self.is_live_actor(source) {
            return Ok(());
        }
        let net_mode = self
            .obj_prop(source, "Level")
            .map_or(0, |level| self.byte_prop(level, "NetMode"));
        if net_mode == 3 || self.obj_prop(source, "Instigator").is_none() {
            return Ok(());
        }
        self.check_noise_hearing(source, loudness)
    }

    /// Engine.dll `AActor::CheckNoiseHearing` (0x1036b6d0), decoded in full (item41c). Order:
    /// instigator/controller gate, the two per-instigator
    /// noise slots (0.2 s / 50 uu / 90 % suppression, 0.18 s reuse), then the
    /// `Level.ControllerList` walk delivering `HearNoise(Loudness, self)` to every probing
    /// controller that is not the instigator's and whose `CanHear` passes. When the instigator
    /// is not a player and its controller's `Enemy` is not a player either, only controllers
    /// with the noise maker's `Tag` or a player pawn are considered.
    fn check_noise_hearing(&mut self, source: ObjectId, loudness: f32) -> VmResult<()> {
        let Some(instigator) = self.obj_prop(source, "Instigator") else {
            return Ok(());
        };
        let Some(inst_controller) = self.obj_prop(instigator, "Controller") else {
            return Ok(());
        };
        let location = self.vector_prop(source, "Location").unwrap_or([0.0; 3]);
        // `XLevel+0xd0` is a double; the VM's level clock is `Vm::time`.
        let now = self.time;
        let near = |spot: [f32; 3]| dist_sq_f64(spot, location) < 2500.0;
        let slot = |vm: &Self, n: u8| {
            (
                vm.vector_prop(instigator, &format!("noise{n}spot"))
                    .unwrap_or([0.0; 3]),
                f64::from(vm.f32_prop(instigator, &format!("noise{n}time"))),
                f64::from(vm.f32_prop(instigator, &format!("noise{n}loudness"))),
            )
        };
        let (spot1, time1, loud1) = slot(self, 1);
        let (spot2, time2, loud2) = slot(self, 2);
        let l = f64::from(loudness);
        let recent = now - f64::from(0.2f32);
        let louder = f64::from(0.9f32) * l;
        if recent < time1 && near(spot1) && louder <= loud1 {
            return Ok(());
        }
        if recent < time2 && near(spot2) && louder <= loud2 {
            return Ok(());
        }
        let reuse = now - f64::from(0.18f32);
        let target = if reuse > time1 {
            Some(1)
        } else if reuse > time2 {
            Some(2)
        } else if near(spot1) && loud1 <= l {
            Some(1)
        } else if loud2 <= l {
            // The fourth case writes slot 1 as well (decoded: stores at +0x2c4/+0x2d0/+0x2d8).
            Some(1)
        } else {
            None
        };
        if let Some(n) = target {
            self.set_property(
                instigator,
                &format!("noise{n}spot"),
                0,
                Value::Vector(location),
            );
            self.set_property(
                instigator,
                &format!("noise{n}time"),
                0,
                Value::Float(now as f32),
            );
            self.set_property(
                instigator,
                &format!("noise{n}loudness"),
                0,
                Value::Float(loudness),
            );
        }

        let broadcast = self.pawn_is_player(instigator)
            || self
                .obj_prop(inst_controller, "Enemy")
                .is_some_and(|enemy| self.pawn_is_player(enemy));
        let source_tag = self.name_prop(source, "Tag");
        let Some(level) = self.obj_prop(source, "Level") else {
            return Ok(());
        };
        let mut next = self.obj_prop(level, "ControllerList");
        let mut walked = 0usize;
        while let Some(controller) = next {
            walked += 1;
            if walked > self.objects.len() {
                return Err(self.err(VmErrorKind::Unresolved {
                    what: "Actor.MakeNoise: Level.ControllerList does not terminate (cycle)".into(),
                }));
            }
            let pawn = self.obj_prop(controller, "Pawn");
            if pawn != Some(instigator)
                && self.is_probing(controller, "HearNoise", HEAR_NOISE_PROBE_BIT)
                && (broadcast
                    || self
                        .name_prop(controller, "Tag")
                        .eq_ignore_ascii_case(&source_tag)
                    || pawn.is_some_and(|p| self.pawn_is_player(p)))
                && self.controller_can_hear(controller, location, loudness, source)?
            {
                self.send_event(
                    controller,
                    "HearNoise",
                    vec![
                        Value::Float(loudness),
                        Value::Object(Some(ObjRef::Instance(source))),
                    ],
                )?;
            }
            // The engine reads `nextController` after the event, as here.
            next = self.obj_prop(controller, "NextController");
        }
        Ok(())
    }

    /// Engine.dll `APawn::IsPlayer` (0x103aff00): `Controller != None && Controller.bIsPlayer`.
    fn pawn_is_player(&self, pawn: ObjectId) -> bool {
        self.obj_prop(pawn, "Controller")
            .is_some_and(|c| self.bool_prop(c, "bIsPlayer"))
    }

    /// `name` property text, or `"None"` when absent.
    fn name_prop(&self, id: ObjectId, name: &str) -> String {
        match self.get_property(id, name) {
            Some(Value::Name(n)) => n.clone(),
            _ => "None".to_owned(),
        }
    }

    /// Core.dll `UObject::IsProbing` (0x10102da0) for a probe name (EName 300..363): bit
    /// `name - 300` of the state frame's probe mask, which `UObject::GotoState` (0x1011eb10) sets
    /// to `(Class.ProbeMask | Node.ProbeMask) & Node.IgnoreMask` with `Node` = the current state
    /// (the class itself without one). The stored masks are already cumulative over super classes
    /// (measured, item41c). A `Disable(probe)` recorded by the VM also clears the probe.
    pub(crate) fn is_probing(&self, id: ObjectId, probe: &str, bit: u32) -> bool {
        let Some(o) = self.objects.get(id as usize) else {
            return false;
        };
        if o.disabled.contains(&lower(probe)) {
            return false;
        }
        let masks = |g: GlobalRef| match self.set.object(g) {
            Some(ScriptObject::Class(c)) => Some((c.state.probe_mask, c.state.ignore_mask)),
            Some(ScriptObject::State(s)) => Some((s.state.probe_mask, s.state.ignore_mask)),
            _ => None,
        };
        let (class_probe, class_ignore) = masks(o.class).unwrap_or((0, u64::MAX));
        let (node_probe, node_ignore) = o
            .state
            .and_then(masks)
            .unwrap_or((class_probe, class_ignore));
        ((class_probe | node_probe) & node_ignore) >> bit & 1 != 0
    }

    /// Engine.dll `AController::CanHear(NoiseLoc, Loudness, Other)` (0x1036b0f0). Every branch
    /// is decoded; the parts the VM cannot evaluate exactly are labelled Partial below and
    /// reported once per VM with a trace note (never silently).
    fn controller_can_hear(
        &mut self,
        controller: ObjectId,
        noise: [f32; 3],
        loudness: f32,
        other: ObjectId,
    ) -> VmResult<bool> {
        let other_controller = self
            .obj_prop(other, "Instigator")
            .and_then(|i| self.obj_prop(i, "Controller"));
        let Some(pawn) = self.obj_prop(controller, "Pawn") else {
            return Ok(false);
        };
        if other_controller.is_none() {
            return Ok(false);
        }
        let pawn_loc = self.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let dist_sq = dist_sq_f64(pawn_loc, noise) as f32;
        let threshold = f64::from(self.f32_prop(pawn, "HearingThreshold"));
        let alert = f64::from(self.f32_prop(pawn, "Alertness")) + 1.0;
        let alert = if 0.0 < alert { alert } else { 0.0 };
        let perceived_ext = threshold * threshold * f64::from(loudness) * alert;
        // `Perceived < DistSq` returns 0; a NaN Perceived passes (decoded flag test).
        if perceived_ext < f64::from(dist_sq) {
            return Ok(false);
        }
        let perceived = perceived_ext as f32;

        if self.bool_prop(pawn, "bSameZoneHearing") || self.bool_prop(pawn, "bAdjacentZoneHearing")
        {
            // Partial: the engine compares `Region.Zone` of the listener pawn and the noise
            // maker, then (bAdjacentZoneHearing) the BSP zone connectivity mask. The VM keeps no
            // zone model (Region is not updated as actors move), so the zone test is treated as
            // "different, unconnected zones" and the remaining branches decide.
            self.hearing_partial(
                "zone",
                "CanHear zone hearing (bSameZoneHearing/bAdjacentZoneHearing) is not modelled: \
                 the VM has no zone model, the zone test is treated as different unconnected zones",
            );
        }
        if !self.bool_prop(pawn, "bLOSHearing") {
            return Ok(false);
        }
        let eye = self.f32_prop(pawn, "BaseEyeHeight");
        let view = [pawn_loc[0], pawn_loc[1], pawn_loc[2] + eye];
        if !self.physics_ready("Actor.MakeNoise", Some(512), controller, Value::Void)? {
            return Ok(false);
        }
        // SingleLineCheck(End = NoiseLoc, Start = ViewLoc, TRACE_World | TRACE_StopAtFirstHit):
        // world geometry including movers, no pawns. The provider's world trace is that query.
        let world_hit = self.world_line(view, noise);
        if world_hit.is_none() {
            return Ok(true);
        }

        if self.bool_prop(pawn, "bMuffledHearing") && perceived > 4.0 * dist_sq {
            // Partial: the engine's two wall traces use TRACE_Level (BSP only); the provider has
            // no BSP-only query, so its world trace stands in. On a miss the reused
            // FCheckResult keeps its previous Location ((0,0,0) from the constructor at first;
            // hypothesis: SingleLineCheck does not write Location on a miss).
            self.hearing_partial(
                "muffled",
                "CanHear bMuffledHearing wall traces use the world trace in place of the engine's \
                 BSP-only TRACE_Level check",
            );
            let mut hit_location = [0.0f32; 3];
            if let Some(h) = self.world_line(view, noise) {
                hit_location = h.location;
            }
            let first = hit_location;
            if let Some(h) = self.world_line(noise, view) {
                hit_location = h.location;
            }
            // `FVector::SizeSquared` stays on the x87 stack (not rounded to float) and is then
            // squared again: the decoded test is `Perceived > W*W + 4*DistSq` with W = |A-B|^2.
            let wall = dist_sq_f64(first, hit_location);
            if f64::from(perceived) > wall * wall + f64::from(4.0 * dist_sq) {
                return Ok(true);
            }
        }

        if !self.bool_prop(pawn, "bAroundCornerHearing") {
            return Ok(false);
        }
        let corner = perceived * 0.125;
        let other_loc = self.vector_prop(other, "Location").unwrap_or([0.0; 3]);
        let mut list = SortedPathList::default();
        let level = self.obj_prop(controller, "Level");
        let mut next = level.and_then(|l| self.obj_prop(l, "NavigationPointList"));
        let mut walked = 0usize;
        while let Some(nav) = next {
            walked += 1;
            if walked > self.objects.len() {
                return Err(self.err(VmErrorKind::Unresolved {
                    what: "Actor.MakeNoise: Level.NavigationPointList does not terminate (cycle)"
                        .into(),
                }));
            }
            if self.bool_prop(nav, "bPropagatesSound") {
                let nav_loc = self.vector_prop(nav, "Location").unwrap_or([0.0; 3]);
                let d1 = dist_sq_f64(nav_loc, pawn_loc) as f32;
                let d2 = dist_sq_f64(nav_loc, other_loc) as f32;
                if d1 < corner && d2 < corner {
                    list.add(nav, d2 + d1);
                }
            }
            next = self.obj_prop(nav, "nextNavigationPoint");
        }
        if list.count == 0 {
            return Ok(false);
        }
        // Partial: `UModel::FastLineCheck` is a BSP-only line test; the provider's world trace
        // (which also sees static meshes, terrain and movers) stands in for it.
        self.hearing_partial(
            "corner",
            "CanHear bAroundCornerHearing uses the world trace in place of the engine's BSP-only \
             FastLineCheck",
        );
        for &nav in &list.nodes[..list.count] {
            let nav_loc = self.vector_prop(nav, "Location").unwrap_or([0.0; 3]);
            if self.world_line(noise, nav_loc).is_none() && self.world_line(view, nav_loc).is_none()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Zero-extent world line trace through the installed provider (callers check
    /// [`Vm::physics_ready`] first).
    fn world_line(&mut self, start: [f32; 3], end: [f32; 3]) -> Option<WorldHit> {
        self.physics
            .as_mut()
            .and_then(|p| p.trace(start, end, [0.0; 3]))
    }

    /// Records a Partial hearing branch once per VM as a visible trace note.
    fn hearing_partial(&mut self, key: &'static str, text: &str) {
        if self.hearing_partials.insert(key) {
            self.note(TraceKind::Note(format!("Partial: {text}")));
        }
    }

    /// `Actor.SetLocation`: teleport when the destination is free of world geometry and not
    /// encroached by a blocking actor; returns whether it moved. Touch relations are updated.
    pub(crate) fn vm_set_location(&mut self, id: ObjectId, location: [f32; 3]) -> VmResult<bool> {
        // FarMoveActor 0x1038a489 refuses bStatic or !bMovable outside the editor.
        // Generated fixtures without the native bMovable property use Actor's true default.
        if self.bool_prop(id, "bStatic")
            || matches!(self.get_property(id, "bMovable"), Some(Value::Bool(false)))
        {
            return Ok(false);
        }
        let extent = self.actor_extent(id);
        let check_world =
            self.bool_prop(id, "bCollideWorld") || self.bool_prop(id, "bCollideWhenPlacing");
        if check_world {
            let free = match self.physics.as_mut() {
                Some(p) => p.point_free(location, extent),
                None => {
                    return Err(self.err(VmErrorKind::NoPhysicsProvider {
                        native: "Actor.SetLocation".into(),
                    }));
                }
            };
            if !free {
                return Ok(false);
            }
        }
        // A scripted `SetLocation` with world/placement collision disabled is the UE2 cutscene
        // teleport path: `BeachInBedWithXIII.Waiting.Tick` moves the player into the authored bed
        // pose while `bCollideWorld == false`. The Plage01 bed pose overlaps the map's closed
        // window actor cylinders; UE2 permits this scripted placement in collisionless mode.
        // Normal encroachment rejection remains active whenever either collision check is enabled.
        if self.bool_prop(id, "bCollideActors") && check_world {
            for b in 0..self.objects.len() as ObjectId {
                if b == id || !self.is_live_actor(b) || !self.blocks_pair(id, b) {
                    continue;
                }
                let (bl, rb, hb) = self.actor_cylinder(b);
                let (_, ri, hi) = self.actor_cylinder(id);
                if cylinders_overlap(location, ri, hi, bl, rb, hb) {
                    return Ok(false);
                }
            }
        }
        // FarMoveActor 0x1038a5c1 marks a successful non-test teleport, and its
        // non-attached path calls SetBase(None, (0,0,1), true) before the Location store.
        self.set_property(id, "bJustTeleported", 0, Value::Bool(true));
        self.set_property(id, "Base", 0, Value::Object(None));
        self.set_property(id, "Floor", 0, Value::Vector([0.0, 0.0, 1.0]));
        self.set_property(id, "Location", 0, Value::Vector(location));
        self.refresh_touching(id, false)?;
        Ok(true)
    }

    /// Whether actor `b` can be returned by an actor line/box check of the given extent kind.
    ///
    /// From `Engine.dll` (item40e): an actor check only walks the level's collision hash
    /// (`FCollisionHash::ActorLineCheck` @ 0x10349C60), `FCollisionHash::AddActor` @ 0x10349980
    /// asserts that the added actor has `bCollideActors`, and `AActor::SetCollision` @ 0x103527D0
    /// removes the actor from the hash before the flags change and adds it back only when the new
    /// `bCollideActors` is set. So an actor with `bCollideActors=false` (lights, nav points, hidden
    /// info actors with the `Actor` defaults `bBlockZeroExtentTraces=true`) is never hit. Among
    /// hash members a zero-extent check also needs `bBlockZeroExtentTraces`: `ActorLineCheck`
    /// tests bit 0x400 of the actor's collision bitfield at 0x10349E90 / 0x10349FDC, the bit
    /// after `bProjTarget` (bit 0x200, `AActor::ShouldTrace` @ 0x10354640) in `Actor`'s
    /// declaration order. The non-zero-extent flag `bBlockNonZeroExtentTraces` for box checks is
    /// upstream UE2 (not located in the disassembly).
    fn actor_blocks_trace(&self, b: ObjectId, nonzero_extent: bool) -> bool {
        if !self.bool_prop(b, "bCollideActors") {
            return false;
        }
        if nonzero_extent {
            self.bool_prop(b, "bBlockNonZeroExtentTraces")
        } else {
            self.bool_prop(b, "bBlockZeroExtentTraces")
        }
    }

    /// Whether the actor carries a placed static mesh (a non-null `StaticMesh` property). The
    /// engine traces such an actor against its mesh kDOP, not a collision cylinder (item53).
    fn actor_has_static_mesh(&self, id: ObjectId) -> bool {
        matches!(
            self.get_property(id, "StaticMesh"),
            Some(Value::Object(Some(_)))
        )
    }

    /// World-only actor trace for `Actor.Trace` when `bTraceActors` is set. Returns the
    /// nearest hit as `(time, actor, normal)`; grown cylinders approximate the extent box.
    /// Static-mesh actors are refined against their own mesh triangles through the provider
    /// (item53): a ray through a mesh opening (a window) must not hit the actor.
    fn trace_actors(
        &mut self,
        id: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> Option<(f32, ObjectId, [f32; 3])> {
        self.trace_actors_flags(id, start, end, extent, 0xbf)
    }

    fn trace_actors_flags(
        &mut self,
        id: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
        flags: u32,
    ) -> Option<(f32, ObjectId, [f32; 3])> {
        let nonzero = extent[0] + extent[1] + extent[2] > 0.0;
        // First pass (immutable): admitted candidates and their cylinder hits. The provider
        // refinement for static-mesh actors runs afterwards so the mutable provider borrow does
        // not alias the object walk.
        let mut candidates: Vec<(f32, ObjectId, [f32; 3])> = Vec::new();
        for b in 0..self.objects.len() as ObjectId {
            if b == id || !self.is_live_actor(b) {
                continue;
            }
            if !self.trace_admits_actor_flags(b, id, nonzero, flags) {
                continue;
            }
            let (lb, rb, hb) = self.actor_cylinder(b);
            let r = rb + extent[0].max(0.0);
            let hh = hb + extent[2].max(0.0);
            // item52: the engine's per-actor cylinder routine (VA 0x103c4cd0..0x103c5573, the
            // vtable+0x70 primitive dispatched by `FCollisionHash::ActorLineCheck`
            // 0x10349c60/0x1034a4d6) answers a line that starts inside (or within 1 UU of the
            // surface of: `dist^2 - R^2 < 1.0` at 0x103c5272, coefficients decoded at
            // 0x103c5204..0x103c526f) the candidate cylinder with the EXIT hit, not the entry:
            // the quadratic in the horizontal direction (0x103c5401..0x103c544a), no hit when the
            // discriminant is negative (0x103c53c4..0x103c53d6, moving away), the on-axis
            // degenerate (`|a| < 1e-8`, 0x103c53d8..0x103c53e6) folded into the same tail, and
            // the hit time `Min3(T-0.001, 1.0, T_exit)` (constants 0x10480600 = 0.001 and
            // 0x10311e00 = Min3 at 0x103c54e9..0x103c54fc) with the location on the ray and the
            // normal = -Dir (0x103c5529..0x103c5566). Measured need: Hual01a/Plage01 point-blank
            // shots — the muzzle (`GetFireStart`, eye + 16 UU forward) sits inside the target's
            // cylinder after the fight-test walk, and retail reports the hit there.
            let u = [start[0] - lb[0], start[1] - lb[1]];
            let dist2 = u[0] * u[0] + u[1] * u[1];
            // Conservative deviation (labelled): the decode routes z-outside starts into the
            // same exit-root tail (the z-range checks at 0x103c5286/0x103c5297 jump to
            // 0x103c53c4), but applying the exit rule to rays that merely pass above/below a
            // cylinder would report hits the retail game demonstrably does not produce; the
            // exit rule is therefore applied only to starts inside the full 3D cylinder.
            let start_inside = dist2 - r * r < 1.0 && (start[2] - lb[2]).abs() <= hh;
            let hit = if start_inside {
                let d = sub3(end, start);
                let a = d[0] * d[0] + d[1] * d[1];
                if a <= 1e-8 {
                    // On-axis: the engine takes the degenerate tail with no exit root
                    // (0x103c53e8..0x103c53f6 jumps to 0x103c54e9 when `|a| < 1e-8`).
                    Some((0.999, [-d[0], -d[1], -d[2]]))
                } else {
                    let bcoef = 2.0 * (u[0] * d[0] + u[1] * d[1]);
                    let ccoef = dist2 - r * r;
                    let disc = bcoef * bcoef - 4.0 * a * ccoef;
                    if disc < 0.0 {
                        // Moving away with no exit (0x103c53c4..0x103c53d6): no hit.
                        None
                    } else {
                        let t_exit = (-bcoef + disc.sqrt()) / (2.0 * a);
                        // A negative exit root means the cylinder is entirely behind the ray
                        // (grazing start within 1 UU of the surface); the engine's caller
                        // discards such a Time, so no hit is reported.
                        if t_exit < 0.0 {
                            None
                        } else {
                            Some((t_exit.min(0.999), [-d[0], -d[1], -d[2]]))
                        }
                    }
                }
            } else {
                segment_cylinder_hit(start, end, lb, r, hh)
            };
            let Some((t, n)) = hit else {
                continue;
            };
            candidates.push((t, b, normalize3(n)));
        }
        // Second pass: a static-mesh actor's zero-extent trace uses its own mesh triangles
        // (UE2 traces the kDOP, item53). A mesh that misses the ray does not block; when the
        // provider has no per-actor mesh data, or the extent is non-zero (a swept-box kDOP is
        // not modelled), the cylinder approximation stands. item52: a mover with
        // `bUseCylinderCollision` collides as its cylinder, not its placed mesh, so it keeps the
        // first-pass cylinder hit (which already applies the start-inside exit rule).
        let mut best: Option<(f32, ObjectId, [f32; 3])> = None;
        let zero_extent = !nonzero;
        for (t, b, n) in candidates {
            let outcome = if zero_extent
                && self.actor_has_static_mesh(b)
                && !(self.is_mover(b) && self.bool_prop(b, "bUseCylinderCollision"))
            {
                self.objects
                    .get(b as usize)
                    .map(|o| o.name.clone())
                    .and_then(|name| {
                        self.physics
                            .as_mut()
                            .map(|p| p.actor_mesh_hit(&name, start, end))
                    })
            } else {
                None
            };
            match outcome {
                Some(crate::physics::ActorMeshHit::Hit(hit)) => {
                    if best.is_none_or(|(bt, _, _)| hit.time <= bt) {
                        best = Some((hit.time, b, hit.normal));
                    }
                }
                // The actor's mesh is traceable and the ray passes through it: no hit.
                Some(crate::physics::ActorMeshHit::Miss) => {}
                // item53b: a REGISTERED mover without `bUseCylinderCollision` answers the line
                // check with its MESH (the vtable+0x70 primitive; the `byte [+0x30] & 0x40`
                // early-exit hypothesis of item52), and that mesh is already in the provider's
                // moving world — the world hit in `vm_trace_flags` carries it. The
                // Engine.Mover class-default cylinder (CollisionRadius/Height 160, measured) is
                // NOT its line-check geometry: it blocked Toits01's through-window bullet/focus
                // sight lines ~136 UU in front of the `PorteDecors18` shutter mesh (entry root
                // of the r=160 cylinder around the actor origin, measured t=0.0062 vs the mesh
                // face t=0.0120). Skip the cylinder fallback only for those.
                // An UNREGISTERED mover without `bUseCylinderCollision` has no mesh the VM can
                // reach (its per-actor query returns `NoData` with nothing behind it); the
                // cylinder stays as the only approximation there — Hual01a's guard sight lines
                // depend on it (the t=182 `EnemyNotVisible` transition regressed when the
                // fallback was dropped, measured).
                // With `bUseCylinderCollision` the cylinder IS the primitive (item52 lever).
                Some(crate::physics::ActorMeshHit::NoData) | None
                    if self.is_mover(b)
                        && !self.bool_prop(b, "bUseCylinderCollision")
                        && self.physics.as_ref().is_some_and(|p| {
                            self.objects
                                .get(b as usize)
                                .map(|o| p.mover_is_registered(&o.name))
                                .unwrap_or(false)
                        }) => {}
                // No per-actor mesh data (or no provider): the cylinder approximation.
                Some(crate::physics::ActorMeshHit::NoData) | None => {
                    if best.is_none_or(|(bt, _, _)| t <= bt) {
                        best = Some((t, b, n));
                    }
                }
            }
        }
        best
    }

    /// The engine's actor-trace candidate filter, decoded from Engine.dll.
    ///
    /// 1. Hash membership: a candidate must be in the collision hash. Every insert/remove site
    ///    gates on `bCollideActors` (`ULevel::SpawnActor` VA 0x10388d3f/0x10388e50,
    ///    `ULevel::SetActorCollision` VA 0x10391b0c, `ULevel::FarMoveActor` VA 0x1038a4b9/0x1038a6df,
    ///    `AActor::SetCollision` VA 0x103527d0, the runtime re-add at VA 0x103536ce): the map-load
    ///    walk and every spawn/move path add an actor only while `byte [actor+0x34] & 0x20` holds.
    ///    `FCollisionHash::ActorLineCheck` (VA 0x10349c60) walks hash buckets only, so an actor
    ///    with `bCollideActors=false` is never a candidate. XIII's own scripts use this: a pawn
    ///    parked by `IAController.faction.BeginState` gets `SetCollision(false,false,false)` and
    ///    is invisible (`SetDrawType(0)`) until its controller leaves the state
    ///    (`faction.EndState` restores `SetCollision(true,true,true)`).
    /// 2. Extent prefilter: the hash walk (`ActorLineCheck` VAs 0x10349e90/0x10349fdc) and the
    ///    octree zero-extent path (VA 0x103a439b) require bit 10 (`bBlockZeroExtentTraces`); the
    ///    octree non-zero-extent path (VA 0x103a47f1) requires bit 11
    ///    (`bBlockNonZeroExtentTraces`).
    /// 3. `ShouldTrace` (`AActor::ShouldTrace` VA 0x10354640): for the script-trace flag word
    ///    (`execTrace` VA 0x103e8abf composes `0x86`/`0xBF | extra`, `SingleLineCheck` forces
    ///    `| 0x400`; bullets add `0x4040` from `XIIIWeapon.RealTraceFire`):
    ///    - `APawn::ShouldTrace` (VA 0x10305d20) returns `TraceFlags & 1` — always set for
    ///      script traces, so an in-hash pawn is always admitted;
    ///    - `AMover`/`ADecoration::ShouldTrace` (shared VA 0x10306c70) return `TraceFlags & 2` —
    ///      also always set, so in-hash movers/decorations are admitted;
    ///    - other actors: a world-geometry actor (bit 30 of `+0x2c`, same bit `IsBlockedBy`
    ///      VA 0x10315620 tests) is admitted because `TraceFlags & 0x80` is set; otherwise
    ///      `TraceFlags & 0x10` and `TraceFlags & 0x20` are both set, which returns
    ///      `bProjTarget || (bBlockActors && bBlockPlayers)`. (`bProjTarget` is bit 9 of
    ///      `+0x34`; the name is an inference from the C++ bitfield order and the
    ///      `execPickTarget` bit-9 gate, not from an Engine.dll string.)
    ///
    /// `bHidden` is deliberately NOT tested: it appears nowhere in the hash insert/remove paths,
    /// the hash walk, or any `ShouldTrace` override. A carried first-person weapon does not block
    /// because its class defaults clear `bCollideActors` (`xiii.Fists`, measured).
    fn trace_admits_actor(&self, candidate: ObjectId, tracer: ObjectId, nonzero: bool) -> bool {
        self.trace_admits_actor_flags(candidate, tracer, nonzero, 0xbf)
    }

    fn trace_admits_actor_flags(
        &self,
        candidate: ObjectId,
        tracer: ObjectId,
        nonzero: bool,
        flags: u32,
    ) -> bool {
        if !self.bool_prop(candidate, "bCollideActors") {
            return false;
        }
        let extent_gate = if nonzero {
            self.bool_prop(candidate, "bBlockNonZeroExtentTraces")
        } else {
            self.bool_prop(candidate, "bBlockZeroExtentTraces")
        };
        if !extent_gate {
            return false;
        }
        // UE2's trace ignores both the tracer's owners and its owned attachments
        // (`SingleLineCheck` calls `IsOwnedBy` at VA 0x1038ba1c; item18 B11 measured the
        // tracer-owned direction: `XIII.M60`'s line hit its own `StarFPMF` first-person mesh).
        if self.is_owned_by(candidate, tracer) || self.is_owned_by(tracer, candidate) {
            return false;
        }
        if self.class_chain_contains(candidate, "pawn") {
            // item52: trace-flag bit 0x2000 (`AdditionalTraceType`) excludes pawn candidates.
            // Measured: `execTrace` composes `(bTraceActors ? 0x39 : 0) + 0x86 | 0x1000? | extra`
            // (VA 0x103e8abf..0x103e8b2e) and `AActor::ShouldTrace` (VA 0x10354640..0x10354753)
            // tests no pawn-specific bit — yet the retail `CWndFocusTrigger.WaitForBeingSeen`
            // sight trace (`Trace(..., XPP.Location, true, vect(0,0,0), HitMat, 8192)`, composed
            // flags 0x20bf) must return `None` although it starts inside the player pawn's
            // cylinder and the pawn's serialized class defaults block zero-extent traces
            // (bBlockZeroExtentTraces=true, imported from XIII.u). The 0x2000 bit is the only
            // structural difference from the default 0xbf weapon traces that must hit pawns.
            // Labelled hypothesis: the exact Engine.dll gate for bit 0x2000 was not located
            // (the property-flag tests at 0x1031ec3a/0x10348205 are object/actor bits, not
            // trace flags); the rule is pinned by the retail behaviour on both sides.
            if flags & 0x2000 != 0 {
                return false;
            }
            return flags & 1 != 0;
        }
        if self.class_chain_contains(candidate, "mover")
            || self.class_chain_contains(candidate, "decoration")
        {
            return flags & 2 != 0;
        }
        if self.bool_prop(candidate, "bWorldGeometry") {
            return flags & 0x80 != 0;
        }
        if flags & 0x10 == 0 {
            return false;
        }
        if flags & 0x20 != 0 {
            return self.bool_prop(candidate, "bProjTarget")
                || (self.bool_prop(candidate, "bBlockActors")
                    && self.bool_prop(candidate, "bBlockPlayers"));
        }
        // 0x103546e2: all other actors when neither projectile-only nor blocking-only.
        if flags & 0x40 == 0 {
            return true;
        }
        self.bool_prop(candidate, "bBlockActors") && self.bool_prop(candidate, "bBlockPlayers")
    }

    pub(crate) fn vm_trace_actors(
        &mut self,
        caller: ObjectId,
        base: Option<GlobalRef>,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> VmResult<Vec<ActorTraceHit>> {
        let Some(provider) = self.physics.as_mut() else {
            return Err(self.err(VmErrorKind::NoPhysicsProvider {
                native: "Actor.TraceActors".into(),
            }));
        };
        // UE2's iterator returns actors intersected by the swept trace; world geometry occludes
        // candidates at or beyond the first world hit. item54: a hit sourced from the candidate's
        // own primitive (a mover's mesh triangles, item53's placed-mesh actors) must not occlude
        // that actor itself — Engine.dll's per-actor line check answers through the actor's own
        // primitive virtual, and the crawl grille BreakableMover13 sits at the end of a tunnel
        // whose only forward geometry is the grille itself.
        let world_raw = provider.trace_with_mover(start, end, extent);
        let (world_t, world_actor) = match world_raw {
            (Some(hit), name) => {
                let named = name.and_then(|n| self.find_live_object(&n)).filter(|&m| {
                    m != caller && (self.is_mover(m) || self.actor_has_static_mesh(m))
                });
                (Some(hit.time), named)
            }
            (None, _) => (None, None),
        };
        let occludes = |t: f32, candidate: ObjectId| {
            world_t.is_none_or(|limit| t < limit) || world_actor == Some(candidate)
        };
        let nonzero = extent.iter().any(|v| *v != 0.0);
        let zero_extent = !nonzero;
        // item53: a static-mesh actor's zero-extent candidate is refined against its own mesh
        // triangles (the engine traces the kDOP); a ray through a mesh opening does not hit.
        let mut mesh_candidates: Vec<ObjectId> = Vec::new();
        let mut hits = Vec::new();
        for id in 0..self.objects.len() as ObjectId {
            if id == caller || !self.is_live_actor(id) {
                continue;
            }
            if base.is_some_and(|class| !self.objects[id as usize].layout.chain.contains(&class)) {
                continue;
            }
            if !self.trace_admits_actor(id, caller, nonzero) {
                continue;
            }
            let (loc, radius, height) = self.actor_cylinder(id);
            let cylinder = segment_cylinder_hit(
                start,
                end,
                loc,
                radius + extent[0].max(0.0),
                height + extent[2].max(0.0),
            );
            // item54: the broad-phase pre-filter must also admit a start point inside the
            // nominal cylinder (the crawl grille's class-default r/h=160 spans the whole tunnel,
            // so the punch ray starts inside it and `segment_cylinder_hit` has no entry hit).
            let start_inside_cylinder = {
                let dx = start[0] - loc[0];
                let dy = start[1] - loc[1];
                let dz = (start[2] - loc[2]).abs();
                dx * dx + dy * dy <= (radius + extent[0].max(0.0)).powi(2)
                    && dz <= height + extent[2].max(0.0)
            };
            if zero_extent && self.actor_has_static_mesh(id) {
                if cylinder.is_some() || start_inside_cylinder {
                    mesh_candidates.push(id);
                }
                continue;
            }
            if let Some((t, normal)) = cylinder
                && occludes(t, id)
            {
                hits.push((t, id, normal));
            }
        }
        for id in mesh_candidates {
            let Some(name) = self.objects.get(id as usize).map(|o| o.name.clone()) else {
                continue;
            };
            let (cyl_loc, cyl_radius, cyl_height) = self.actor_cylinder(id);
            let cylinder_fallback = || {
                segment_cylinder_hit(
                    start,
                    end,
                    cyl_loc,
                    cyl_radius + extent[0].max(0.0),
                    cyl_height + extent[2].max(0.0),
                )
            };
            let outcome = self
                .physics
                .as_mut()
                .map(|p| p.actor_mesh_hit(&name, start, end))
                .unwrap_or(crate::physics::ActorMeshHit::NoData);
            let hit = match outcome {
                crate::physics::ActorMeshHit::Hit(hit) => Some((hit.time, hit.normal)),
                // item54: a registered mover whose mesh model misses the ray still answers
                // through its collision cylinder — the class-authored interactive volume
                // (BreakableMover r/h=160) that `DrawInteractions`' TargDist gate is sized for;
                // the registered triangle set is the render mesh's simplified model (a thin
                // grille quad) and does not fill it. Placed static-mesh actors (item53) keep
                // strict mesh semantics: a ray through a mesh opening does not hit.
                crate::physics::ActorMeshHit::Miss if self.is_mover(id) => cylinder_fallback(),
                crate::physics::ActorMeshHit::Miss => None,
                crate::physics::ActorMeshHit::NoData => cylinder_fallback(),
            };
            // item54: report the mesh hit's own normal — `ReturnTrace` validates the pick with a
            // second trace to `HitLoc - HitNorm`, which needs the surface normal, not a placeholder.
            if let Some((t, normal)) = hit
                && occludes(t, id)
            {
                hits.push((t, id, normal));
            }
        }
        hits.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        Ok(hits
            .into_iter()
            .map(|(t, id, normal)| (id, lerp3(start, end, t), normal))
            .collect())
    }

    /// `Actor.Trace`: nearest of world (provider) and, when `bTraceActors`, actor cylinders;
    /// world hits return the map's `LevelInfo` (upstream), no hit returns `None`.
    /// Fills `(hit_actor, hit_location, hit_normal)`.
    #[allow(clippy::type_complexity)]
    #[cfg(test)]
    pub(crate) fn vm_trace(
        &mut self,
        id: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
        b_trace_actors: bool,
        extent: [f32; 3],
    ) -> VmResult<(Option<ObjectId>, [f32; 3], [f32; 3])> {
        self.vm_trace_flags(
            id,
            start,
            end,
            if b_trace_actors { 0xbf } else { 0x86 },
            extent,
        )
    }

    /// Script Trace's composed flags; pawn/mover category bits remain effective even when
    /// bTraceActors was false. Special BSP/material filtering remains Partial.
    #[allow(clippy::type_complexity)]
    #[cfg(test)]
    pub(crate) fn vm_trace_flags(
        &mut self,
        id: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
        flags: u32,
        extent: [f32; 3],
    ) -> VmResult<(Option<ObjectId>, [f32; 3], [f32; 3])> {
        self.vm_trace_flags_impl(id, start, end, flags, extent, false)
    }

    /// Script execTrace has zero miss outputs and only updates the last-bone cache
    /// when TRACE_HitBoxes (0x10000) was requested. Internal query users keep their
    /// endpoint-on-miss convention and existing hit-zone query behavior.
    #[allow(clippy::type_complexity)]
    pub(crate) fn vm_script_trace(
        &mut self,
        id: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
        flags: u32,
        extent: [f32; 3],
    ) -> VmResult<(Option<ObjectId>, [f32; 3], [f32; 3])> {
        self.vm_trace_flags_impl(id, start, end, flags, extent, true)
    }

    #[allow(clippy::type_complexity)]
    fn vm_trace_flags_impl(
        &mut self,
        id: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
        flags: u32,
        extent: [f32; 3],
        script: bool,
    ) -> VmResult<(Option<ObjectId>, [f32; 3], [f32; 3])> {
        // item52: a mover with `bUseCylinderCollision` collides as its cylinder, not its placed
        // mesh. Measured need (Hual01a): the lever `XIIIMover6` (pivot (4489.4,-5437.4,-85),
        // cylinder r=10/h=50, `bUseCylinderCollision=true`, Rotation pitch -22.5°/yaw 180°,
        // DrawScale 0.5) leans its handle mesh (`statichual01.manette`, local bounds
        // [-10.7,-4.3,-19.5]..[4.3,4.3,141.2]) west across EVERY sight line to the bridge-focus
        // target `CWndTarget1` (4474,-5437,-50) — xiii-tool box-probe measures the mesh hit at
        // t=0.976 of the line, triangle (4460.9,-5435.5,-20.4)..(4491.6,-5439.4,-94.6), while
        // the target sits 15.4 UU short of the lever's cylinder, so retail (cylinder collision)
        // admits the line and the port's mesh hit suspended the map's only `PontA`
        // bridge-close. Engine.dll: the per-actor line-check geometry is the actor's own
        // primitive virtual (`FCollisionHash::ActorLineCheck` VA 0x10349c60 dispatches through
        // vtable+0x70 at 0x1034a4d6/0x10349f2e), and the actor flag bit `byte [+0x30] & 0x40`
        // is tested in the engine's collision helpers (0x1038e800/0x1038f150/0x1038f3c0) with
        // early exits that skip the mesh-shaped path — hypothesis (labelled): that bit is
        // `bUseCylinderCollision` and the primitive virtual answers with the cylinder. The
        // authored flag on XIIIMover6 is only meaningful under exactly that rule.
        let nonzero = extent.iter().any(|v| *v != 0.0);
        const MAX_MOVER_MESH_SKIPS: usize = 8;
        let d = sub3(end, start);
        let total = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
        let mut cursor = start;
        let mut advance = 0.0f32;
        let mut world: Option<super::physics::WorldHit> = None;
        let mut mover: Option<ObjectId> = None;
        for _ in 0..=MAX_MOVER_MESH_SKIPS {
            let remaining = total - advance;
            if remaining <= 1e-3 {
                break;
            }
            let (w, mv) = match self.physics.as_mut() {
                Some(p) => p.trace_with_mover(cursor, end, extent),
                None => {
                    return Err(self.err(VmErrorKind::NoPhysicsProvider {
                        native: "Actor.Trace".into(),
                    }));
                }
            };
            let Some(hit) = w else {
                break;
            };
            let named = mv
                .and_then(|name| self.find_live_object(&name))
                .filter(|&m| m != id && (self.is_mover(m) || self.actor_has_static_mesh(m)));
            let cylinder_mover = named
                .filter(|&m| self.actor_blocks_trace(m, nonzero))
                .filter(|&m| self.bool_prop(m, "bUseCylinderCollision"));
            let hit_global = advance + hit.time * remaining;
            match cylinder_mover {
                Some(m) => {
                    let (c, r, hh) = self.actor_cylinder(m);
                    match segment_cylinder_hit(start, end, c, r, hh) {
                        // The cylinder also blocks at or before the mesh hit: it is the
                        // collision shape, report the mover at the cylinder time.
                        Some((t, n)) if t * total <= hit_global + 1.0 => {
                            world = Some(super::physics::WorldHit {
                                location: lerp3(start, end, t),
                                normal: n,
                                time: t,
                            });
                            mover = named;
                            break;
                        }
                        // The mesh alone blocks: skip past the mesh hit and re-query.
                        _ => {
                            let skip = hit_global + 1.0;
                            if skip >= total {
                                break;
                            }
                            cursor = lerp3(start, end, skip / total);
                            advance = skip;
                        }
                    }
                }
                None => {
                    world = Some(hit);
                    mover = named;
                    break;
                }
            }
        }
        // item40e: a hit on a registered mover's geometry returns that mover (a collision-hash
        // actor in UE2) when it blocks this kind of trace; other world hits return the level.
        // item53: a placed static-mesh actor's own triangles are its trace collision in UE2, so
        // a world hit sourced from `"<actor> -> <mesh>"` returns that actor as well (Still under
        // the same ShouldTrace/collision-flags gates). The item52 cylinder-mover loop above
        // already restricts the carried name to movers and static-mesh actors; the same gates
        // chain here.
        let hit_actor = mover.filter(|&m| {
            m != id
                && self.actor_blocks_trace(m, nonzero)
                && self.trace_admits_actor_flags(m, id, nonzero, flags)
                && (self.is_mover(m) || self.actor_has_static_mesh(m))
        });
        let mut best: Option<(f32, Option<ObjectId>, [f32; 3])> =
            world.map(|h| (h.time, hit_actor, h.normal));
        if let Some((t, b, n)) = self.trace_actors_flags(id, start, end, extent, flags)
            && best.is_none_or(|(bt, _, _)| t <= bt)
        {
            best = Some((t, Some(b), n));
        }
        let out = match best {
            Some((t, Some(b), n)) => (Some(b), lerp3(start, end, t), n),
            Some((t, None, n)) => (self.find_level_info(), lerp3(start, end, t), n),
            None => (None, if script { [0.0; 3] } else { end }, [0.0; 3]),
        };
        // item14: record the hit zone for `Actor.GetLastTraceBone` (`XIIIPawn.LastBoneHit`).
        // A world/LevelInfo hit is not a pawn, so the bone stays `None`.
        //
        // item14b: when the installed provider holds the target's posed decoded skeleton, the
        // bullet ray is intersected with the per-bone hit boxes (`ray_bone`) and the nearest box's
        // bone name wins; otherwise the collision-cylinder classification is the fallback. The
        // ray is the exact trace segment, not the hit point, because a body's boxes can be
        // smaller than the cylinder.
        if !script || flags & 0x10000 != 0 {
            self.last_trace_bone = match out.0 {
                Some(b) if !self.is_a(b, "levelinfo") && !self.is_mover(b) => self
                    .hit_zones
                    .as_ref()
                    .and_then(|z| z.ray_bone(b, start, end))
                    .unwrap_or_else(|| {
                        let (center, radius, half_height) = self.actor_cylinder(b);
                        match &self.hit_zones {
                            Some(z) => z.bone_at(center, radius, half_height, out.1),
                            None => crate::physics::CylinderZones.bone_at(
                                center,
                                radius,
                                half_height,
                                out.1,
                            ),
                        }
                    }),
                _ => "None".to_owned(),
            };
        }
        if self.collect_combat_natives {
            let player = self.objects.iter().enumerate().find_map(|(i, o)| {
                (o.is_actor && !o.deleted && self.is_a(i as ObjectId, "XIIIPlayerPawn"))
                    .then_some(i as ObjectId)
            });
            let cylinder = player.map(|p| {
                let (center, radius, height) = self.actor_cylinder(p);
                (
                    center,
                    radius,
                    height,
                    segment_cylinder_hit(start, end, center, radius, height).map(|h| h.0),
                )
            });
            let posed_bone = player.and_then(|p| {
                self.hit_zones
                    .as_ref()
                    .and_then(|z| z.ray_bone(p, start, end))
            });
            self.note(TraceKind::Note(format!(
                "combat-ray this={} start={start:?} end={end:?} flags={flags:#x} hit={} location={:?} bone={} player_cylinder={cylinder:?} player_posed_bone={posed_bone:?}",
                self.objects[id as usize].name,
                out.0.map_or("None", |b| self.objects[b as usize].name.as_str()),
                out.1,
                self.last_trace_bone
            )));
        }
        Ok(out)
    }

    /// `Actor.FastTrace`: world-only line trace; true when clear. item54: a hit at the segment
    /// endpoint (the traced-to point lies on the surface) does not block — `XIIIPlayerController.
    /// ReturnTrace` 0x0000 ends its visibility checks on the picked actor's own surface
    /// (`FastTrace(HitLoc, Start)`), which the retail punch chain passes.
    pub(crate) fn vm_fast_trace(&mut self, start: [f32; 3], end: [f32; 3]) -> VmResult<bool> {
        match self.physics.as_mut() {
            Some(p) => Ok(p
                .trace(start, end, [0.0; 3])
                .is_none_or(|hit| hit.time >= 1.0 - 1e-3)),
            None => Err(self.err(VmErrorKind::NoPhysicsProvider {
                native: "Actor.FastTrace".into(),
            })),
        }
    }

    // ------------------------------------------------------------------ navigation / pathing

    /// The navigation graph points, if a provider is installed.
    pub(crate) fn nav_points(&self) -> Option<&[NavPointInfo]> {
        self.navigation.as_deref().map(NavigationData::points)
    }

    /// The navigation graph edges, if a provider is installed.
    pub(crate) fn nav_edges(&self) -> Option<&[NavEdgeInfo]> {
        self.navigation.as_deref().map(NavigationData::edges)
    }

    /// Map instance of navigation point `id`, resolved through the point's script object path.
    pub(crate) fn nav_point_actor(&self, id: u32) -> Option<ObjectId> {
        let p = self.nav_points()?.get(id as usize)?;
        self.find_object(&p.actor)
    }

    /// Nearest navigation point actor to `from` whose collision fits the pawn's size, optionally
    /// restricted to an actor class name. Returns `None` when no provider is installed or no
    /// point survives the fit/class test.
    pub(crate) fn nav_nearest_point_actor(
        &self,
        from: [f32; 3],
        radius: f32,
        height: f32,
        class: Option<&str>,
    ) -> Option<ObjectId> {
        let points = self.nav_points()?;
        let mut order: Vec<(f32, u32)> = points
            .iter()
            .enumerate()
            .filter(|(_, p)| point_fits(p, radius, height))
            .map(|(i, p)| (horizontal_distance(from, p.location), i as u32))
            .collect();
        order.sort_by(|a, b| a.0.total_cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
        for (_, i) in order {
            if let Some(a) = self.nav_point_actor(i)
                && class.is_none_or(|c| self.is_a(a, c))
            {
                return Some(a);
            }
        }
        None
    }

    /// Half-extents of `pawn` (its collision cylinder) used for edge/arrival tests.
    fn nav_pawn_size(&self, pawn: ObjectId) -> (f32, f32) {
        (
            self.f32_prop(pawn, "CollisionRadius"),
            self.f32_prop(pawn, "CollisionHeight"),
        )
    }

    /// Shortest path from `from` to `to` over the decoded graph for a pawn of the given size.
    /// The pawn is never treated as a player (`R_PLAYERONLY` edges are skipped).
    pub(crate) fn nav_path_between(
        &self,
        from: [f32; 3],
        to: [f32; 3],
        radius: f32,
        height: f32,
    ) -> Option<Vec<u32>> {
        let points = self.nav_points()?;
        let edges = self.nav_edges()?;
        let start = nearest_point(points, from)?;
        let goal = nearest_point(points, to)?;
        find_path(points, edges, start, goal, radius, height, false)
    }

    /// Writes a path (as actor ids) into the controller's `RouteCache` array, clearing the rest
    /// to `None` exactly like a fixed script array. Returns the first path actor.
    pub(crate) fn nav_set_route(
        &mut self,
        controller: ObjectId,
        path: &[ObjectId],
    ) -> Option<ObjectId> {
        let dim = self.objects[controller as usize]
            .layout
            .slot_by_name("RouteCache")
            .map_or(0, |s| s.dim);
        for i in 0..dim {
            let v = path.get(i).map_or(Value::Object(None), |a| {
                Value::Object(Some(ObjRef::Instance(*a)))
            });
            self.set_property(controller, "RouteCache", i, v);
        }
        path.first().copied()
    }

    /// Shared body of `Controller.FindPathToward` / `FindPathTo`: nearest start from the pawn and
    /// nearest goal to `goal_location`, search, write `RouteCache` and `RouteDist`, and return the
    /// first path actor (the goal's own actor when the goal is the start node).
    pub(crate) fn nav_find_path_to(
        &mut self,
        controller: ObjectId,
        goal_location: [f32; 3],
    ) -> VmResult<Option<ObjectId>> {
        if !self.navigation_ready(
            "Controller.FindPathToward",
            None,
            controller,
            Value::Object(None),
        )? {
            return Ok(None);
        }
        let pawn = self.obj_prop(controller, "Pawn");
        let (from, radius, height) = match pawn {
            Some(p) => {
                let (r, h) = self.nav_pawn_size(p);
                (self.vector_prop(p, "Location").unwrap_or([0.0; 3]), r, h)
            }
            None => ([0.0; 3], 0.0, 0.0),
        };
        let path = self.nav_path_between(from, goal_location, radius, height);
        let (actors, dist) = match path {
            Some(points) => {
                let mut ids = Vec::with_capacity(points.len());
                let mut dist = 0.0f32;
                let mut prev: Option<[f32; 3]> = None;
                for id in &points {
                    if let Some(p) = self.nav_points().and_then(|p| p.get(*id as usize)) {
                        if let Some(q) = prev {
                            dist += horizontal_distance(q, p.location);
                        }
                        prev = Some(p.location);
                    }
                    if let Some(a) = self.nav_point_actor(*id) {
                        ids.push(a);
                    }
                }
                // RouteCache holds the path *after* the node the pawn is standing on.
                let cache = if ids.len() > 1 {
                    ids[1..].to_vec()
                } else {
                    Vec::new()
                };
                (cache, dist)
            }
            None => (Vec::new(), 0.0),
        };
        let first = self.nav_set_route(controller, &actors);
        self.set_property(controller, "RouteDist", 0, Value::Float(dist));
        Ok(first)
    }

    /// `Controller.FindRandomDest`: a deterministic pseudo-random navigation point actor.
    pub(crate) fn nav_find_random_dest(
        &mut self,
        controller: ObjectId,
    ) -> VmResult<Option<ObjectId>> {
        if !self.navigation_ready(
            "Controller.FindRandomDest",
            None,
            controller,
            Value::Object(None),
        )? {
            return Ok(None);
        }
        let n = self.nav_points().map_or(0, |p| p.len()) as u64;
        if n == 0 {
            return Ok(None);
        }
        let idx = (self.next_random() % n) as usize;
        Ok(self.nav_point_actor(idx as u32))
    }

    /// `Controller.LineOfSightTo` (Engine.dll `?LineOfSightTo@AController@@QAEKPAVAActor@@H@Z`
    /// VA 0x1036ac70, item27m): the view location comes from `GetViewTarget` (a PlayerController's
    /// `ViewTarget`, otherwise the pawn or the controller itself), raised by the view actor's
    /// `BaseEyeHeight` only when the view target is this controller's own pawn. The engine traces
    /// more than one line: first to `Other->Location` (the base), and only when that is blocked
    /// tries the eye point (`+BaseEyeHeight`) when `Other` is the controller's `Enemy`, or
    /// `Location.Z + 0.8*CollisionHeight` otherwise — the latter two guarded by distance limits
    /// (>= 8000^2 and >= 2000^2 reject; the 2000^2 exception `IsA(APawn::StaticClass())` never
    /// holds for a controller). Blocked-by-the-target counts as visible upstream, which the
    /// world-geometry-only provider cannot express (no actor occlusion, the standing convention);
    /// the `int` flag argument is always 0 from script (`execLineOfSightTo` literal,
    /// `execCanSee`->`SeePawn`), so its extra rejection is not reachable. No `bHidden` check:
    /// XIII's binary does not have the upstream one. The no-provider case fails explicitly.
    pub(crate) fn nav_line_of_sight_to(
        &mut self,
        controller: ObjectId,
        other: ObjectId,
    ) -> VmResult<bool> {
        if !self.physics_ready(
            "Controller.LineOfSightTo",
            Some(514),
            controller,
            Value::Bool(false),
        )? {
            return Ok(false);
        }
        let view = self.los_view_location(controller);
        let Some(target) = self.vector_prop(other, "Location") else {
            return Ok(false);
        };
        if self.world_line(view, target).is_none() {
            return Ok(true);
        }
        let enemy = self.obj_prop(controller, "Enemy");
        let top = if enemy == Some(other) {
            // Enemy: no distance limits, retry at the target's eye point.
            Some(self.f32_prop(other, "BaseEyeHeight"))
        } else {
            let d = [
                target[0] - view[0],
                target[1] - view[1],
                target[2] - view[2],
            ];
            let dist_sq = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
            // 8000^2 and 2000^2 reject (float constants at 0x1047c878 / 0x1047c874).
            if dist_sq >= 6_400_000.0 || dist_sq >= 4_000_000.0 {
                return Ok(false);
            }
            Some(0.8 * self.f32_prop(other, "CollisionHeight"))
        };
        let Some(raise) = top else {
            return Ok(false);
        };
        let raised = [target[0], target[1], target[2] + raise];
        Ok(self.world_line(view, raised).is_none())
    }

    /// `GetViewTarget` result and its trace start point: a PlayerController uses its `ViewTarget`
    /// (Engine.dll 0x10368040), a plain controller its pawn or itself (0x10368030); the pawn's
    /// `BaseEyeHeight` is added only when the view target is the controller's own pawn.
    fn los_view_location(&self, controller: ObjectId) -> [f32; 3] {
        let view = if self.is_a(controller, "playercontroller") {
            self.obj_prop(controller, "ViewTarget")
        } else {
            None
        }
        .or_else(|| self.obj_prop(controller, "Pawn"))
        .unwrap_or(controller);
        let mut loc = self.vector_prop(view, "Location").unwrap_or([0.0; 3]);
        if Some(view) == self.obj_prop(controller, "Pawn") {
            loc[2] += self.f32_prop(view, "BaseEyeHeight");
        }
        loc
    }

    /// `Controller.CanSee(Pawn Other)` — Engine.dll `AController::SeePawn`
    /// (`?SeePawn@AController@@QAEKPAVAPawn@@H@Z` VA 0x1036dc40, item27m). `execCanSee`
    /// (0x1036f070) calls this with the second argument 0 — XIII's `CanSee` is not a plain
    /// `LineOfSightTo` forward: for the controller's `Enemy` it is exactly `LineOfSightTo`,
    /// otherwise it adds the retail range gate
    /// `DistSq <= (min(1.0, Other.Visibility/128.0) * Pawn.SightRadius)^2` (strictly greater
    /// rejects; `Visibility` byte at Pawn+0x227, `SightRadius` at Pawn+0x238) and the decoded
    /// `|delta| > Pawn.PeripheralVision` check (the binary computes `dot(delta,
    /// SafeNormal(delta))` and compares with Pawn+0x23c; with the Engine.u default
    /// `PeripheralVision = 0` this degenerates to a not-exactly-overlapping guard, and XIII
    /// pawns store raw degrees or -1 here). The no-provider case fails explicitly.
    pub(crate) fn nav_can_see(&mut self, controller: ObjectId, other: ObjectId) -> VmResult<bool> {
        let Some(pawn) = self.obj_prop(controller, "Pawn") else {
            return Ok(false);
        };
        if self.obj_prop(controller, "Enemy") == Some(other) {
            return self.nav_line_of_sight_to(controller, other);
        }
        let Some(pawn_loc) = self.vector_prop(pawn, "Location") else {
            return Ok(false);
        };
        let Some(other_loc) = self.vector_prop(other, "Location") else {
            return Ok(false);
        };
        let d = [
            other_loc[0] - pawn_loc[0],
            other_loc[1] - pawn_loc[1],
            other_loc[2] - pawn_loc[2],
        ];
        let dist_sq = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
        // Range gate: min(1.0, Visibility/128) * SightRadius (float 1.0 @0x1046da0c, float
        // 0.0078125 @0x104794b0). Visibility is a byte property; 128 is the Engine.u Pawn
        // default (the fallback for synthetic objects without the property). SightRadius
        // defaults to 5000 (Engine.u Pawn defaults).
        let visibility = match self.get_property(other, "Visibility") {
            Some(Value::Byte(b)) => f32::from(*b),
            Some(Value::Int(i)) => *i as f32,
            _ => 128.0,
        } * 0.0078125;
        let scale = visibility.min(1.0);
        let sight = {
            let s = self.f32_prop(pawn, "SightRadius");
            if s > 0.0 { s } else { 5000.0 }
        };
        let range = scale * sight;
        if dist_sq > range * range {
            return Ok(false);
        }
        // dot(delta, SafeNormal(delta)) = |delta| must be strictly greater than PeripheralVision
        // (Engine.u default 0; XIII pawns use raw degrees or -1).
        let peripheral = self.f32_prop(pawn, "PeripheralVision");
        if dist_sq.sqrt() <= peripheral {
            return Ok(false);
        }
        self.nav_line_of_sight_to(controller, other)
    }

    /// `Actor.Location + Actor.BaseEyeHeight` (the eye point upstream traces between).
    fn eye_location(&self, id: ObjectId) -> [f32; 3] {
        let l = self.vector_prop(id, "Location").unwrap_or([0.0; 3]);
        let eye = self.f32_prop(id, "BaseEyeHeight");
        [l[0], l[1], l[2] + eye]
    }

    /// Host-driven engine perception (`item14b`): for every live `IAController`, test whether its
    /// pawn can see the player (range, facing cone, clear world line of sight) and dispatch the
    /// engine's own `SeePlayer` / `EnemyNotVisible` events only when visibility changes, as the
    /// engine's sight counter does. This is the native half of perception (the sight test and the
    /// event dispatch); the AI's own states decide what to do with the event. Appends
    /// `(controller, event)` to `out` for the host timeline. Never a silent success: an event call
    /// that raises is reported in `out` as `event: error`.
    pub fn update_ai_perception(&mut self, player: ObjectId, out: &mut Vec<(String, String)>) {
        if self.objects.get(player as usize).is_none_or(|o| o.deleted) {
            return;
        }
        let t0 = self.profile.enabled.then(Instant::now);
        let player_dead = self.bool_prop(player, "bIsDead");
        let controllers: Vec<ObjectId> = self
            .objects
            .iter()
            .enumerate()
            .filter(|(i, o)| {
                o.is_actor && !o.deleted && o.active && self.is_a(*i as ObjectId, "iacontroller")
            })
            .map(|(i, _)| i as ObjectId)
            .collect();
        for ctrl in controllers {
            let Some(pawn) = self.obj_prop(ctrl, "Pawn") else {
                continue;
            };
            if pawn == player || self.bool_prop(pawn, "bIsDead") {
                continue;
            }
            let visible = !player_dead && self.ai_sight(ctrl, pawn, player);
            let was = self.ai_visible.get(&ctrl).copied().unwrap_or(false);
            self.ai_visible.insert(ctrl, visible);
            let name = self.objects[ctrl as usize].name.clone();
            // The engine's sight counter calls `SeePlayer` repeatedly while the player stays
            // visible (the base handler only reacts until `EnemyAcquired` disables the event), and
            // `EnemyNotVisible` once when sight is lost. The host only logs a transition.
            if visible {
                let args = vec![Value::Object(Some(ObjRef::Instance(player)))];
                let res = self.send_event(ctrl, "SeePlayer", args);
                if !was {
                    match res {
                        Ok(_) => out.push((name, "SeePlayer".to_owned())),
                        Err(e) => out.push((name.clone(), format!("SeePlayer: {e}"))),
                    }
                }
            } else if was {
                match self.send_event(ctrl, "EnemyNotVisible", Vec::new()) {
                    Ok(_) => out.push((name, "EnemyNotVisible".to_owned())),
                    Err(e) => out.push((name, format!("EnemyNotVisible: {e}"))),
                }
            }
        }
        if let Some(t0) = t0 {
            self.profile.perception_micros += t0.elapsed().as_micros() as u64;
        }
    }

    /// Sight test for [`Vm::update_ai_perception`]: range (`Pawn.SightRadius`), facing cone
    /// (`Pawn.PeripheralVision`, cos of the half-angle after the pawn's `Init` conversion) and a
    /// clear world trace between the eyes (`Controller.LineOfSightTo`). Actor occlusion is not
    /// modelled, matching the existing `LineOfSightTo` native.
    fn ai_sight(&mut self, ctrl: ObjectId, pawn: ObjectId, player: ObjectId) -> bool {
        let eye = self.eye_location(pawn);
        let target = self.eye_location(player);
        let d = [target[0] - eye[0], target[1] - eye[1], target[2] - eye[2]];
        let dist2 = d[0] * d[0] + d[1] * d[1] + d[2] * d[2];
        let mut sight = self.f32_prop(pawn, "SightRadius");
        if sight <= 0.0 {
            sight = 5000.0;
        }
        if dist2 > sight * sight {
            return false;
        }
        let dot = {
            let rot = self.rotation_prop(pawn).unwrap_or([0; 3]);
            let k = std::f32::consts::TAU / 65536.0;
            let (sp, cp) = ((rot[0] as f32) * k).sin_cos();
            let (sy, cy) = ((rot[1] as f32) * k).sin_cos();
            let f = [cp * cy, cp * sy, sp];
            let n = dist2.sqrt();
            if n > 1e-6 {
                (f[0] * d[0] + f[1] * d[1] + f[2] * d[2]) / n
            } else {
                1.0
            }
        };
        let mut cone = self.f32_prop(pawn, "PeripheralVision");
        if !(-1.0001..=1.0001).contains(&cone) {
            // Raw degrees (Init not run): the scripts convert with cos(deg * 0.00873).
            cone = (cone * 0.00873).cos();
        }
        if cone > 0.9999 {
            // A raw 0 degrees (or an unset value) means "in front", not a full sphere.
            cone = 0.0;
        }
        if dot < cone {
            return false;
        }
        self.nav_line_of_sight_to(ctrl, player).unwrap_or(false)
    }

    /// Retail XIDPawn 0x11903ea0: scan the actual linked NavigationPointList in list order.
    /// Strictly better alignment wins; preserve LastSeenPos when there is no eligible node.
    pub(crate) fn ai_stake_out_dir(&mut self, controller: ObjectId) -> VmResult<()> {
        let Some(enemy) = self.obj_prop(controller, "Enemy") else {
            return Ok(());
        };
        let Some(pawn) = self.obj_prop(controller, "Pawn") else {
            return Err(self.err(VmErrorKind::Other(
                "FindNewStakeOutDir requires Pawn when Enemy is set".into(),
            )));
        };
        let loc = self.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let enemy_dir = normalize3(sub3(
            self.vector_prop(enemy, "Location").unwrap_or(loc),
            loc,
        ));
        let mut node = self
            .obj_prop(controller, "Level")
            .or_else(|| self.find_level_info())
            .and_then(|level| self.obj_prop(level, "NavigationPointList"));
        let mut seen = HashSet::new();
        let mut best_dot = -1.0;
        let mut best = None;
        while let Some(id) = node {
            if !seen.insert(id) {
                return Err(self.err(VmErrorKind::Other(
                    "FindNewStakeOutDir: cycle in NavigationPointList".into(),
                )));
            }
            let delta = sub3(self.vector_prop(id, "Location").unwrap_or(loc), loc);
            let distance = dot3(delta, delta).sqrt();
            if distance > 100.0 && distance < 800.0 {
                let alignment = dot3(enemy_dir, scale3(delta, distance.recip()));
                if alignment > best_dot && self.nav_line_of_sight_to(controller, id)? {
                    best_dot = alignment;
                    best = Some(id);
                }
            }
            node = self.obj_prop(id, "NextNavigationPoint");
        }
        if let Some(best) = best {
            let mut focal = self.vector_prop(best, "Location").unwrap_or(loc);
            focal[2] += 0.5 * self.f32_prop(pawn, "CollisionHeight");
            self.set_property(controller, "LastSeenPos", 0, Value::Vector(focal));
        }
        Ok(())
    }

    /// XIDPawn 0x11903230. The x87 equality gate at 0x119033a6 etc. really
    /// rejects unequal or unordered components: ordinary nonzero separation returns zero. Do not
    /// replace this surprising retail behavior with a conventional steering algorithm.
    pub(crate) fn ai_pseudo_steering(&mut self, controller: ObjectId) -> VmResult<[f32; 3]> {
        let Some(group) = self.obj_prop(controller, "GenAlerte") else {
            return Ok([0.0; 3]);
        };
        let Some(pawn) = self.obj_prop(controller, "Pawn") else {
            return Err(self.err(VmErrorKind::Other(
                "PseudoSteering requires Pawn when GenAlerte is set".into(),
            )));
        };
        let Some(Value::Array(members)) = self.get_property(group, "SoldierInFightList").cloned()
        else {
            return Err(self.err(VmErrorKind::Other(
                "PseudoSteering requires SoldierInFightList".into(),
            )));
        };
        let loc = self.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let mut sum = [0.0; 3];
        let mut last = None;
        for member in members {
            let Value::Object(Some(ObjRef::Instance(id))) = member else {
                return Err(self.err(VmErrorKind::Other(
                    "PseudoSteering: null/non-instance fight-list entry".into(),
                )));
            };
            last = Some(id);
            if id != pawn {
                let delta = sub3(loc, self.vector_prop(id, "Location").unwrap_or(loc));
                sum = add3(sum, scale3(delta, dot3(delta, delta).recip()));
            }
        }
        if sum.iter().any(|component| *component != 0.0) {
            return Ok([0.0; 3]);
        }
        let Some(last) = last else {
            return Err(self.err(VmErrorKind::Other(
                "PseudoSteering: empty fight list reaches a null dereference in retail".into(),
            )));
        };
        let last_loc = self.vector_prop(last, "Location").unwrap_or(loc);
        let mut direction = normalize3(sum);
        let mut start = loc;
        start[2] -= 30.0;
        let mut end = add3(last_loc, scale3(direction, 4000.0));
        end[2] -= 30.0;
        // TRACE_AllBlocking (0x86); world/actor geometry comes through the existing provider.
        let Some(provider) = self.physics.as_mut() else {
            return Err(self.err(VmErrorKind::NoPhysicsProvider {
                native: "IAController.PseudoSteering".into(),
            }));
        };
        let mut hit = provider.trace(start, end, [0.0; 3]);
        if let Some((time, _, normal)) = self.trace_actors(controller, start, end, [0.0; 3])
            && hit.is_none_or(|world| time <= world.time)
        {
            hit = Some(crate::physics::WorldHit {
                time,
                normal,
                location: lerp3(start, end, time),
            });
        }
        if let Some(hit) = hit {
            let denominator = dot3(sub3(loc, last_loc), sub3(loc, last_loc));
            sum = add3(sum, scale3(sub3(loc, hit.location), denominator.recip()));
            direction = normalize3(sum);
        }
        sum = scale3(sum, 50000.0);
        let size_sq = dot3(sum, sum);
        Ok(if size_sq > 16000000.0 {
            scale3(direction, 4000.0)
        } else if size_sq > 2500.0 {
            sum
        } else {
            [0.0; 3]
        })
    }

    /// XIDPawn.dll DirectionDuTir's point contract, firing origin and cone sampling.
    /// Projectile/base-velocity lead branches remain explicitly Partial.
    pub(crate) fn ai_aim_point(&mut self, controller: ObjectId) -> VmResult<[f32; 3]> {
        let Some(soldier) = self.obj_prop(controller, "BaseS") else {
            return Ok([0.0; 3]);
        };
        let target = self
            .vector_prop(controller, "EnemyTargetPos")
            .unwrap_or([0.0; 3]);
        if target == [0.0; 3] {
            return Ok([0.0; 3]);
        }
        let Some(pawn) = self.obj_prop(controller, "Pawn") else {
            return Err(self.err(VmErrorKind::Other("DirectionDuTir requires Pawn".into())));
        };
        let Some(weapon) = self.obj_prop(pawn, "Weapon") else {
            return Err(self.err(VmErrorKind::Other(
                "DirectionDuTir requires Pawn.Weapon".into(),
            )));
        };
        let Some(enemy) = self.obj_prop(controller, "Enemy") else {
            return Err(self.err(VmErrorKind::Other("DirectionDuTir requires Enemy".into())));
        };
        let loc = self.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let rot = self.rotation_prop(controller).unwrap_or([0; 3]);
        let (x, y, z) = crate::registry::rotator_basis(rot);
        let offset = self.vector_prop(weapon, "FireOffset").unwrap_or([0.0; 3]);
        let mut start = add3(loc, [0.0, 0.0, self.f32_prop(pawn, "BaseEyeHeight")]);
        start = add3(
            start,
            add3(
                scale3(x, offset[0]),
                add3(scale3(y, offset[1]), scale3(z, offset[2])),
            ),
        );
        let ammo = self.obj_prop(weapon, "AmmoType");
        let instant = ammo.is_some_and(|a| self.bool_prop(a, "bInstantHit"));
        let hand = match self.get_property(weapon, "WHand") {
            Some(Value::Byte(v)) => *v,
            _ => 0,
        };
        if instant && hand != 0 && hand != 4 {
            start = add3(start, scale3(x, 16.0));
        }
        self.set_property(controller, "WeaponStartTrace", 0, Value::Vector(start));
        let skill = match self.get_property(soldier, "Skill") {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        if skill == 5 && self.rand_float() > 0.5 && instant {
            let point = self.vector_prop(enemy, "Location").unwrap_or(target);
            self.set_property(controller, "DirectionTir", 0, Value::Vector(point));
            return Ok(point);
        }
        let delta = sub3(self.vector_prop(enemy, "Location").unwrap_or(target), loc);
        let distance = dot3(delta, delta).sqrt();
        // 0x11901d60: three FRand draws, reject outside the unit sphere, normalize.
        let mut random = None;
        for _ in 0..1024 {
            let sample = [
                2.0 * self.rand_float() - 1.0,
                2.0 * self.rand_float() - 1.0,
                2.0 * self.rand_float() - 1.0,
            ];
            if dot3(sample, sample) <= 1.0 {
                random = Some(normalize3(sample));
                break;
            }
        }
        let Some(random) = random else {
            return Err(self.err(VmErrorKind::Other(
                "DirectionDuTir random-vector rejection budget exhausted".into(),
            )));
        };
        let forward = normalize3(x);
        let perpendicular = normalize3([
            forward[1] * random[2] - forward[2] * random[1],
            forward[2] * random[0] - forward[0] * random[2],
            forward[0] * random[1] - forward[1] * random[0],
        ]);
        let angle = self.f32_prop(controller, "Angle_Visee")
            * if self.bool_prop(controller, "bTirSurConeMax") {
                1.0
            } else {
                self.rand_float()
            };
        let mut point = add3(
            target,
            scale3(perpendicular, distance * angle.to_radians().tan()),
        );
        point[2] += match skill {
            1 => -35.0,
            2 | 3 => -25.0,
            _ => 23.62,
        };
        // +0x4a0 is Temps_RefreshEnemyPos (elapsed target sampling lead).
        point = add3(
            point,
            scale3(
                self.vector_prop(controller, "EnemyTargetVelocity")
                    .unwrap_or([0.0; 3]),
                self.f32_prop(controller, "Temps_RefreshEnemyPos"),
            ),
        );
        if self.bool_prop(enemy, "bIsCrouched") {
            point[2] -=
                self.f32_prop(pawn, "CollisionHeight") - self.f32_prop(pawn, "CrouchHeight");
        }
        self.set_property(controller, "DirectionTir", 0, Value::Vector(point));
        Ok(point)
    }

    /// First actor on WeaponStartTrace -> DirectionTir, with XIII shooting-through flags.
    /// A world hit terminates the line but classifies as zero; no hit-zone state is modified.
    pub(crate) fn ai_fire_obstacle(&mut self, controller: ObjectId) -> VmResult<Option<ObjectId>> {
        let Some(pawn_id) = self.obj_prop(controller, "Pawn") else {
            return Err(self.err(VmErrorKind::Other(
                "LineOfFireObstacle requires Pawn.Weapon.AmmoType".into(),
            )));
        };
        let Some(ammo) = self
            .obj_prop(pawn_id, "Weapon")
            .and_then(|id| self.obj_prop(id, "AmmoType"))
        else {
            return Err(self.err(VmErrorKind::Other(
                "LineOfFireObstacle requires Pawn.Weapon.AmmoType".into(),
            )));
        };
        let pawn = Some(pawn_id);
        let instant = self.bool_prop(ammo, "bInstantHit");
        let through = if instant {
            "bCanShootThroughWithRayCastingWeapon"
        } else {
            "bCanShootThroughWithProjectileWeapon"
        };
        let start = self
            .vector_prop(controller, "WeaponStartTrace")
            .unwrap_or([0.0; 3]);
        let end = self
            .vector_prop(controller, "DirectionTir")
            .unwrap_or(start);
        let Some(provider) = self.physics.as_mut() else {
            return Err(self.err(VmErrorKind::NoPhysicsProvider {
                native: "IAController.LineOfFireObstacle".into(),
            }));
        };
        let world = provider.trace(start, end, [0.0; 3]);
        let mut best = world.map_or(1.0, |hit| hit.time);
        let mut actor = None;
        for id in 0..self.objects.len() as ObjectId {
            if id == controller
                || !self.is_live_actor(id)
                || self.bool_prop(id, through)
                || (!self.bool_prop(id, "bCollideActors")
                    && !self.bool_prop(id, "bBlockZeroExtentTraces"))
                || self.is_owned_by(id, controller)
                || self.is_owned_by(controller, id)
            {
                continue;
            }
            let (loc, radius, height) = self.actor_cylinder(id);
            if let Some((time, _)) = segment_cylinder_hit(start, end, loc, radius, height)
                && time <= best
            {
                best = time;
                actor = Some(id);
            }
        }
        if self.collect_combat_natives {
            self.note(TraceKind::Note(format!(
                "combat-ray obstacle={} start={start:?} end={end:?} first={} world_time={:?}",
                self.objects[controller as usize].name,
                actor.map_or("None", |id| self.objects[id as usize].name.as_str()),
                world.map(|h| h.time)
            )));
        }
        Ok(actor.filter(|id| {
            Some(*id) != pawn
                && Some(*id) != self.obj_prop(controller, "Enemy")
                && !self.is_a(*id, "LevelInfo")
        }))
    }

    /// `Controller.pointReachable`: the point is directly reachable (clear pawn trace) and a
    /// navigation neighbourhood exists near it.
    pub(crate) fn nav_point_reachable(
        &mut self,
        controller: ObjectId,
        point: [f32; 3],
    ) -> VmResult<bool> {
        if !self.physics_ready(
            "Controller.pointReachable",
            Some(521),
            controller,
            Value::Bool(false),
        )? {
            return Ok(false);
        }
        let Some(pawn) = self.obj_prop(controller, "Pawn") else {
            return Ok(false);
        };
        let start = self.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        let extent = self.actor_extent(pawn);
        let clear = self
            .physics
            .as_mut()
            .is_some_and(|p| p.trace(start, point, extent).is_none());
        if !clear {
            return Ok(false);
        }
        if self.nav_points().is_some() {
            let (r, h) = self.nav_pawn_size(pawn);
            Ok(self.nav_path_between(start, point, r, h).is_some())
        } else {
            Ok(true)
        }
    }

    /// `Controller.actorReachable`: `pointReachable` on the actor's location.
    pub(crate) fn nav_actor_reachable(
        &mut self,
        controller: ObjectId,
        other: ObjectId,
    ) -> VmResult<bool> {
        let point = self.vector_prop(other, "Location").unwrap_or([0.0; 3]);
        self.nav_point_reachable(controller, point)
    }

    /// Starts a latent `MoveTo`/`MoveToward`. Returns `false` when there is no pawn to move (the
    /// native then returns `void` without suspending, like upstream on a controller without a
    /// pawn). `native` is recorded in the latent for the trace.
    pub(crate) fn start_move(
        &mut self,
        controller: ObjectId,
        destination: [f32; 3],
        speed: f32,
        native: &'static str,
        in_state: bool,
    ) -> VmResult<bool> {
        if !in_state {
            return Err(self.err(VmErrorKind::LatentOutsideState {
                path: native.into(),
            }));
        }
        if !self.physics_ready(native, None, controller, Value::Void)? {
            return Ok(false);
        }
        let pawn = self.obj_prop(controller, "Pawn");
        let Some(pawn) = pawn else {
            return Ok(false);
        };
        let loc = self.vector_prop(pawn, "Location").unwrap_or([0.0; 3]);
        // Engine.dll 0x1036a758: Speed is a fraction, default 1, not UU/s.
        let walking_pct = self.f32_prop(pawn, "WalkingPct");
        let max_desired = self.f32_prop(pawn, "MaxDesiredSpeed");
        if !speed.is_finite()
            || !walking_pct.is_finite()
            || !max_desired.is_finite()
            || (speed <= walking_pct && walking_pct == 0.0)
        {
            return Err(self.err(VmErrorKind::Other(format!(
                "{native}: non-finite speed or zero WalkingPct in walking-speed division"
            ))));
        }
        let walking = speed <= walking_pct;
        let requested = if walking { speed / walking_pct } else { speed };
        let desired = max_desired.max(0.0).min(requested);
        self.set_property(pawn, "bWalking", 0, Value::Bool(walking));
        self.set_property(pawn, "bReducedSpeed", 0, Value::Bool(false));
        self.set_property(pawn, "DesiredSpeed", 0, Value::Float(desired));
        let timer_base_speed = match self.byte_prop(pawn, "Physics") {
            PHYS_WALKING | 2 | 9 => self.f32_prop(pawn, "GroundSpeed"), // falling/spider
            3 => self.f32_prop(pawn, "WaterSpeed"),
            4 => self.f32_prop(pawn, "AirSpeed"),
            _ => 200.0,
        };
        let speed = timer_base_speed * desired;
        let delta = sub3(destination, loc);
        let distance = dot3(delta, delta).sqrt();
        // APawn::setMoveTimer 0x103affb0: zero speed gets 0.5 s, otherwise 1+2*D/S.
        let budget = if native == "Controller.MoveToward"
            && self
                .obj_prop(controller, "MoveTarget")
                .is_some_and(|target| self.is_a(target, "Pawn"))
        {
            1.2
        } else if speed == 0.0 {
            0.5
        } else {
            1.0 + 2.0 * distance / speed
        };
        self.set_property(controller, "Destination", 0, Value::Vector(destination));
        self.set_property(controller, "MoveTimer", 0, Value::Float(budget));
        self.set_property(controller, "bAdjusting", 0, Value::Bool(false));
        let _ = self.controller_move_step(controller, pawn, destination, 0.0)?;
        self.pending_latent = Some(Latent::Move {
            pawn,
            destination,
            speed,
            remaining: budget,
            started: self.time,
            native,
        });
        Ok(true)
    }

    /// Controller movement is acceleration-driven, unlike XIDCine's direct Steering.
    /// Engine moveToward 0x103b3950 -> Acceleration (+0xf0), physWalking 0x103bdac0
    /// -> calcVelocity 0x103ba250 -> swept displacement. Non-walking modes are left
    /// to their physics handler; PHYS_None must not be moved by the latent poll.
    pub(crate) fn controller_move_step(
        &mut self,
        controller: ObjectId,
        pawn: ObjectId,
        destination: [f32; 3],
        dt: f32,
    ) -> VmResult<bool> {
        let loc = self.vector_prop(pawn, "Location").unwrap_or(destination);
        let mut delta = sub3(destination, loc);
        let walking = self.byte_prop(pawn, "Physics") == PHYS_WALKING;
        if walking {
            delta[2] = 0.0;
        }
        let distance = dot3(delta, delta).sqrt();
        let radius = self.f32_prop(pawn, "CollisionRadius");
        // The full ReachedDestination navigation/height rules remain Partial; do not
        // turn a vertically distant target into horizontal arrival.
        let target_radius = self
            .obj_prop(controller, "MoveTarget")
            .map_or(0.0, |target| self.f32_prop(target, "CollisionRadius"));
        if distance <= radius + target_radius
            && (destination[2] - loc[2]).abs() <= self.f32_prop(pawn, "CollisionHeight")
        {
            self.set_property(pawn, "Acceleration", 0, Value::Vector([0.0; 3]));
            return Ok(true);
        }
        let direction = normalize3(delta);
        let mut acceleration = scale3(direction, self.f32_prop(pawn, "AccelRate"));
        self.set_property(pawn, "Acceleration", 0, Value::Vector(acceleration));
        if !walking || dt <= 0.0 {
            return Ok(false);
        }
        // calcVelocity 0x103ba5b4: both bWalking and bIsCrouched use WalkingPct
        // for the acceleration cap (CrouchingPct is a separate velocity cap).
        if self.bool_prop(pawn, "bWalking") || self.bool_prop(pawn, "bIsCrouched") {
            let cap = self.f32_prop(pawn, "AccelRate") * self.f32_prop(pawn, "WalkingPct");
            if dot3(acceleration, acceleration) > cap * cap {
                acceleration = scale3(normalize3(acceleration), cap);
                self.set_property(pawn, "Acceleration", 0, Value::Vector(acceleration));
            }
        }
        let mut velocity = self.vector_prop(pawn, "Velocity").unwrap_or([0.0; 3]);
        velocity[2] = 0.0;
        let speed = dot3(velocity, velocity).sqrt();
        let friction = self
            .obj_prop(pawn, "PhysicsVolume")
            .map_or(0.0, |volume| self.f32_prop(volume, "GroundFriction"));
        // calcVelocity's directional friction uses the old speed; it is not drag.
        velocity = sub3(
            velocity,
            scale3(sub3(velocity, scale3(direction, speed)), dt * friction),
        );
        velocity = add3(velocity, scale3(acceleration, dt));
        let mut limit = self.f32_prop(pawn, "GroundSpeed") * self.f32_prop(pawn, "DesiredSpeed");
        if self.bool_prop(pawn, "bIsCrouched") {
            limit *= self.f32_prop(pawn, "CrouchingPct");
        } else if self.bool_prop(pawn, "bWalking") {
            limit *= self.f32_prop(pawn, "WalkingPct");
        }
        let size_sq = dot3(velocity, velocity);
        if size_sq > limit * limit {
            velocity = scale3(normalize3(velocity), limit);
        }
        let displacement = scale3(velocity, dt);
        let extent = self.actor_extent(pawn);
        let collides_world = self.bool_prop(pawn, "bCollideWorld");
        let end = if collides_world {
            let Some(provider) = self.physics.as_mut() else {
                return Err(self.err(VmErrorKind::NoPhysicsProvider {
                    native: "Controller movement".into(),
                }));
            };
            provider.walk_box(loc, displacement, extent).end
        } else {
            add3(loc, displacement)
        };
        self.set_property(pawn, "Location", 0, Value::Vector(end));
        self.set_property(
            pawn,
            "Velocity",
            0,
            Value::Vector(scale3(sub3(end, loc), dt.recip())),
        );
        Ok(false)
    }

    /// item53: engine physics integration for script-driven actors, run once per tick after the
    /// script Tick phase. Decoded from Engine.dll (local/reports/item53-grapple-hook.md):
    ///
    /// - `PHYS_Projectile` (6), `AActor::physProjectile` 0x103c09a0: `Velocity += Acceleration*dt`
    ///   (no zone gravity; the `bMoveProjectiles` zone-velocity drag only applies to water zones
    ///   and is not modelled), then a swept move with the actor's collision extent. On a world hit
    ///   the `HitWall` event runs and, with `bBounce` clear, the actor continues as
    ///   `PHYS_Falling` (the `cmpb $0x2, 0x38(%esi)` fall-through at 0x103c0c51) — this is what
    ///   makes the demo's `Crochet.Velocity.Z < 1` cast transition fire.
    /// - `PHYS_Falling` (2), `AActor::physFalling` 0x103bfc80: `Velocity +=
    ///   (ZoneGravity*(1 - Buoyancy/Mass) + Acceleration)*dt`, downward speed clamped to the
    ///   zone `TerminalVelocity` (the measured `Engine.PhysicsVolume` default 2500; the VM tracks
    ///   no volumes), then a swept move.
    /// - `PHYS_Flying` (4): a constant-velocity swept move (the demo rewrites `Velocity` every
    ///   tick; a hit stops the actor rather than sliding along the wall).
    /// - `PHYS_Walking` (1) is integrated for a pawn whose controller is in `NoControl` —
    ///   a cinematic owns the pawn and steers it by writing `Velocity`
    ///   (`RoofGrapnleDemonstrator.MoveToRightThePlace`; its arrival test is horizontal distance
    ///   < 1 UU, so the step is horizontal-only) — and (item53b) for a pawn whose controller is
    ///   a `CineController2` with `bMoving` (a `movseq`/`movseqb` steering move owns the pawn's
    ///   walk: `execSteering` writes `Acceleration` and the speed fields, this pass integrates
    ///   them). Every other Walking pawn is moved by its controller latents
    ///   ([`Vm::controller_move_step`]) or the host, which already include the walking step;
    ///   integrating them here too would move them twice.
    ///
    /// Documented approximations (not silent successes): water-zone buoyancy/gravity overrides
    /// are not tracked (`GetNetBuoyancy` decodes to 0.0 outside water; see
    /// [`Vm::integrate_falling`]), actor-vs-actor blocking is not applied to physics moves
    /// (world + registered movers only), `Landed` is not dispatched from this pass (the host
    /// owns the player's landing; a floor-grade contact instead zeroes the velocity and, since
    /// item53b, switches a pawn to `PHYS_Walking` — the engine's `processLanded` fallback), and
    /// the projectile hit-fall-through applies gravity for the frame without the engine's second
    /// sub-frame move.
    fn advance_scripted_physics(&mut self, dt: f32) -> Vec<(ObjectId, VmError)> {
        let mut errors = Vec::new();
        if dt <= 0.0 {
            return errors;
        }
        for id in 0..self.objects.len() as ObjectId {
            if !self.objects[id as usize].active
                || !self.objects[id as usize].is_actor
                || self.objects[id as usize].deleted
            {
                continue;
            }
            let result = match self.byte_prop(id, "Physics") {
                PHYS_WALKING => {
                    if self.pawn_script_owned(id) {
                        self.integrate_walking(id, dt)
                    } else if self.cine_steering_owned(id) {
                        self.integrate_steered_walking(id, dt)
                    } else {
                        Ok(())
                    }
                }
                PHYS_FALLING => self.integrate_falling(id, dt),
                PHYS_FLYING => self.integrate_linear(id, dt),
                PHYS_PROJECTILE => self.integrate_projectile(id, dt),
                _ => Ok(()),
            };
            if let Err(e) = result {
                let suspended = self.suspend_for_error(id, &e);
                errors.push((suspended, e));
            }
        }
        errors
    }

    /// The pawn's controller is live and in `NoControl`: a cinematic owns the pawn's movement
    /// and look direction (the session's `script_owns_player_pawn` mirrors this for the player
    /// pawn, so the host stops writing the pawn fields while this holds).
    fn pawn_script_owned(&self, pawn: ObjectId) -> bool {
        self.obj_prop(pawn, "Controller")
            .is_some_and(|ctrl| self.is_live_actor(ctrl) && self.is_in_state(ctrl, "NoControl"))
    }

    /// item53b: the pawn's controller is a live `CineController2` with `bMoving` — a cine
    /// steering move (`movseq`/`movseqb`) is driving the pawn, and `execSteering` (XIDCine.dll
    /// 0x100021a0) has just written its `Acceleration`/speed fields for the engine's
    /// `physWalking` to integrate.
    fn cine_steering_owned(&self, pawn: ObjectId) -> bool {
        self.obj_prop(pawn, "Controller").is_some_and(|ctrl| {
            self.is_live_actor(ctrl)
                && self.is_a(ctrl, "CineController2")
                && self.bool_prop(ctrl, "bMoving")
        })
    }

    /// `PHYS_Walking` for a script-owned pawn: one horizontal step by the script-written
    /// `Velocity` (the engine's gravity/floor-follow stays with the authoring; the demo walks a
    /// flat roof and its arrival test ignores z).
    fn integrate_walking(&mut self, pawn: ObjectId, dt: f32) -> VmResult<()> {
        let velocity = self.vector_prop(pawn, "Velocity").unwrap_or([0.0; 3]);
        let delta = [velocity[0] * dt, velocity[1] * dt, 0.0];
        if delta == [0.0; 3] {
            return Ok(());
        }
        let Some(location) = self.vector_prop(pawn, "Location") else {
            return Ok(());
        };
        let extent = self.actor_extent(pawn);
        let end = if self.bool_prop(pawn, "bCollideWorld") {
            match self.physics.as_mut() {
                Some(provider) => provider.walk_box(location, delta, extent).end,
                None => add3(location, delta),
            }
        } else {
            add3(location, delta)
        };
        self.set_property(pawn, "Location", 0, Value::Vector(end));
        Ok(())
    }

    /// `PHYS_Falling`: gravity (+ `Acceleration`) integrated into `Velocity`, then one swept
    /// move ([`Vm::integrate_move`]).
    ///
    /// The engine scales the gravity by `1 - GetNetBuoyancy/Mass`
    /// (`AActor::physFalling` 0x103bfe99-0x103bfeb8), but `GetNetBuoyancy` (0x103bbf50) does
    /// NOT read the `Buoyancy` property outside water zones: for a non-water volume it walks the
    /// actor's attached-actor array (offset 0xb4) and returns `(1.0 - accumulated) * k`, i.e. 0.0
    /// for an actor without attachments — so the full zone gravity applies (a XIII corpse with
    /// the class-default `Buoyancy=99, Mass=100` still falls at -950). The water-zone branch and
    /// attached-actor contributions are not modelled (Partial, documented): the VM has no water
    /// zones, so the plain zone gravity stands.
    fn integrate_falling(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        let gravity = self.zone_gravity(id);
        let acceleration = self.vector_prop(id, "Acceleration").unwrap_or([0.0; 3]);
        let mut velocity = self.vector_prop(id, "Velocity").unwrap_or([0.0; 3]);
        for axis in 0..3 {
            velocity[axis] += (gravity[axis] + acceleration[axis]) * dt;
        }
        // `Engine.PhysicsVolume` default `TerminalVelocity` 2500 (measured via
        // `xiii-tool script defaults`); the VM tracks no volumes, so the default stands.
        const TERMINAL_VELOCITY: f32 = 2500.0;
        if velocity[2] < -TERMINAL_VELOCITY {
            velocity[2] = -TERMINAL_VELOCITY;
        }
        self.set_property(id, "Velocity", 0, Value::Vector(velocity));
        let hit = self.integrate_move_sliding(id, velocity, dt, true)?;
        // APawn::processLanded (Engine.dll 0x103c1580): after the `Landed` events, a pawn whose
        // `Physics` is still PHYS_Falling (2, checked at 0x103c188d) is switched to
        // PHYS_Walking (1) by the engine (the `pushl $1` before the virtual `setPhysics` at
        // 0x103c18cb). Without this the scripted pawn rests Falling forever and a cine steering
        // move never reaches its walking phase (measured on Toits01 Cine0, probe24c).
        if let Some(hit) = hit
            && hit.normal[2] > 0.7
            && self.is_a(id, "Pawn")
            && !self.objects[id as usize].deleted
            && self.byte_prop(id, "Physics") == PHYS_FALLING
        {
            self.set_property(id, "Physics", 0, Value::Byte(PHYS_WALKING));
        }
        Ok(())
    }

    /// item53b: `PHYS_Falling`'s move as per-axis sub-moves (x, then y, then z). The engine's
    /// pawn move slides along the surfaces it is pressed against; a single combined sweep
    /// instead stops the whole delta when one axis is blocked (measured: the Toits01 cine pawn
    /// flush against the Model71 wall with a steering Acceleration into it stopped falling
    /// entirely, probe27). Each blocked axis cancels its own velocity component into the
    /// surface; the first blocking hit is returned (and, with `dispatch`, runs `HitWall`).
    fn integrate_move_sliding(
        &mut self,
        id: ObjectId,
        mut velocity: [f32; 3],
        dt: f32,
        dispatch: bool,
    ) -> VmResult<Option<crate::physics::WorldHit>> {
        let Some(mut location) = self.vector_prop(id, "Location") else {
            return Ok(None);
        };
        let delta = scale3(velocity, dt);
        if delta == [0.0; 3] {
            return Ok(None);
        }
        if !self.bool_prop(id, "bCollideWorld") {
            self.set_property(id, "Location", 0, Value::Vector(add3(location, delta)));
            return Ok(None);
        }
        let extent = self.actor_extent(id);
        let mut first_hit: Option<crate::physics::WorldHit> = None;
        for axis in 0..3 {
            if delta[axis] == 0.0 {
                continue;
            }
            let mut step = [0.0_f32; 3];
            step[axis] = delta[axis];
            let end = add3(location, step);
            let hit = match self.physics.as_mut() {
                Some(provider) => {
                    let (hit, _) = provider.trace_with_mover(location, end, extent);
                    hit
                }
                None => None,
            };
            match hit {
                Some(h) => {
                    location = h.location;
                    // Cancel this axis's velocity component when it points into the surface.
                    if velocity[axis] * h.normal[axis] < 0.0 {
                        velocity[axis] = 0.0;
                    }
                    if first_hit.is_none() {
                        first_hit = Some(h);
                    }
                }
                None => location = end,
            }
        }
        self.set_property(id, "Location", 0, Value::Vector(location));
        self.set_property(id, "Velocity", 0, Value::Vector(velocity));
        if dispatch && let Some(hit) = &first_hit {
            self.send_event(
                id,
                "HitWall",
                vec![
                    Value::Vector(hit.normal),
                    Value::Object(Some(ObjRef::Instance(id))),
                ],
            )?;
        }
        Ok(first_hit)
    }

    /// item53b: `PHYS_Walking` for a pawn steered by a `CineController2` move sequence
    /// (`movseq`/`movseqb`). The engine's `APawn::physWalking` integrates the `Acceleration`
    /// that `execSteering` wrote: the velocity accelerates toward `Normal(Acceleration) *
    /// GroundSpeed` (`calcVelocity`, Engine.dll 0x103ba250, with the pawn's `AccelRate` — the
    /// measured `Engine.Pawn` default 2048), then one collision move with floor follow
    /// ([`WorldPhysics::walk_box`]) moves the pawn. The obstacle slide this produces is what
    /// lets a steered pawn walk around a wall and cross the arrival plane behind it.
    ///
    /// Documented approximation: the engine's per-tick ground friction and the exact
    /// `calcVelocity` friction iterations are reduced to the accelerate-then-cap form here (a
    /// steered cine pawn's velocity starts at zero after its landing, so the cap is the
    /// operative rule), and a steering tick without acceleration parks the velocity (the engine
    /// decays it).
    fn integrate_steered_walking(&mut self, pawn: ObjectId, dt: f32) -> VmResult<()> {
        let Some(location) = self.vector_prop(pawn, "Location") else {
            return Ok(());
        };
        let acceleration = self.vector_prop(pawn, "Acceleration").unwrap_or([0.0; 3]);
        let speed = self.f32_prop(pawn, "GroundSpeed");
        let mut velocity = self.vector_prop(pawn, "Velocity").unwrap_or([0.0; 3]);
        let length = (acceleration[0] * acceleration[0] + acceleration[1] * acceleration[1]).sqrt();
        if length > 0.0 && speed > 0.0 {
            let dir = [acceleration[0] / length, acceleration[1] / length, 0.0];
            let accel_rate = {
                let rate = self.f32_prop(pawn, "AccelRate");
                if rate > 0.0 { rate } else { 2048.0 }
            };
            for axis in 0..2 {
                velocity[axis] += dir[axis] * accel_rate * dt;
            }
            let vlen = (velocity[0] * velocity[0] + velocity[1] * velocity[1]).sqrt();
            if vlen > speed {
                velocity = [dir[0] * speed, dir[1] * speed, 0.0];
            }
        } else {
            velocity = [0.0; 3];
        }
        self.set_property(pawn, "Velocity", 0, Value::Vector(velocity));
        if velocity == [0.0; 3] || !self.bool_prop(pawn, "bCollideWorld") {
            return Ok(());
        }
        let delta = [velocity[0] * dt, velocity[1] * dt, 0.0];
        if delta == [0.0; 3] {
            return Ok(());
        }
        let extent = self.actor_extent(pawn);
        let walked = self
            .physics
            .as_mut()
            .map(|provider| provider.walk_box(location, delta, extent));
        let end = walked.map_or_else(|| add3(location, delta), |o| o.end);
        // TEMPORARY item53d diagnostic (removed before finishing): with XIII_VM_MOVE_TRACE set,
        // print each steered move's start/end/velocity and the blocking hit + overlapping
        // primitives at the blocked position.
        if std::env::var_os("XIII_VM_MOVE_TRACE").is_some() {
            let name = self
                .objects
                .get(pawn as usize)
                .map(|o| o.name.clone())
                .unwrap_or_default();
            let hit_info = walked
                .as_ref()
                .and_then(|o| o.hit.as_ref())
                .map(|h| {
                    format!(
                        "HIT t={:.3} at ({:.1},{:.1},{:.1}) n=({:.2},{:.2},{:.2})",
                        h.time,
                        h.location[0],
                        h.location[1],
                        h.location[2],
                        h.normal[0],
                        h.normal[1],
                        h.normal[2]
                    )
                })
                .unwrap_or_else(|| "free".to_owned());
            let overlaps = if walked.as_ref().is_some_and(|o| o.hit.is_some()) {
                self.physics
                    .as_mut()
                    .map(|p| {
                        p.dump_overlap(end, extent)
                            .iter()
                            .take(4)
                            .map(|r| format!("{}:{}", r.kind, r.source))
                            .collect::<Vec<_>>()
                            .join(", ")
                    })
                    .unwrap_or_default()
            } else {
                String::new()
            };
            println!(
                "[move-trace] {name} t={:.3} start=({:.1},{:.1},{:.1}) end=({:.1},{:.1},{:.1}) vel=({:.1},{:.1},{:.1}) {hit_info} overlaps[{overlaps}]",
                self.time,
                location[0],
                location[1],
                location[2],
                end[0],
                end[1],
                end[2],
                velocity[0],
                velocity[1],
                velocity[2],
            );
        }
        self.set_property(pawn, "Location", 0, Value::Vector(end));
        Ok(())
    }

    /// `PHYS_Flying`: one swept move by the script-written `Velocity` (no gravity).
    fn integrate_linear(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        let velocity = self.vector_prop(id, "Velocity").unwrap_or([0.0; 3]);
        self.integrate_move(id, velocity, dt, true)?;
        Ok(())
    }

    /// `PHYS_Projectile`: `Velocity += Acceleration*dt` (no zone gravity), one swept move; the
    /// engine's exit tail (`AActor::physProjectile` 0x103c0ca6-0x103c0d03) then rewrites
    /// `Velocity = (Location - MoveStart)/dt` for a projectile with the bounce bits clear — a
    /// blocked projectile's velocity decays toward zero with the actual displacement, and its
    /// `Physics` stays `PHYS_Projectile` (the `Physics == PHYS_Falling` check at 0x103c0c51 only
    /// continues with `physFalling` when a `HitWall` script handler set it). This is what parks
    /// the grapple demo's `CineHook` at the ceiling it strikes and lets the demo's
    /// `Crochet.Velocity.Z < 1` cast transition fire (probe23: Jones climbs the full 675 to it).
    fn integrate_projectile(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        let Some(start) = self.vector_prop(id, "Location") else {
            return Ok(());
        };
        let acceleration = self.vector_prop(id, "Acceleration").unwrap_or([0.0; 3]);
        let mut velocity = self.vector_prop(id, "Velocity").unwrap_or([0.0; 3]);
        for axis in 0..3 {
            velocity[axis] += acceleration[axis] * dt;
        }
        self.set_property(id, "Velocity", 0, Value::Vector(velocity));
        let hit = self.integrate_move(id, velocity, dt, true)?;
        // `HitWall` script may have destroyed the projectile (`Destroy` in a HitWall handler);
        // the engine checks `bDeleteMe` before the exit tail.
        if self.objects[id as usize].deleted {
            return Ok(());
        }
        // Exit tail: the tracked bounce gate is `bBounce` (+0x34 bit 0x800000); the second
        // clear-bit (0x8000000) is not tracked as a property and the VM actors that reach this
        // path (CineHook, XIIIProjectile rounds) have it clear.
        if !self.bool_prop(id, "bBounce") {
            let end = self.vector_prop(id, "Location").unwrap_or(start);
            let moved = [
                (end[0] - start[0]) / dt,
                (end[1] - start[1]) / dt,
                (end[2] - start[2]) / dt,
            ];
            self.set_property(id, "Velocity", 0, Value::Vector(moved));
        }
        let _ = hit;
        Ok(())
    }

    /// One swept move by `velocity*dt` with the actor's collision extent against the world and
    /// registered movers. Returns the blocking world hit, if any. The actor stops at the sweep
    /// contact point; with `dispatch`, a hit runs the `HitWall(Vector, Actor)` event.
    fn integrate_move(
        &mut self,
        id: ObjectId,
        velocity: [f32; 3],
        dt: f32,
        dispatch: bool,
    ) -> VmResult<Option<crate::physics::WorldHit>> {
        let Some(location) = self.vector_prop(id, "Location") else {
            return Ok(None);
        };
        let delta = scale3(velocity, dt);
        if delta == [0.0; 3] {
            return Ok(None);
        }
        let end = add3(location, delta);
        let extent = self.actor_extent(id);
        let outcome = if self.bool_prop(id, "bCollideWorld") {
            match self.physics.as_mut() {
                Some(provider) => {
                    let (hit, _) = provider.trace_with_mover(location, end, extent);
                    hit
                }
                None => None,
            }
        } else {
            None
        };
        match &outcome {
            Some(hit) => {
                self.set_property(id, "Location", 0, Value::Vector(hit.location));
                // Contact response (engine `physFalling`/`physProjectile` move-with-collision):
                // remove the velocity component into the surface; a floor-grade contact (the
                // engine's pawn `Landed` -> `PHYS_Walking` + ground friction, decoded) stops the
                // actor instead of leaving it sliding forever. The resulting velocity is
                // written back so a Falling actor's post-contact state is observable (the
                // demo's `Crochet.Velocity.Z < 1` cast transition reads it after a wall catch).
                let mut velocity = self.vector_prop(id, "Velocity").unwrap_or([0.0; 3]);
                let into = dot3(velocity, hit.normal);
                if into < 0.0 {
                    velocity = sub3(velocity, scale3(hit.normal, into));
                }
                if hit.normal[2] > 0.7 {
                    velocity = [0.0; 3];
                }
                self.set_property(id, "Velocity", 0, Value::Vector(velocity));
                if dispatch {
                    self.send_event(
                        id,
                        "HitWall",
                        vec![
                            Value::Vector(hit.normal),
                            Value::Object(Some(ObjRef::Instance(id))),
                        ],
                    )?;
                }
            }
            None => {
                self.set_property(id, "Location", 0, Value::Vector(end));
            }
        }
        Ok(outcome)
    }

    /// Zone gravity at the actor's location for physics integration. The engine reads the
    /// enclosing `PhysicsVolume`'s `Gravity`; the VM tracks no volumes, so this is the measured
    /// `Engine.PhysicsVolume` class default `(0, 0, -950)`.
    fn zone_gravity(&mut self, id: ObjectId) -> [f32; 3] {
        let at = self.vector_prop(id, "Location").unwrap_or([0.0; 3]);
        match self.physics.as_mut() {
            Some(provider) => provider.zone_gravity(at),
            None => [0.0, 0.0, -950.0],
        }
    }

    /// Test-only direct-step adapter for the shared cinematic collision walker. Controller
    /// latents use controller_move_step; XIDCine supplies its own arrival radius below.
    #[cfg(test)]
    pub(crate) fn move_pawn_step(
        &mut self,
        pawn: ObjectId,
        destination: [f32; 3],
        speed: f32,
        dt: f32,
    ) -> VmResult<bool> {
        let radius = self.f32_prop(pawn, "CollisionRadius");
        self.move_pawn_step_within(pawn, destination, speed, dt, radius)
    }

    /// Test-only direct-step adapter for the shared cinematic collision walker. Controller
    /// latents use controller_move_step; the cine steering no longer uses a direct step
    /// (item53b: the decoded `execSteering` writes `Acceleration`/speed fields for the physics
    /// pass instead).
    #[cfg(test)]
    pub(crate) fn move_pawn_step_within(
        &mut self,
        pawn: ObjectId,
        destination: [f32; 3],
        speed: f32,
        dt: f32,
        radius: f32,
    ) -> VmResult<bool> {
        let location = self.vector_prop(pawn, "Location").unwrap_or(destination);
        let speed = if speed > 0.0 {
            speed
        } else {
            self.f32_prop(pawn, "GroundSpeed")
        };
        let (next, arrived) = move_step(location, destination, speed, dt, radius);
        if arrived && next == location {
            return Ok(true);
        }
        let delta = [next[0] - location[0], next[1] - location[1], 0.0];
        let extent = self.actor_extent(pawn);
        // UE2 calls APawn::physWalking only for PHYS_Walking with world collision enabled.
        // Controller.MoveTo can be
        // requested for pawns in other physics modes (e.g. BaseSoldier defaults to PHYS_None),
        // so do not apply walking step-up/floor-follow to them. Cine Steering also bypasses this
        // path entirely while bCollideWorld is false (`collisionoff`).
        let collides_world = match self.get_property(pawn, "bCollideWorld") {
            Some(Value::Bool(enabled)) => *enabled,
            // Native Actor default is bCollideWorld=true; small synthetic VM fixtures can omit
            // the reflected slot while still modelling a walking Pawn.
            _ => true,
        };
        let walking = self.byte_prop(pawn, "Physics") == PHYS_WALKING && collides_world;
        let outcome = self.physics.as_mut().map(|p| {
            if walking {
                p.walk_box(location, delta, extent)
            } else {
                p.move_box(location, delta, extent)
            }
        });
        let end = outcome.map_or_else(|| add3(location, delta), |o| o.end);
        self.set_property(pawn, "Location", 0, Value::Vector(end));
        Ok(horizontal_distance(end, destination) <= radius)
    }

    /// `Actor.SetCollision`: omitted flags keep their current value; touching is recomputed.
    pub(crate) fn vm_set_collision(
        &mut self,
        id: ObjectId,
        col_actors: Option<bool>,
        block_actors: Option<bool>,
        block_players: Option<bool>,
    ) -> VmResult<()> {
        if let Some(v) = col_actors {
            self.set_property(id, "bCollideActors", 0, Value::Bool(v));
        }
        if let Some(v) = block_actors {
            self.set_property(id, "bBlockActors", 0, Value::Bool(v));
        }
        if let Some(v) = block_players {
            self.set_property(id, "bBlockPlayers", 0, Value::Bool(v));
        }
        self.refresh_touching(id, false)?;
        Ok(())
    }

    /// `Actor.SetCollisionSize`: update the cylinder, recompute touching. Returns true (the
    /// decoded XIII declaration carries no encroachment flag; see the registry evidence).
    pub(crate) fn vm_set_collision_size(
        &mut self,
        id: ObjectId,
        radius: f32,
        height: f32,
    ) -> VmResult<bool> {
        self.set_property(id, "CollisionRadius", 0, Value::Float(radius));
        self.set_property(id, "CollisionHeight", 0, Value::Float(height));
        self.refresh_touching(id, false)?;
        Ok(true)
    }

    // ------------------------------------------------------------------ animation

    /// Display path of an object reference (empty when it cannot be named).
    fn ref_path(&self, r: &ObjRef) -> String {
        match r {
            ObjRef::Static(g) => self.set.path(*g),
            ObjRef::Instance(i) => self
                .objects
                .get(*i as usize)
                .map(|o| o.name.clone())
                .unwrap_or_default(),
            ObjRef::External(id) => self
                .external_object(*id)
                .map_or_else(String::new, |o| o.path),
        }
    }

    /// Candidate animation sources of `id`: the `LinkSkelAnim` links in call order, then the
    /// actor's `Mesh` (a `SkeletalMesh` path, which carries the default `MeshAnimation`).
    ///
    /// UE2 keeps the render `Mesh` and the linked skeletal animations separately; the provider
    /// resolves a `SkeletalMesh` source through its default animation and a `MeshAnimation`
    /// source directly, so the VM does not need to decode either.
    pub(crate) fn animation_sources(&self, id: ObjectId) -> Vec<String> {
        let linked = self
            .objects
            .get(id as usize)
            .map(|o| o.anim.linked_anims.clone())
            .unwrap_or_default();
        let mut sources = linked;
        if let Some(Value::Object(Some(r))) = self.get_property(id, "Mesh") {
            let mesh = self.ref_path(r);
            if !mesh.is_empty() && !sources.contains(&mesh) {
                sources.push(mesh);
            }
        }
        sources
    }

    /// Queries the animation provider for `sequence` over the actor's candidate sources,
    /// returning the first hit with the source that answered. `Ok(None)` = not found anywhere
    /// (unknown sequence); a provider decode failure is returned as
    /// [`VmErrorKind::AnimationDataError`].
    fn sequence_info(
        &mut self,
        id: ObjectId,
        sequence: &str,
    ) -> VmResult<Option<(String, SeqInfo)>> {
        if self.animation.is_none() {
            return Ok(None);
        }
        let mut sources = self.animation_sources(id);
        // A diagnostic provider that ignores sources (FixedAnimation) must still be reached when
        // the actor has no `Mesh`/link; the empty source is unknown to a real provider.
        if sources.is_empty() {
            sources.push(String::new());
        }
        let mut decode_error: Option<(String, String)> = None;
        {
            let provider = self.animation.as_mut().expect("checked above");
            for source in &sources {
                match provider.sequence(source, sequence) {
                    Ok(Some(info)) => return Ok(Some((source.clone(), info))),
                    Ok(None) => {}
                    Err(message) => {
                        decode_error = Some((source.clone(), message));
                        break;
                    }
                }
            }
        }
        if let Some((source, message)) = decode_error {
            return Err(self.err(VmErrorKind::AnimationDataError {
                source,
                sequence: sequence.to_owned(),
                message,
            }));
        }
        Ok(None)
    }

    /// `Actor.LinkSkelAnim`: link a `MeshAnimation` object as an additional animation source of
    /// the actor. Upstream keeps the render `Mesh` separate, so the `Mesh` property is left
    /// untouched (the VM does not overwrite a `SkeletalMesh` with a `MeshAnimation`).
    pub(crate) fn link_skel_anim(&mut self, id: ObjectId, anim: Option<ObjRef>) {
        let path = match anim {
            Some(r) => self.ref_path(&r),
            None => String::new(),
        };
        if path.is_empty() {
            return;
        }
        let linked = &mut self.objects[id as usize].anim.linked_anims;
        if !linked.contains(&path) {
            linked.push(path);
        }
    }

    /// `Actor.AnimBlendParams`: store the blending parameters of `stage`/`channel`.
    pub(crate) fn anim_blend_params(
        &mut self,
        id: ObjectId,
        stage: i32,
        blend_alpha: f32,
        in_time: f32,
        out_time: f32,
        bone_name: Option<String>,
    ) {
        if !(1..=255).contains(&stage) {
            return;
        }
        self.objects[id as usize].anim.blend_params.insert(
            stage,
            AnimBlendParams {
                blend_alpha,
                in_time: in_time.min(1.0),
                out_time: out_time.min(1.0),
                bone_name,
                alpha_target: None,
            },
        );
    }

    /// BlendToAlpha preserves the subtree and uses a remaining seconds interval.
    pub(crate) fn anim_blend_to_alpha(&mut self, id: ObjectId, stage: i32, target: f32, time: f32) {
        if !(1..=255).contains(&stage) {
            return;
        }
        let p = self.objects[id as usize]
            .anim
            .blend_params
            .entry(stage)
            .or_insert(AnimBlendParams {
                blend_alpha: 0.0,
                in_time: 0.0,
                out_time: 0.0,
                bone_name: None,
                alpha_target: None,
            });
        if time <= 0.0 {
            p.blend_alpha = target;
            p.alpha_target = None;
        } else {
            p.alpha_target = Some((target, time));
        }
    }

    /// `Actor.PlayAnim`/`LoopAnim`/`TweenAnim`: start `sequence` on `channel`. Zero rate
    /// holds frame zero; negative velocity-dependent rates remain Partial. Tween time holds
    /// frame zero before playback. Unknown sequences produce a visible no-op.
    pub(crate) fn start_animation(
        &mut self,
        id: ObjectId,
        sequence: &str,
        rate: f32,
        tween_time: f32,
        channel: u8,
        looping: bool,
    ) -> VmResult<()> {
        // None is an unknown sequence, not StopAnimating: the retail lookup returns
        // without changing playback (0x103f5723..0x103f5776).
        if sequence.eq_ignore_ascii_case("None") {
            self.note(TraceKind::Note(
                "Actor.PlayAnim('None'): no sequence, playback unchanged".into(),
            ));
            return Ok(());
        }
        if self.animation.is_none() {
            return Err(self.err(VmErrorKind::NoAnimationProvider {
                native: "Actor.PlayAnim".into(),
            }));
        }
        if !rate.is_finite() || !tween_time.is_finite() {
            return Err(self.err(VmErrorKind::AnimationDataError {
                source: self.animation_sources(id).join(", "),
                sequence: sequence.to_owned(),
                message: "nonfinite animation rate or tween duration".into(),
            }));
        }
        // UE2 `AActor::PlayAnim` returns immediately when `Mesh == NULL` (Engine.dll
        // `?PlayAnim@AActor` RVA 0xDF8B0: it tests the mesh pointer at `this+0x138` and jumps
        // straight to the epilogue, logging, when it is null). An actor with no animation
        // source therefore no-ops instead of failing. The diagnostic `FixedAnimation` provider
        // still answers the empty source, so harness diagnostics are unaffected.
        let mesh_less = self.animation_sources(id).is_empty();
        let Some((source, info)) = self.sequence_info(id, sequence)? else {
            if mesh_less {
                let actor = self.objects[id as usize].name.clone();
                self.note(TraceKind::Note(format!(
                    "Actor.PlayAnim('{sequence}') on {actor}: no mesh, UE2 returns without \
                     playing (no-op)"
                )));
                return Ok(());
            }
            // UE2 `AActor::PlayAnim`/`LoopAnim` look the sequence up in the mesh's animation
            // set and simply play nothing when it is absent (no state failure). The shipped
            // maps rely on this: `xidcine.Cine2.PostBeginPlay` calls `LoopAnim(DefaultAnim)`
            // with `DefaultAnim` values ("Wait", "acqiesce") that no decoded source of the
            // actor carries. A *decode* failure is still fatal (`AnimationDataError`), so a
            // corrupt provider is never hidden; a genuinely unknown name is a visible no-op.
            let mesh = self.animation_sources(id).join(", ");
            self.note(TraceKind::Note(format!(
                "Actor.PlayAnim('{sequence}') not in [{mesh}]: UE2 plays nothing (no-op)"
            )));
            return Ok(());
        };
        // Nonloop PlayAnim takes Rate<=0 through 0x103f5d2f. Negative Rate returns
        // false immediately at 0x103f5d3d; only exactly zero initializes a hold/tween.
        // A sequence with no frames is refused in both branches, without replacing the
        // previous channel. LoopAnim has a separate velocity-dependent negative-rate path.
        if info.frames == 0 || (!looping && rate < 0.0) {
            self.note(TraceKind::Note(format!(
                "Actor.PlayAnim('{sequence}'): retail refuses zero frames or negative nonloop rate; playback unchanged"
            )));
            return Ok(());
        }
        if !info.rate.is_finite() || (rate > 0.0 && !(rate * info.rate).is_finite()) {
            return Err(self.err(VmErrorKind::AnimationDataError {
                source: self.animation_sources(id).join(", "),
                sequence: sequence.to_owned(),
                message: "nonfinite decoded or multiplied animation rate".into(),
            }));
        }
        // UE2 `AActor::PlayAnim`/`LoopAnim` pass `Rate` as a **multiplier** of the sequence's own
        // authored rate (`Engine.dll ?execPlayAnim@AActor` RVA 0xDF990 pushes the default `1.0`;
        // the mesh instance advances `AnimRate * Seq->Rate` frames per second). The provider's
        // `SeqInfo.rate` is that authored rate (30 fps for the decoded MeshAnimation clips), so a
        // script rate of `1.0` must play at 30 fps, not 1. Omitted native Rate defaults to 1.0;
        // explicit zero enters the retail hold/tween branch. Negative rates remain Partial.
        let natural = if info.rate > 0.0 { info.rate } else { 1.0 };
        // Retail skeletal PlayAnim's zero-rate branch 0x103f5d2f clears channel playback
        // rate and holds frame zero. An omitted native argument is 1.0, not explicit zero.
        let rate = if rate >= 0.0 { rate * natural } else { natural };
        let mut notifies = info.notifies;
        notifies.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        // Engine.dll `PlayAnim` keeps ONE cached previous pose per channel (channel+0x58
        // previous tween frame, +0x5c cached pose; see item47's report) and overwrites it on
        // every new tweened start. Authored scripts rely on that bound:
        // `xidcine.Cine2.CineInit.PlayMoving` re-runs `LoopAnim(WaitAnim, none, 0.2)` from
        // `PlayingSequence.Tick` every tick, so a chain that froze each interrupted tween
        // recursively grew without limit and failed the whole cutscene. The frozen source is
        // therefore the channel's current state with its own frozen source dropped — a single
        // cached level, like the engine. Whether the engine's cache holds the channel's blended
        // in-progress pose or its target pose is not fully decoded (the report labels it a
        // hypothesis); this freeze keeps the interrupted tween's frame and remaining time.
        let tween_source = if tween_time > 0.0 {
            let mut source = self
                .actor_animation(id)
                .and_then(|a| a.channels.into_iter().find(|c| c.channel == channel));
            if let Some(source) = source.as_mut() {
                source.tween_source = None;
            }
            source.map(Box::new)
        } else {
            None
        };
        self.objects[id as usize].anim.channels.insert(
            channel,
            AnimChannel {
                sequence: sequence.to_owned(),
                source,
                frames: info.frames,
                rate,
                frame: 0.0,
                looping,
                active: info.frames > 0,
                tween_remaining: tween_time.max(0.0),
                tween_duration: tween_time.max(0.0),
                tween_source,
                tween_only: false,
                loop_end_sent: false,
                notifies,
                notify_idx: 0,
            },
        );
        self.mirror_base_animation(id);
        Ok(())
    }

    /// TweenAnim reaches the first frame then holds; it never starts clip playback.
    pub(crate) fn mark_tween_only(&mut self, id: ObjectId, channel: u8) {
        if let Some(c) = self.objects[id as usize].anim.channels.get_mut(&channel) {
            c.tween_only = true;
            c.rate = 0.0;
            c.active = c.tween_remaining > 0.0;
        }
        self.mirror_base_animation(id);
    }

    /// Actor properties mirror channel zero in the DLL's normalized sequence units.
    fn mirror_base_animation(&mut self, id: ObjectId) {
        let Some(c) = self.objects[id as usize].anim.channels.get(&0) else {
            return;
        };
        let sequence = c.sequence.clone();
        let finished = !c.active;
        let (frame, rate) = self.anim_channel_params(id, 0).unwrap_or((0.0, 0.0));
        self.set_property(id, "AnimSequence", 0, Value::Name(sequence));
        self.set_property(id, "AnimRate", 0, Value::Float(rate));
        self.set_property(id, "AnimFrame", 0, Value::Float(frame));
        self.set_property(id, "bAnimFinished", 0, Value::Bool(finished));
    }

    pub(crate) fn anim_channel_sequence(&self, id: ObjectId, channel: u8) -> Option<&str> {
        self.objects
            .get(id as usize)?
            .anim
            .channels
            .get(&channel)
            .map(|c| c.sequence.as_str())
    }

    /// Current pose parameters of `channel`: `(sequence, source, frame, looping)`.
    pub(crate) fn anim_channel_pose(
        &self,
        id: ObjectId,
        channel: u8,
    ) -> Option<(String, String, f32, bool)> {
        self.objects
            .get(id as usize)?
            .anim
            .channels
            .get(&channel)
            .map(|c| (c.sequence.clone(), c.source.clone(), c.frame, c.looping))
    }

    /// `Engine.Actor.GetBoneCoords` pose query: world-space position of `bone` from the channel
    /// 0 pose through the animation provider, or `None` when no posed answer is available (no
    /// provider, no playing channel, unknown mesh/sequence/bone, or a decode failure).
    ///
    /// The provider returns the posed bone offset relative to the actor origin in
    /// actor-rotation space (its `RotOrigin`/`MeshOrigin`/scale applied); the actor's own yaw
    /// and `Location` are added here. The rotation is the source-space R(yaw) convention —
    /// `x' = x·cos − y·sin, y' = x·sin + y·cos` — the same policy as the pawn renderer's
    /// `root_transform` (XIII characters are authored +Y-forward; the mesh `RotOrigin` turns
    /// that onto the actor's forward axis).
    pub(crate) fn posed_bone_origin(&mut self, id: ObjectId, bone: &str) -> Option<[f32; 3]> {
        let (sequence, source, frame, looping) = self.anim_channel_pose(id, 0)?;
        if sequence.is_empty() || self.animation.is_none() {
            return None;
        }
        let mesh = match self.get_property(id, "Mesh") {
            Some(Value::Object(Some(r))) => self.ref_path(r),
            _ => String::new(),
        };
        if mesh.is_empty() {
            return None;
        }
        let offset = {
            let provider = self.animation.as_mut().expect("checked above");
            match provider.bone_offset(&mesh, &source, &sequence, frame, looping, bone) {
                Ok(offset) => offset,
                // A decode failure is surfaced by `sequence` lookups on the same data; here a
                // posed query quietly falls back to the actor origin like the item19 Partial.
                Err(_) => return None,
            }
        };
        let offset = offset?;
        let location = self.vector_prop(id, "Location").unwrap_or([0.0; 3]);
        let yaw = self.rotation_prop(id).unwrap_or([0; 3])[1];
        let theta = (yaw as f32) * std::f32::consts::TAU / 65536.0;
        let (sin, cos) = theta.sin_cos();
        Some([
            location[0] + offset[0] * cos - offset[1] * sin,
            location[1] + offset[0] * sin + offset[1] * cos,
            location[2] + offset[2],
        ])
    }

    /// `Actor.StopAnimating` (native 417): stop every animation channel and clear the animation
    /// properties, like UE2 `AActor::StopAnimating`. Decoded call site
    /// `engine.Inventory.DropFrom` 0x003A (immediately before `GotoState('None')`).
    pub(crate) fn stop_animating(&mut self, id: ObjectId) {
        self.objects[id as usize].anim.channels.clear();
        self.set_property(id, "AnimSequence", 0, Value::Name("None".into()));
        self.set_property(id, "AnimRate", 0, Value::Float(0.0));
        self.set_property(id, "AnimFrame", 0, Value::Float(0.0));
    }

    /// `Actor.HasAnim`: whether any of the actor's animation sources has `sequence`.
    pub(crate) fn has_anim(
        &mut self,
        native: &str,
        id: ObjectId,
        sequence: &str,
    ) -> VmResult<bool> {
        if self.animation.is_none() {
            return Err(self.err(VmErrorKind::NoAnimationProvider {
                native: native.to_owned(),
            }));
        }
        Ok(self.sequence_info(id, sequence)?.is_some())
    }

    /// True when `channel` currently has an active animation.
    pub(crate) fn anim_channel_active(&self, id: ObjectId, channel: u8) -> bool {
        self.objects
            .get(id as usize)
            .is_some_and(|o| o.anim.channels.get(&channel).is_some_and(|c| c.active))
    }

    /// `(normalized frame, normalized rate)` as exported by GetAnimParams (Engine.dll
    /// GetAnimFrame 0x103ee9a9 reads channel+0x10 directly). Tween frames are negative.
    pub(crate) fn anim_channel_params(&self, id: ObjectId, channel: u8) -> Option<(f32, f32)> {
        self.objects
            .get(id as usize)
            .and_then(|o| o.anim.channels.get(&channel))
            .map(|c| {
                let n = c.frames.max(1) as f32;
                let frame = if c.tween_duration > 0.0 && c.tween_remaining > 0.0 {
                    -c.tween_remaining / (c.tween_duration * n)
                } else {
                    c.frame / n
                };
                (frame, if c.active { c.rate / n } else { 0.0 })
            })
    }

    /// `Actor.FinishAnim`: suspend state code until `channel` ends. Returns `false` (no latent)
    /// when the channel is not animating; an error when called outside state code, like `Sleep`.
    pub(crate) fn finish_anim(
        &mut self,
        id: ObjectId,
        channel: u8,
        in_state: bool,
    ) -> VmResult<bool> {
        if !self.anim_channel_active(id, channel) {
            return Ok(false);
        }
        if !in_state {
            return Err(self.err(VmErrorKind::LatentOutsideState {
                path: "Actor.FinishAnim".into(),
            }));
        }
        // execFinishAnim stops loop playback so the current cycle can finish.
        if let Some(c) = self.objects[id as usize].anim.channels.get_mut(&channel) {
            c.looping = false;
        }
        self.pending_latent = Some(Latent::AnimEnd {
            channel,
            started: self.time,
        });
        Ok(true)
    }

    /// Advances every channel of `id` by `dt` frames, firing notifies and `AnimEnd` once.
    /// UE2 `AActor::physInterpolating`-style advance for a `Mover` with `bInterpolating` set:
    /// `PhysAlpha += PhysRate * dt` and `Location`/`Rotation` interpolate from `OldPos`/`OldRot`
    /// to `BasePos+KeyPos[KeyNum]`/`BaseRot+KeyRot[KeyNum]`. On `PhysAlpha >= 1` the actor snaps
    /// to the key, `bInterpolating` clears and `KeyFrameReached` fires (which may chain the next
    /// key for a multi-key mover). The latent `Actor.FinishInterpolation` then resumes.
    ///
    /// Evidence: `engine.Mover.InterpolateTo`/`KeyFrameReached`/`DoOpen` disassembly
    /// (`native#301 engine.Actor.FinishInterpolation`, `native#3970 SetPhysics`); the per-tick
    /// alpha/rate advance is the upstream UE2 `PHYS_MovingBrush` move
    /// (`AActor::physInterpolating`), not a decoded DLL body (labelled a hypothesis).
    fn advance_interpolation(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        if self.objects.get(id as usize).is_none_or(|o| o.deleted) {
            return Ok(());
        }
        if !self.interpolating(id) {
            return Ok(());
        }
        let rate = self.f32_prop(id, "PhysRate");
        if rate <= 0.0 || !rate.is_finite() {
            // The script sets `1/max(Seconds, 0.005)`, always positive; a non-positive rate
            // cannot advance, so the mover is left suspended rather than silently snapped.
            return Ok(());
        }
        let alpha = self.f32_prop(id, "PhysAlpha") + rate * dt;
        let key_num = usize::from(self.byte_prop(id, "KeyNum"));
        let old_pos = self
            .vector_prop(id, "OldPos")
            .or_else(|| self.vector_prop(id, "Location"))
            .unwrap_or([0.0; 3]);
        let old_rot = self
            .rotator_prop(id, "OldRot")
            .or_else(|| self.rotator_prop(id, "Rotation"))
            .unwrap_or([0; 3]);
        let base_pos = self.vector_prop(id, "BasePos").unwrap_or([0.0; 3]);
        let base_rot = self.rotator_prop(id, "BaseRot").unwrap_or([0; 3]);
        let key_pos = self
            .vector_prop_elem(id, "KeyPos", key_num)
            .unwrap_or([0.0; 3]);
        let key_rot = self
            .rotator_prop_elem(id, "KeyRot", key_num)
            .unwrap_or([0; 3]);
        let target_pos = add3(base_pos, key_pos);
        let target_rot = [
            base_rot[0].wrapping_add(key_rot[0]),
            base_rot[1].wrapping_add(key_rot[1]),
            base_rot[2].wrapping_add(key_rot[2]),
        ];
        if alpha >= 1.0 {
            self.set_property(id, "PhysAlpha", 0, Value::Float(1.0));
            self.set_property(id, "Location", 0, Value::Vector(target_pos));
            self.set_property(id, "Rotation", 0, Value::Rotator(target_rot));
            self.set_property(id, "bInterpolating", 0, Value::Bool(false));
            // The engine fires `KeyFrameReached` when a brush finishes interpolating.
            self.send_event(id, "KeyFrameReached", Vec::new())?;
        } else {
            self.set_property(id, "PhysAlpha", 0, Value::Float(alpha));
            self.set_property(
                id,
                "Location",
                0,
                Value::Vector(lerp3(old_pos, target_pos, alpha)),
            );
            self.set_property(
                id,
                "Rotation",
                0,
                Value::Rotator(lerp_rotator(old_rot, target_rot, alpha)),
            );
        }
        self.update_physics_mover(id);
        Ok(())
    }

    /// Mirrors a mover's current pose into the world-physics provider (if one is installed) so
    /// the VM's own `Move`/`Trace` see the moving brush. The default provider method is a no-op.
    fn update_physics_mover(&mut self, id: ObjectId) {
        let Some(name) = self.objects.get(id as usize).map(|o| o.name.clone()) else {
            return;
        };
        let location = self.vector_prop(id, "Location").unwrap_or([0.0; 3]);
        let rotation = self.rotator_prop(id, "Rotation").unwrap_or([0; 3]);
        if let Some(p) = self.physics.as_mut() {
            p.set_mover(&name, location, rotation);
        }
    }

    fn advance_animation(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        if self.objects.get(id as usize).is_none_or(|o| o.deleted) {
            return Ok(());
        }
        if self.objects[id as usize].anim.channels.values().any(|c| {
            c.looping
                && c.frames > 0
                && (!((c.rate * dt).is_finite()) || c.rate * dt / c.frames as f32 > 4096.0)
        }) {
            return Err(self.err(VmErrorKind::AnimationDataError {
                source: self.animation_sources(id).join(", "),
                sequence: "<advance>".into(),
                message: "animation step exceeds 4096 loop wraps".into(),
            }));
        }
        // The actor name is only needed when a notify or animation end fires; the old code
        // cloned it unconditionally, allocating a string for every active actor every tick
        // (most have no active channel).
        let mut notifies: Vec<(u8, String)> = Vec::new();
        let mut ended: Vec<(u8, f32)> = Vec::new();
        {
            let o = &mut self.objects[id as usize];
            for p in o.anim.blend_params.values_mut() {
                if let Some((target, remaining)) = p.alpha_target {
                    p.blend_alpha += (target - p.blend_alpha) * (dt / remaining).min(1.0);
                    p.alpha_target = if remaining > dt {
                        Some((target, remaining - dt))
                    } else {
                        None
                    };
                }
            }
            for (&channel, st) in o.anim.channels.iter_mut() {
                if !st.active {
                    continue;
                }
                let step_dt = if st.tween_remaining > 0.0 {
                    let spent = dt.min(st.tween_remaining);
                    st.tween_remaining -= spent;
                    if st.tween_remaining > 0.0 {
                        continue;
                    }
                    st.tween_source = None;
                    if st.tween_only {
                        st.active = false;
                        ended.push((channel, 0.0));
                        continue;
                    }
                    dt - spent
                } else {
                    dt
                };
                if st.tween_only {
                    continue;
                }
                let length = st.frames as f32;
                let end = if st.looping { length } else { length - 1.0 };
                let mut remaining = (st.rate * step_dt).max(0.0);
                loop {
                    let old = st.frame;
                    let next = (old + remaining).min(end.max(0.0));
                    while st.notify_idx < st.notifies.len() {
                        let (t, name) = &st.notifies[st.notify_idx];
                        let target = *t * length;
                        if target > next {
                            break;
                        }
                        if old < target && target >= 0.0 {
                            notifies.push((channel, name.clone()));
                        }
                        st.notify_idx += 1;
                    }
                    if st.looping && !st.loop_end_sent && old < length - 1.0 && next >= length - 1.0
                    {
                        ended.push((channel, length - 1.0));
                        st.loop_end_sent = true;
                    }
                    st.frame = next;
                    remaining -= next - old;
                    if st.frame < end {
                        break;
                    }
                    if st.looping && length > 0.0 {
                        st.frame = 0.0;
                        st.notify_idx = 0;
                        st.loop_end_sent = false;
                        if remaining <= 0.0 {
                            break;
                        }
                    } else {
                        st.active = false;
                        ended.push((channel, st.frame));
                        break;
                    }
                }
            }
        }
        self.mirror_base_animation(id);
        for (channel, function) in notifies {
            let actor = self.objects[id as usize].name.clone();
            self.note(TraceKind::AnimNotify {
                actor,
                function: function.clone(),
                channel,
            });
            self.send_event(id, &function, Vec::new())?;
        }
        for (channel, _frame) in ended {
            let actor = self.objects[id as usize].name.clone();
            self.note(TraceKind::AnimEnd { actor, channel });
            // UE2 `APawn::NotifyAnimEnd` (Engine.dll RVA 0xB0A00) sends `AnimEnd(Channel)` to the
            // **controller** when the controller is probing the event (`UObject::IsProbing`),
            // otherwise to the pawn itself. The decoded cutscene controllers rely on this: the
            // animation runs on the possessed `Cine2` pawn but the handler that clears the
            // sequence pause bit is the controller's `PlayingSequence.AnimEnd`; without the
            // forward the intro waits forever. `IsProbing` is modelled as "the controller's
            // state-aware handler is not the empty `Engine.Actor.AnimEnd` base".
            let controller = self.obj_prop(id, "Controller");
            let controller_probes = controller.is_some_and(|c| {
                self.find_function(c, "AnimEnd", true)
                    .is_some_and(|f| !self.short_path(f).ends_with("Actor.AnimEnd"))
            });
            match (controller_probes, controller) {
                (true, Some(c)) => {
                    self.send_event(c, "AnimEnd", vec![Value::Int(i32::from(channel))])?;
                }
                _ => {
                    self.send_event(id, "AnimEnd", vec![Value::Int(i32::from(channel))])?;
                }
            }
        }
        Ok(())
    }

    /// True when `id`'s `Owner` chain (including itself) contains `other` (UE1 `IsOwnedBy`).
    pub(crate) fn is_owned_by(&self, id: ObjectId, other: ObjectId) -> bool {
        let mut cur = Some(id);
        let mut guard = 0;
        while let Some(c) = cur {
            if c == other {
                return true;
            }
            guard += 1;
            if guard > 4096 {
                break;
            }
            cur = self.obj_prop(c, "Owner");
        }
        false
    }

    /// Actors iterated by `DynamicActors` (non-static actors of a class with a tag), in
    /// object order.
    pub(crate) fn dynamic_actors(
        &self,
        base: Option<GlobalRef>,
        tag: Option<&str>,
    ) -> Vec<ObjectId> {
        let mut out = Vec::new();
        for (i, o) in self.objects.iter().enumerate() {
            if !o.is_actor || o.deleted || o.name.starts_with("Default__") {
                continue;
            }
            if let Some(b) = base
                && !o.layout.chain.contains(&b)
            {
                continue;
            }
            // UE2 DynamicActors starts at the first non-static actor (bStatic actors first).
            let is_static = o
                .layout
                .slot_by_name("bStatic")
                .and_then(|s| o.props.get(s.base))
                == Some(&Value::Bool(true));
            if is_static {
                continue;
            }
            if let Some(tag) = tag {
                let t = o
                    .layout
                    .slot_by_name("Tag")
                    .and_then(|s| o.props.get(s.base));
                match t {
                    Some(Value::Name(n)) if n.eq_ignore_ascii_case(tag) => {}
                    _ => continue,
                }
            }
            out.push(i as ObjectId);
        }
        out
    }

    /// Actors iterated by `AllActors`: all live actors of a class (static included), in object
    /// order.
    pub(crate) fn all_actors(&self, base: Option<GlobalRef>, tag: Option<&str>) -> Vec<ObjectId> {
        let mut out = Vec::new();
        for (i, o) in self.objects.iter().enumerate() {
            if !o.is_actor || o.deleted || o.name.starts_with("Default__") {
                continue;
            }
            if let Some(b) = base
                && !o.layout.chain.contains(&b)
            {
                continue;
            }
            if let Some(tag) = tag {
                let t = o
                    .layout
                    .slot_by_name("Tag")
                    .and_then(|s| o.props.get(s.base));
                match t {
                    Some(Value::Name(n)) if n.eq_ignore_ascii_case(tag) => {}
                    _ => continue,
                }
            }
            out.push(i as ObjectId);
        }
        out
    }

    /// Actors iterated by `RadiusActors`: live actors of `base` whose `Location` lies within
    /// `radius` of `loc` (inclusive), in object order.
    pub(crate) fn radius_actors(
        &self,
        base: Option<GlobalRef>,
        radius: f32,
        loc: [f32; 3],
    ) -> Vec<ObjectId> {
        let r2 = radius * radius;
        let mut out = Vec::new();
        for (i, o) in self.objects.iter().enumerate() {
            if !o.is_actor || o.deleted || o.name.starts_with("Default__") {
                continue;
            }
            if let Some(b) = base
                && !o.layout.chain.contains(&b)
            {
                continue;
            }
            let l = self
                .vector_prop(i as ObjectId, "Location")
                .unwrap_or([0.0; 3]);
            let (dx, dy, dz) = (l[0] - loc[0], l[1] - loc[1], l[2] - loc[2]);
            if dx * dx + dy * dy + dz * dz <= r2 {
                out.push(i as ObjectId);
            }
        }
        out
    }

    pub(crate) fn time_now(&self) -> f64 {
        self.time
    }

    /// Actors iterated by `VisibleDamageableActors` (item48): live actors of `base` within
    /// `radius` of `loc` (the same distance filter as [`Vm::radius_actors`], the measured shape
    /// of the retail level-hash query at Engine.dll 0x103e90a0), minus hidden actors when
    /// `ignore_hidden`, minus actors whose world-geometry line from `loc` to their `Location` is
    /// blocked (the retail SingleLineCheck with flags 0x86). Documented gaps (Partial at the
    /// native): the retail visibility point is a bounds mid-point (0.5 constant at 0x1046f584)
    /// rather than `Location`, the retail check also honours two unidentified class constants
    /// and a second higher trace point, and actor occlusion (the retail check can be blocked by
    /// actors, not only world geometry) is not modelled — the provider trace is world-only.
    pub(crate) fn visible_damageable_actors(
        &mut self,
        base: Option<GlobalRef>,
        radius: f32,
        loc: [f32; 3],
        ignore_hidden: bool,
    ) -> VmResult<Vec<ObjectId>> {
        let candidates: Vec<(ObjectId, [f32; 3])> = self
            .radius_actors(base, radius, loc)
            .into_iter()
            .filter(|&id| !(ignore_hidden && self.bool_prop(id, "bHidden")))
            .map(|id| {
                let end = self.vector_prop(id, "Location").unwrap_or([0.0; 3]);
                (id, end)
            })
            .collect();
        match self.physics.as_mut() {
            Some(p) => {
                let mut out = Vec::new();
                for (id, end) in candidates {
                    if p.trace(loc, end, [0.0; 3]).is_none() {
                        out.push(id);
                    }
                }
                Ok(out)
            }
            None => Err(self.err(VmErrorKind::NoPhysicsProvider {
                native: "Actor.VisibleDamageableActors".into(),
            })),
        }
    }

    /// Retail PC CRT rand (MSVCR70.dll 0x7c02836d), shared by appRand/appFrand.
    /// Uses the low 32 bits of the configured seed and unsigned wrapping arithmetic.
    pub(crate) fn next_random(&mut self) -> u64 {
        let state = (self.rng as u32).wrapping_mul(214013).wrapping_add(2531011);
        self.rng = u64::from(state);
        u64::from((state >> 16) & 0x7fff)
    }

    /// Core.dll appFrand 0x1010ffd0: CRT rand times float bits 0x38000100.
    /// Both endpoints are possible.
    pub(crate) fn rand_float(&mut self) -> f32 {
        self.next_random() as f32 * f32::from_bits(0x38000100)
    }

    /// `Rand(Max)`: a deterministic int in `[0, Max)` (0 when `Max <= 0`).
    pub(crate) fn rand_int(&mut self, max: i32) -> i32 {
        if max <= 0 {
            0
        } else {
            (self.next_random() % u64::from(max as u32)) as i32
        }
    }

    /// Statistic: functions in the set flagged native (for reports).
    pub fn is_native_function(&self, g: GlobalRef) -> bool {
        matches!(self.set.object(g), Some(ScriptObject::Function(f)) if f.flags & function_flags::NATIVE != 0)
    }
}

/// Paths of the native meta-classes (`Core.Class` and friends) that have no export.
fn meta_class_path(path: &str) -> bool {
    matches!(
        path.to_ascii_lowercase().as_str(),
        "core.class" | "core.object" | "core.struct" | "core.function" | "core.state"
    )
}

fn add3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn sub3(a: [f32; 3], b: [f32; 3]) -> [f32; 3] {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn dot3(a: [f32; 3], b: [f32; 3]) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn scale3(a: [f32; 3], scale: f32) -> [f32; 3] {
    a.map(|component| component * scale)
}

fn normalize3(a: [f32; 3]) -> [f32; 3] {
    let size_sq = dot3(a, a);
    if size_sq == 0.0 {
        [0.0; 3]
    } else {
        scale3(a, size_sq.sqrt().recip())
    }
}

fn lerp3(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    [
        a[0] + (b[0] - a[0]) * t,
        a[1] + (b[1] - a[1]) * t,
        a[2] + (b[2] - a[2]) * t,
    ]
}

/// Interpolates two Unreal rotators with the shortest arc on each axis. UE2 interpolates a
/// mover's `Rotation` from `OldRot` to `BaseRot+KeyRot`; the per-axis wrap keeps a door's yaw
/// crossing the 0/65535 boundary on the short side.
fn lerp_rotator(a: [i32; 3], b: [i32; 3], t: f32) -> [i32; 3] {
    std::array::from_fn(|i| {
        let mut delta = (b[i].wrapping_sub(a[i]) as f32).rem_euclid(65536.0);
        if delta > 32768.0 {
            delta -= 65536.0;
        }
        let v = a[i] as f32 + delta * t;
        (v.round() as i64).rem_euclid(65536) as i32
    })
}

/// Horizontal (XY) distance between two Unreal points.
fn horizontal_distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    let dx = a[0] - b[0];
    let dy = a[1] - b[1];
    (dx * dx + dy * dy).sqrt()
}

/// True when a cylinder `(loc, radius, half_height)` overlaps another cylinder, including the
/// exact contact boundary (XIII `Actor.TouchingActor` comparison).
fn cylinders_overlap(
    loc: [f32; 3],
    radius: f32,
    half_height: f32,
    other_loc: [f32; 3],
    other_radius: f32,
    other_half_height: f32,
) -> bool {
    if (loc[2] - other_loc[2]).abs() > half_height + other_half_height {
        return false;
    }
    let (dx, dy) = (loc[0] - other_loc[0], loc[1] - other_loc[1]);
    dx * dx + dy * dy <= (radius + other_radius) * (radius + other_radius)
}

/// Earliest fraction `t in [0, 1]` at which a point moving `start + t*delta` first sits inside
/// the (inclusive) vertical cylinder `(center, radius, half_height)`, or `None`.
fn segment_cylinder_contact(
    start: [f32; 3],
    delta: [f32; 3],
    center: [f32; 3],
    radius: f32,
    half_height: f32,
) -> Option<f32> {
    let (px, py) = (start[0] - center[0], start[1] - center[1]);
    let (dx, dy) = (delta[0], delta[1]);
    let a = dx * dx + dy * dy;
    let b = 2.0 * (px * dx + py * dy);
    let c = px * px + py * py - radius * radius;
    let (x0, x1) = if a <= f32::EPSILON {
        if c <= 0.0 { (0.0, 1.0) } else { return None }
    } else {
        let disc = b * b - 4.0 * a * c;
        if disc < 0.0 {
            return None;
        }
        let sq = disc.sqrt();
        ((-b - sq) / (2.0 * a), (-b + sq) / (2.0 * a))
    };
    let z = start[2] - center[2];
    let dz = delta[2];
    let (z0, z1) = if dz.abs() <= f32::EPSILON {
        if z.abs() <= half_height {
            (0.0, 1.0)
        } else {
            return None;
        }
    } else {
        let t0 = (-half_height - z) / dz;
        let t1 = (half_height - z) / dz;
        (t0.min(t1), t0.max(t1))
    };
    let lo = x0.max(z0).max(0.0);
    let hi = x1.min(z1).min(1.0);
    (lo <= hi).then_some(lo)
}

/// Ray `start -> end` vs a finite vertical cylinder. Returns `(fraction, unit normal)` of the
/// first intersection in `[0, 1]`; the normal is radial on the side and `+/-Z` on the caps.
fn segment_cylinder_hit(
    start: [f32; 3],
    end: [f32; 3],
    center: [f32; 3],
    radius: f32,
    half_height: f32,
) -> Option<(f32, [f32; 3])> {
    let d = sub3(end, start);
    let len = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    if len <= f32::EPSILON {
        return None;
    }
    let dir = [d[0] / len, d[1] / len, d[2] / len];
    let (px, py, pz) = (
        start[0] - center[0],
        start[1] - center[1],
        start[2] - center[2],
    );
    let a = dir[0] * dir[0] + dir[1] * dir[1];
    let mut hits: Vec<f32> = Vec::new();
    if a > f32::EPSILON {
        let b = 2.0 * (px * dir[0] + py * dir[1]);
        let c = px * px + py * py - radius * radius;
        let disc = b * b - 4.0 * a * c;
        if disc >= 0.0 {
            let sq = disc.sqrt();
            for t in [(-b - sq) / (2.0 * a), (-b + sq) / (2.0 * a)] {
                let z = pz + t * dir[2];
                if t >= 0.0 && t <= len && z.abs() <= half_height {
                    hits.push(t);
                }
            }
        }
    }
    if dir[2].abs() > f32::EPSILON {
        for cap in [half_height, -half_height] {
            let t = (cap - pz) / dir[2];
            if t >= 0.0 && t <= len {
                let x = px + t * dir[0];
                let y = py + t * dir[1];
                if x * x + y * y <= radius * radius {
                    hits.push(t);
                }
            }
        }
    }
    let t = hits
        .into_iter()
        .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))?;
    let p = [
        start[0] + dir[0] * t,
        start[1] + dir[1] * t,
        start[2] + dir[2] * t,
    ];
    let normal = cylinder_hit_normal(p, center, radius, half_height);
    Some((t / len, normal))
}

/// Outward surface normal of a vertical cylinder at hit point `p` (least-penetration axis).
fn cylinder_hit_normal(p: [f32; 3], center: [f32; 3], radius: f32, half_height: f32) -> [f32; 3] {
    let dz = p[2] - center[2];
    let (rx, ry) = (p[0] - center[0], p[1] - center[1]);
    let radial = (rx * rx + ry * ry).sqrt();
    let vertical_depth = half_height - dz.abs();
    let radial_depth = radius - radial;
    if radial < 1e-6 || vertical_depth <= radial_depth {
        [0.0, 0.0, if dz >= 0.0 { 1.0 } else { -1.0 }]
    } else {
        [rx / radial, ry / radial, 0.0]
    }
}

fn member_get(v: &Value, m: &str) -> Option<Value> {
    match (v, m) {
        (Value::Vector(a), "x") => Some(Value::Float(a[0])),
        (Value::Vector(a), "y") => Some(Value::Float(a[1])),
        (Value::Vector(a), "z") => Some(Value::Float(a[2])),
        (Value::Rotator(a), "pitch") => Some(Value::Int(a[0])),
        (Value::Rotator(a), "yaw") => Some(Value::Int(a[1])),
        (Value::Rotator(a), "roll") => Some(Value::Int(a[2])),
        (Value::Struct(ms), m) => ms
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(m))
            .map(|(_, v)| v.clone()),
        _ => None,
    }
}

fn member_set(v: &mut Value, m: &str, x: Value) -> bool {
    match (v, m, x) {
        (Value::Vector(a), "x", Value::Float(f)) => a[0] = f,
        (Value::Vector(a), "y", Value::Float(f)) => a[1] = f,
        (Value::Vector(a), "z", Value::Float(f)) => a[2] = f,
        (Value::Rotator(a), "pitch", Value::Int(i)) => a[0] = i,
        (Value::Rotator(a), "yaw", Value::Int(i)) => a[1] = i,
        (Value::Rotator(a), "roll", Value::Int(i)) => a[2] = i,
        (Value::Struct(ms), m, x) => match ms.iter_mut().find(|(n, _)| n.eq_ignore_ascii_case(m)) {
            Some(slot) => slot.1 = x,
            None => return false,
        },
        _ => return false,
    }
    true
}

/// UnrealScript equality used by `switch` and struct comparisons: `Name` and `string` compare
/// case-insensitively (`appStricmp`/`FName`), the rest by Rust equality. The cine interpreter's
/// `switch (GetFirstWord(Argument))` relies on this: the map scripts use lowercase action words
/// (`dial`, `event`, `wait`) while the compiled `case` values are `Dial`/`Event`/`Wait`.
pub fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Name(x), Value::Name(y)) => x.eq_ignore_ascii_case(y),
        (Value::Str(x), Value::Str(y)) => x.eq_ignore_ascii_case(y),
        (Value::Int(x), Value::Byte(y)) | (Value::Byte(y), Value::Int(x)) => *x == i32::from(*y),
        _ => a == b,
    }
}

#[cfg(test)]
mod stack_name_tests {
    use super::*;

    /// Builds a layout with one `Int` slot per name (base index = position, all dim 1).
    fn layout_with(names: &[&str]) -> ClassLayout {
        let mut by_name = HashMap::new();
        let mut slots = Vec::new();
        for (base, n) in names.iter().enumerate() {
            let lower = n.to_ascii_lowercase();
            by_name.insert(lower.clone(), slots.len());
            slots.push(Slot {
                prop: GlobalRef {
                    package: 0,
                    export: 0,
                },
                name: lower,
                ty: Ty::Int,
                dim: 1,
                base,
                flags: 0,
                declaring: GlobalRef {
                    package: 0,
                    export: 0,
                },
            });
        }
        let size = slots.len();
        ClassLayout {
            class: GlobalRef {
                package: 0,
                export: 0,
            },
            chain: vec![],
            chain_names: vec![],
            slots,
            by_prop: HashMap::new(),
            by_name,
            size,
            defaults: vec![],
            location_slot: None,
            rotation_slot: None,
            collide_slot: None,
            interp_slot: None,
            is_mover_class: false,
        }
    }

    /// The allocation-free stack path (short ASCII names) and the allocating fallback (long or
    /// non-ASCII names) must agree on case-insensitive lookups and reject missing names.
    #[test]
    fn slot_lookup_case_insensitive_short_long_and_non_ascii() {
        let long = "A".repeat(80);
        let l = layout_with(&["LoCaTiOn", "bCollideActors", &long, "café", "tail"]);
        // Short ASCII: every case spelling maps to the same slot.
        assert_eq!(l.slot_by_name("LOCATION").map(|s| s.base), Some(0));
        assert_eq!(l.slot_by_name("location").map(|s| s.base), Some(0));
        assert_eq!(l.slot_by_name("bcollideactors").map(|s| s.base), Some(1));
        // Over the 64-byte stack buffer: the allocating fallback must still match.
        assert_eq!(
            l.slot_by_name(&long.to_ascii_uppercase()).map(|s| s.base),
            Some(2)
        );
        // Non-ASCII: fallback path, exact bytes match.
        assert_eq!(l.slot_by_name("café").map(|s| s.base), Some(3));
        // The 64/65-byte boundary: both are stored lowercased and found.
        let n64 = "b".repeat(64);
        let n65 = format!("{}c", "b".repeat(64));
        let l = layout_with(&[&n64, &n65]);
        assert_eq!(
            l.slot_by_name(&n64.to_ascii_uppercase()).map(|s| s.base),
            Some(0)
        );
        assert_eq!(
            l.slot_by_name(&n65.to_ascii_uppercase()).map(|s| s.base),
            Some(1)
        );
        assert!(l.slot_by_name("missing").is_none());
    }
}

#[cfg(test)]
mod sorted_path_list_tests {
    use super::SortedPathList;

    fn keys(l: &SortedPathList) -> Vec<f32> {
        l.dist[..l.count].to_vec()
    }

    /// Ascending order; an equal key goes before the existing one; entries shifted down have
    /// their keys truncated to integers (the decoded `_ftol` reload), the inserted key does not.
    #[test]
    fn inserts_in_order_and_truncates_shifted_keys() {
        let mut l = SortedPathList::default();
        l.add(1, 30.5);
        l.add(2, 10.25);
        assert_eq!(keys(&l), [10.25, 30.0]);
        assert_eq!(&l.nodes[..2], [2, 1]);
        l.add(3, 10.25);
        assert_eq!(
            &l.nodes[..3],
            [3, 2, 1],
            "ties insert before the existing key"
        );
        assert_eq!(keys(&l), [10.25, 10.0, 30.0]);
        l.add(4, 99.9);
        assert_eq!(
            keys(&l),
            [10.25, 10.0, 30.0, 99.9],
            "an append shifts nothing"
        );
    }

    /// Capacity 32: inserting into a full list drops the largest; a key larger than all 32 is
    /// not inserted.
    #[test]
    fn is_capped_at_32_entries() {
        let mut l = SortedPathList::default();
        for i in 0..32 {
            l.add(i, (i * 10) as f32);
        }
        assert_eq!(l.count, 32);
        l.add(100, 1000.0);
        assert_eq!(l.count, 32);
        assert!(!l.nodes.contains(&100));
        l.add(200, 5.0);
        assert_eq!(l.count, 32);
        assert_eq!(&l.nodes[..3], [0, 200, 1]);
        assert_eq!(l.nodes[31], 30, "the former last entry (31) fell off");
    }

    /// With more than 8 (and 16) entries the scan starts at the coarse binary step; for sorted
    /// content that gives the same position as a full linear scan.
    #[test]
    fn binary_step_finds_the_linear_position() {
        for n in [9usize, 16, 17, 31] {
            for probe in [-1.0f32, 0.0, 5.0, 45.0, 80.0, 155.0, 1000.0] {
                let mut l = SortedPathList::default();
                for i in 0..n {
                    l.add(i as u32, (i * 10) as f32);
                }
                let expected = (0..n).position(|i| probe <= (i * 10) as f32).unwrap_or(n);
                l.add(999, probe);
                let at = l.nodes[..l.count].iter().position(|&x| x == 999);
                assert_eq!(
                    at,
                    Some(expected).filter(|&e| e < 32),
                    "n {n} probe {probe}"
                );
            }
        }
    }
}
