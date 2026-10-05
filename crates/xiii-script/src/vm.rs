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
//! decides which actors are *active* (executed). Script calls into inactive actors are
//! recorded as [`TraceKind::Deferred`] and not executed (an error if the call needs a return
//! value). Unsupported tokens, unimplemented natives, budget overruns and bad values fail with
//! [`VmError`] carrying a script stack trace. Nothing is stubbed silently.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt;
use std::rc::Rc;
use std::time::Instant;

use xiii_package::{Limits, ObjectRef, PropertyBlock, PropertyValue, RawReason, StructValue};

use crate::animation::{AnimationData, SeqInfo};
use crate::bytecode::{Call, Context, Script, Token, TokenKind, opcode_name};
use crate::canvas::CanvasState;
use crate::events::{PresentationEvent, SoundEvent};
use crate::external::ExternalObjectData;
use crate::linker::{GlobalRef, ScriptSet};
use crate::localize::{LocalizationData, placeholder};
use crate::navigation::{
    NavEdgeInfo, NavPointInfo, NavigationData, find_path, move_step, nearest_point, point_fits,
};
use crate::physics::{HitZones, WorldPhysics};
use crate::reflect::{Property, PropertyKind, ScriptObject, function_flags, property_flags};
use crate::registry::{NativeCtx, NativeDef, NativeOutcome, Registry};
use crate::value::{ObjRef, ObjectId, Ty, Value};

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
}

/// Per-channel animation playback state owned by the VM.
#[derive(Debug, Clone)]
pub(crate) struct AnimChannel {
    /// Sequence name this channel is playing.
    pub(crate) sequence: String,
    /// Total frames.
    pub(crate) frames: u32,
    /// Playback rate (frames/second).
    pub(crate) rate: f32,
    /// Current position in frames.
    pub(crate) frame: f32,
    /// Loop when reaching the end (no `AnimEnd`).
    pub(crate) looping: bool,
    /// Still playing.
    pub(crate) active: bool,
    /// Seconds still to be spent tweening in before playback advances.
    pub(crate) tween_remaining: f32,
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

/// Per-actor bone-control state set by `Pawn.SpineYawControl` / `Actor.SetBoneDirection` /
/// `Actor.SetBoneScalePerAxis`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct BoneState {
    /// Latest `Pawn.SpineYawControl` parameters, if the native ran.
    pub spine: Option<SpineControl>,
    /// `Actor.SetBoneDirection` requests, in call order.
    pub directions: Vec<BoneDirection>,
    /// `Actor.SetBoneScalePerAxis` requests, in call order.
    pub scales: Vec<BoneScale>,
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
    /// Whether the sequence loops (no `AnimEnd`).
    pub looping: bool,
    /// Still advancing (false once a non-looping sequence ended).
    pub active: bool,
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
    /// Executed by the VM (in scope).
    pub active: bool,
    /// Derives from `Actor`.
    pub is_actor: bool,
    /// Destroyed: behaves as `None` for further references.
    pub deleted: bool,
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
    Elem(Box<Place>, usize),
    Member(Box<Place>, String),
    /// A dynamic array's `Length` (UE2 `Array.Length = n` resizes the array).
    ArrayLen(Box<Place>),
}

struct IterState {
    items: Vec<Value>,
    idx: usize,
    place: Option<Place>,
    body: usize,
}

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
        self.natives_micros = 0;
        self.player_write_micros = 0;
        self.touch_micros = 0;
        self.events_micros = 0;
        self.sync_micros = 0;
        self.mover_states_micros = 0;
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
    /// Survey mode: unimplemented natives are counted and skipped instead of failing.
    pub survey: bool,
    /// Distinct unimplemented natives seen in survey mode (path -> record, first-hit order).
    pub missing_natives: std::collections::BTreeMap<String, MissingNative>,
    pub(crate) pending_latent: Option<Latent>,
    /// World-collision provider (movement/trace natives). `None` = every collision native
    /// fails with [`VmErrorKind::NoPhysicsProvider`].
    pub(crate) physics: Option<Box<dyn WorldPhysics>>,
    /// Animation-sequence provider (animation natives). `None` = every native that needs
    /// sequence data fails with [`VmErrorKind::NoAnimationProvider`].
    pub(crate) animation: Option<Box<dyn AnimationData>>,
    /// Decoded navigation graph (pathing natives). `None` = every native that needs it fails
    /// with [`VmErrorKind::NoNavProvider`].
    pub(crate) navigation: Option<Box<dyn NavigationData>>,
    /// Hit-zone provider for `Actor.GetLastTraceBone` (item14). `None` = the default
    /// [`crate::physics::CylinderZones`] is used.
    hit_zones: Option<Box<dyn HitZones>>,
    /// Bone name recorded by the most recent `Actor.Trace` actor hit, returned by
    /// `Actor.GetLastTraceBone` (`XIIIPawn.LastBoneHit`). `"None"` when the last trace hit world
    /// geometry (or nothing).
    last_trace_bone: String,
    /// Voice-wave duration provider (dialogue natives). `None` = `Actor.GetWaveDuration` reports
    /// `0` with a visible note (the script then falls back to its own default wave length).
    pub(crate) voice_duration: Option<Box<dyn crate::voice::VoiceDuration>>,
    /// Outbound presentation events emitted by presentation natives (sound, texture, display,
    /// projectors). Drained with [`Vm::drain_events`].
    events: Vec<PresentationEvent>,
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
}

fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
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
            survey: false,
            missing_natives: Default::default(),
            pending_latent: None,
            physics: None,
            animation: None,
            navigation: None,
            hit_zones: None,
            last_trace_bone: "None".to_owned(),
            voice_duration: None,
            events: Vec::new(),
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

    /// Number of presentation events waiting to be drained.
    pub fn queued_events(&self) -> usize {
        self.events.len()
    }

    /// Appends a presentation event at the current VM time.
    pub(crate) fn emit_event(&mut self, event: PresentationEvent) {
        self.events.push(event);
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

    /// Interns an external object path, returning its id. A path maps to exactly one id, so
    /// equality by path holds even when two references record different classes.
    fn intern_external(&self, path: &str, class: Option<String>) -> u32 {
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
        self.objects.push(Instance {
            class,
            name: name.to_owned(),
            props,
            layout,
            state: None,
            state_code: None,
            generation: 0,
            disabled: HashSet::new(),
            active: false,
            is_actor,
            deleted: false,
            export: None,
            timers: [None, None, None],
            anim: AnimState::default(),
            bone: BoneState::default(),
        });
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
        }
        Ok(actors)
    }

    /// Object id by display name (case-insensitive).
    pub fn find_object(&self, name: &str) -> Option<ObjectId> {
        self.objects
            .iter()
            .position(|o| !o.deleted && o.name.eq_ignore_ascii_case(name))
            .map(|i| i as ObjectId)
    }

    /// Class-level function by name, ignoring state shadowing. Host-driven verbs that the engine
    /// resolves against the class (for example the player's `Fire` while a weapon state defines an
    /// empty shadow) can call this instead of [`Vm::send_event`], which is state-aware.
    pub fn class_function(&self, id: ObjectId, name: &str) -> Option<GlobalRef> {
        self.find_function(id, name, false)
    }

    /// Marks an object as executed (in scope).
    pub fn set_active(&mut self, id: ObjectId, active: bool) {
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.active = active;
        }
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
        let r = match self.get_property(id, "Mesh") {
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
            o.bone.scales.push(BoneScale { slot, scale, bone });
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
    pub fn send_event(
        &mut self,
        id: ObjectId,
        event: &str,
        args: Vec<Value>,
    ) -> VmResult<Option<Value>> {
        self.steps = 0;
        let actor = self.objects[id as usize].name.clone();
        if self.objects[id as usize].disabled.contains(&lower(event)) {
            self.note(TraceKind::ProbeDisabled {
                actor,
                probe: event.to_owned(),
            });
            return Ok(None);
        }
        let Some(f) = self.find_function(id, event, true) else {
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
        self.call_values(f, id, args).map(Some)
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
        Ok(())
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
        for id in 0..self.objects.len() as ObjectId {
            if self.objects[id as usize].active
                && self.objects[id as usize].is_actor
                && let Err(e) = self.dispatch_tick(id, dt)
            {
                let suspended = self.suspend_for_error(id, &e);
                errors.push((suspended, e));
            }
        }
        errors
    }

    /// Fires the per-frame `Tick(DeltaTime)` event on one active actor. UE2's engine calls
    /// `AActor::Tick` (the script `event Tick`) each frame; the VM runs state code latently but
    /// must also dispatch `Tick` or per-frame script (the `CineController2` sequence interpreter,
    /// `XIIIBaseHud.Tick`, pawn controllers) never runs. `Tick` is looked up in the actor's
    /// current state first, then the class chain.
    fn dispatch_tick(&mut self, id: ObjectId, dt: f32) -> VmResult<()> {
        if let Some(f) = self.find_function(id, "Tick", true) {
            self.call_values(f, id, vec![Value::Float(dt)])?;
        }
        Ok(())
    }

    /// Suspends the actor that should stop after a failing tick: the innermost object on the
    /// error stack when it can be resolved, otherwise the actor being ticked. Returns the id.
    fn suspend_for_error(&mut self, ticked: ObjectId, e: &VmError) -> ObjectId {
        let id = e
            .stack
            .last()
            .and_then(|s| self.find_live_object(&s.object))
            .unwrap_or(ticked);
        if let Some(o) = self.objects.get_mut(id as usize) {
            o.active = false;
        }
        id
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
                return Err(self.err(VmErrorKind::BudgetExceeded {
                    limit: self.limits.max_steps,
                }));
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
                    let arrived = self.move_pawn_step(pawn, destination, speed, dt)?;
                    let left = remaining - dt;
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
        // A class-default object (`Default__Class`) is never `active`, and a `static` function
        // dispatches on the class default object; UE2 runs both regardless of instance scope
        // (`MessageClass.default.GetColor`, `Message.static.GetString`). Only non-static calls
        // on *placed* actors outside the executed scope are deferred.
        if !self.objects[target as usize].active
            && !self.objects[target as usize].name.starts_with("Default__")
            && !f.is_static()
        {
            let o = &self.objects[target as usize];
            let (tname, class) = (o.name.clone(), set.path(o.class));
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
                    place: None,
                    body: 0,
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
            let place = layout
                .params
                .iter()
                .enumerate()
                .find(|(_, p)| p.out)
                .and_then(|(i, _)| places.get(i).cloned().flatten());
            frame.iters.push(IterState {
                items: items.clone(),
                idx: 0,
                place,
                body: 0,
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
                let (func, call, index) = match &expr.kind {
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
                            opcode: expr.opcode,
                            name: "Iterator over a non-native call",
                        }));
                    }
                };
                let this = frame.this;
                match self.native_from_tokens(frame, func, call, this, index)? {
                    NativeOutcome::Iterate(items) => {
                        let found = items.iter().map(|v| self.value_text(v)).collect();
                        self.note(TraceKind::Iterator {
                            native: self.short_path(func),
                            found,
                        });
                        let st = frame.iters.last_mut().expect("pushed");
                        st.body = pc + 1;
                        if items.is_empty() {
                            Flow::Goto(self.goto_offset(frame, u32::from(*end))?)
                        } else {
                            let place = st.place.clone();
                            if let Some(pl) = place {
                                self.write(frame, &pl, items[0].clone())?;
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
                let Some(st) = frame.iters.last_mut() else {
                    return Err(
                        self.err(VmErrorKind::Other("IteratorNext without iterator".into()))
                    );
                };
                st.idx += 1;
                if st.idx < st.items.len() {
                    let (v, place, body) = (st.items[st.idx].clone(), st.place.clone(), st.body);
                    if let Some(pl) = place {
                        self.write(frame, &pl, v)?;
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

    fn context_target(
        &mut self,
        frame: &mut Frame<'s>,
        object: &Token,
        target: ObjectId,
    ) -> VmResult<Option<ObjectId>> {
        let v = self.eval_in(frame, object, target)?;
        self.context_value(v)
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
            K::Context(c) => match self.context_target(frame, &c.object, target)? {
                Some(obj) => return self.place(frame, &c.member, obj),
                None => return Ok(None),
            },
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
                let Some(base) = self.place(frame, array, target)? else {
                    return Ok(None);
                };
                if i < 0 {
                    return Err(self.err(VmErrorKind::ArrayIndex {
                        index: i64::from(i),
                        len: 0,
                    }));
                }
                Place::Elem(Box::new(base), i as usize)
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
                let Some(base) = self.place(frame, e, target)? else {
                    return Ok(None);
                };
                Place::ArrayLen(Box::new(base))
            }
            _ => return Err(self.err(VmErrorKind::NotAPlace { opcode: t.opcode })),
        }))
    }

    fn read(&self, frame: &Frame<'s>, p: &Place) -> VmResult<Value> {
        let v = match p {
            Place::Local(i) => frame.locals.get(*i).cloned(),
            Place::Slot(o, i) => self.objects[*o as usize].props.get(*i).cloned(),
            Place::Elem(base, i) => match self.read(frame, base)? {
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
            Place::ArrayLen(base) => match self.read(frame, base)? {
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
                self.objects[*o as usize].props[*i] = v;
            }
            Place::Elem(base, i) => {
                let mut arr = match self.read(frame, base)? {
                    Value::Array(a) => a,
                    other => return Err(self.type_err("array", &other)),
                };
                if *i >= arr.len() {
                    // UE2 grows a dynamic array on assignment past its end.
                    let zero = match &v {
                        Value::Int(_) => Value::Int(0),
                        Value::Float(_) => Value::Float(0.0),
                        Value::Object(_) => Value::Object(None),
                        Value::Name(_) => Value::Name("None".into()),
                        other => other.clone(),
                    };
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
            Place::ArrayLen(base) => {
                let n = match v {
                    Value::Int(i) => i.max(0) as usize,
                    other => return Err(self.type_err("int", &other)),
                };
                let mut arr = match self.read(frame, base)? {
                    Value::Array(a) => a,
                    other => return Err(self.type_err("array", &other)),
                };
                let zero = match arr.last() {
                    Some(Value::Float(_)) => Value::Float(0.0),
                    Some(Value::Object(_)) => Value::Object(None),
                    Some(Value::Name(_)) => Value::Name("None".into()),
                    Some(Value::Bool(_)) => Value::Bool(false),
                    Some(Value::Byte(_)) => Value::Byte(0),
                    _ => Value::Int(0),
                };
                arr.resize(n, zero);
                self.write(frame, base, Value::Array(arr))?;
            }
        }
        Ok(())
    }

    // ------------------------------------------------------------------ helpers for natives

    pub(crate) fn disable_probe(&mut self, id: ObjectId, probe: &str, disable: bool) {
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
        self.objects[id as usize].active = true;
        self.note(TraceKind::Spawned {
            actor: name,
            class: self.set.path(class),
        });
        Ok(Some(id))
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
    fn byte_prop(&self, id: ObjectId, name: &str) -> u8 {
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
        })
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
        // Level-start placement: a placed pickup's collision cylinder rests on the first walkable
        // surface below it (`Location.Z = surface + CollisionHeight`). Measured: Plage01
        // `Plage01CahuteKeyPick0` has `Location.Z=1257.59`, `CollisionHeight=8`, and the floor
        // plank under it is at 1259.91, so the decoded `Location` is the cylinder base and the
        // pickup is sunk into the plank; `ValidTouch`'s eye->key `FastTrace` then hits the plank.
        // This corrects the cylinder onto its support (no-op without a physics provider and
        // idempotent once resting).
        self.settle_pickups();
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

    /// **Host workaround (hypothesis, not engine behaviour found in the data):** at level start,
    /// put each placed `Pickup`'s collision cylinder on the first walkable surface below it,
    /// i.e. `Location.Z = surface_z + CollisionHeight`.
    ///
    /// Measured: Plage01 `Plage01CahuteKeyPick0` has `Location.Z=1257.5927`, `CollisionHeight=8`
    /// and the plank under it at 1259.91, so its centre is 2.3 UU below the surface and
    /// `ValidTouch`'s eye->key `FastTrace` hits the plank. Pickups keep `Physics=0` and no decoded
    /// script moves them, so how the original engine makes this pickup touchable is unknown
    /// (candidates: our placement/collision of the desk, one-sided line checks, or a different
    /// `ValidTouch` trace). Replace this with the real mechanism once found. Uses the
    /// world-physics provider; without one it is a no-op; idempotent. Returns the number of
    /// actors moved (callers should count/log it).
    pub fn settle_pickups(&mut self) -> usize {
        if self.physics.is_none() {
            return 0;
        }
        let mut settled = 0;
        for id in 0..self.objects.len() as ObjectId {
            if !self.is_live_actor(id) || !self.is_a(id, "pickup") {
                continue;
            }
            if !self.bool_prop(id, "bCollideWorld") {
                continue;
            }
            let Some(loc) = self.vector_prop(id, "Location") else {
                continue;
            };
            let h = self.f32_prop(id, "CollisionHeight");
            if h <= 0.0 {
                continue;
            }
            let end = [loc[0], loc[1], loc[2] - 2.0 * h - 32.0];
            let hit = match self.physics.as_mut() {
                Some(p) => p.trace(loc, end, [0.0; 3]),
                None => continue,
            };
            let Some(hit) = hit else { continue };
            // Rest only on an upward-facing (walkable) surface; a ceiling/steep face is skipped.
            if hit.normal[2] < 0.7 {
                continue;
            }
            let new_z = hit.location[2] + h;
            if (new_z - loc[2]).abs() > 0.01 {
                self.set_property(id, "Location", 0, Value::Vector([loc[0], loc[1], new_z]));
                settled += 1;
            }
        }
        settled
    }

    /// `Actor.Destroy`: runs `Destroyed`, then marks the object deleted so later references act
    /// as `None` and it leaves iterators. Idempotent; a nested `Destroy` during `Destroyed` is
    /// ignored (the object is already marked), so it cannot recurse or panic.
    pub fn destroy(&mut self, id: ObjectId) -> VmResult<bool> {
        if self.objects.get(id as usize).is_none_or(|o| o.deleted) {
            return Ok(true);
        }
        self.objects[id as usize].deleted = true;
        self.objects[id as usize].active = false;
        self.objects[id as usize].state = None;
        self.objects[id as usize].state_code = None;
        self.objects[id as usize].timers = [None, None, None];
        self.objects[id as usize].generation += 1;
        if let Some(f) = self.find_function(id, "Destroyed", true) {
            self.call_values(f, id, Vec::new())?;
        }
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
        let mut blocked_actor = false;
        if self.bool_prop(id, "bCollideActors")
            && let Some((t, _)) = self.sweep_blocking_actor(id, start, end)
        {
            end = lerp3(start, end, t);
            blocked_actor = true;
        }
        self.set_property(id, "Location", 0, Value::Vector(end));
        self.refresh_touching(id, true)?;
        Ok(!world_hit && !blocked_actor)
    }

    /// `Actor.SetLocation`: teleport when the destination is free of world geometry and not
    /// encroached by a blocking actor; returns whether it moved. Touch relations are updated.
    pub(crate) fn vm_set_location(&mut self, id: ObjectId, location: [f32; 3]) -> VmResult<bool> {
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
        if self.bool_prop(id, "bCollideActors") {
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
        self.set_property(id, "Location", 0, Value::Vector(location));
        self.refresh_touching(id, false)?;
        Ok(true)
    }

    /// World-only actor trace for `Actor.Trace` when `bTraceActors` is set. Returns the
    /// nearest hit as `(time, actor, normal)`; grown cylinders approximate the extent box.
    fn trace_actors(
        &self,
        id: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> Option<(f32, ObjectId, [f32; 3])> {
        let mut best: Option<(f32, ObjectId, [f32; 3])> = None;
        // UE2 selects actor hits by the trace extent: a zero-extent (line) trace needs
        // `bBlockZeroExtentTraces`, a swept box needs `bBlockNonZeroExtentTraces`. A pawn may have
        // `bCollideActors=false` yet still block hitscan traces (measured: `BaseSoldier6`), so the
        // extent flag is the correct gate here.
        let nonzero = extent[0] + extent[1] + extent[2] > 0.0;
        for b in 0..self.objects.len() as ObjectId {
            if b == id || !self.is_live_actor(b) {
                continue;
            }
            let gate = if nonzero {
                self.bool_prop(b, "bBlockNonZeroExtentTraces")
            } else {
                self.bool_prop(b, "bBlockZeroExtentTraces")
            };
            // The engine's actor-trace also reaches actors in the collision list (`bCollideActors`)
            // even when they do not set the extent flag (a traced pawn may clear `bCollideActors`
            // but still block a hitscan through `bBlockZeroExtentTraces`); accept either.
            if !gate && !self.bool_prop(b, "bCollideActors") {
                continue;
            }
            // Skip actors in the tracer's owner chain (upstream TraceFirstHit IsOwnedBy).
            if self.is_owned_by(id, b) {
                continue;
            }
            let (lb, rb, hb) = self.actor_cylinder(b);
            if let Some((t, n)) = segment_cylinder_hit(
                start,
                end,
                lb,
                rb + extent[0].max(0.0),
                hb + extent[2].max(0.0),
            ) && best.is_none_or(|(bt, _, _)| t <= bt)
            {
                best = Some((t, b, n));
            }
        }
        best
    }

    /// `Actor.Trace`: nearest of world (provider) and, when `bTraceActors`, actor cylinders;
    /// world hits return the map's `LevelInfo` (upstream), no hit returns `None`.
    /// Fills `(hit_actor, hit_location, hit_normal)`.
    #[allow(clippy::type_complexity)]
    pub(crate) fn vm_trace(
        &mut self,
        id: ObjectId,
        start: [f32; 3],
        end: [f32; 3],
        b_trace_actors: bool,
        extent: [f32; 3],
    ) -> VmResult<(Option<ObjectId>, [f32; 3], [f32; 3])> {
        let world = match self.physics.as_mut() {
            Some(p) => p.trace(start, end, extent),
            None => {
                return Err(self.err(VmErrorKind::NoPhysicsProvider {
                    native: "Actor.Trace".into(),
                }));
            }
        };
        let mut best: Option<(f32, Option<ObjectId>, [f32; 3])> =
            world.map(|h| (h.time, None, h.normal));
        if b_trace_actors
            && let Some((t, b, n)) = self.trace_actors(id, start, end, extent)
            && best.is_none_or(|(bt, _, _)| t <= bt)
        {
            best = Some((t, Some(b), n));
        }
        let out = match best {
            Some((t, Some(b), n)) => (Some(b), lerp3(start, end, t), n),
            Some((t, None, n)) => (self.find_level_info(), lerp3(start, end, t), n),
            None => (None, end, [0.0, 0.0, 0.0]),
        };
        // item14: record the hit zone for `Actor.GetLastTraceBone` (`XIIIPawn.LastBoneHit`).
        // A world/LevelInfo hit is not a pawn, so the bone stays `None`.
        //
        // item14b: when the installed provider holds the target's posed decoded skeleton, the
        // bullet ray is intersected with the per-bone hit boxes (`ray_bone`) and the nearest box's
        // bone name wins; otherwise the collision-cylinder classification is the fallback. The
        // ray is the exact trace segment, not the hit point, because a body's boxes can be
        // smaller than the cylinder.
        self.last_trace_bone = match out.0 {
            Some(b) if !self.is_a(b, "levelinfo") => self
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
        Ok(out)
    }

    /// `Actor.FastTrace`: world-only line trace; true when clear.
    pub(crate) fn vm_fast_trace(&mut self, start: [f32; 3], end: [f32; 3]) -> VmResult<bool> {
        match self.physics.as_mut() {
            Some(p) => Ok(p.trace(start, end, [0.0; 3]).is_none()),
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

    /// `Controller.LineOfSightTo`: world line trace between the controller pawn's eye and
    /// `other`'s eye. Actor occlusion is not modelled; the no-provider case fails explicitly.
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
        let (Some(pawn), Some(other_pawn)) = (self.obj_prop(controller, "Pawn"), Some(other))
        else {
            return Ok(false);
        };
        let a = self.eye_location(pawn);
        let b = self.eye_location(other_pawn);
        let hit = self.physics.as_mut().and_then(|p| p.trace(a, b, [0.0; 3]));
        Ok(hit.is_none())
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
        let speed = if speed > 0.0 {
            speed
        } else {
            self.f32_prop(pawn, "GroundSpeed")
        };
        let distance = horizontal_distance(loc, destination);
        let travel = if speed > 0.0 { distance / speed } else { 0.0 };
        // Upstream MoveTo's `MoveTimer` fail-safe: give the pawn twice the nominal travel time
        // plus a second before the latent ends even if it is stuck.
        let budget = travel * 2.0 + 1.0;
        self.set_property(controller, "Destination", 0, Value::Vector(destination));
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

    /// One tick of a `Latent::Move`: move the pawn toward its destination using the world
    /// provider, returning whether it arrived. Extracted so tests can step the movement without
    /// a full state frame.
    pub(crate) fn move_pawn_step(
        &mut self,
        pawn: ObjectId,
        destination: [f32; 3],
        speed: f32,
        dt: f32,
    ) -> VmResult<bool> {
        let location = self.vector_prop(pawn, "Location").unwrap_or(destination);
        let radius = self.f32_prop(pawn, "CollisionRadius");
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
        let end = match self.physics.as_mut() {
            Some(p) => p.move_box(location, delta, extent).end,
            None => add3(location, delta),
        };
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
    /// returning the first hit. `Ok(None)` = not found anywhere (unknown sequence); a provider
    /// decode failure is returned as [`VmErrorKind::AnimationDataError`].
    fn sequence_info(&mut self, id: ObjectId, sequence: &str) -> VmResult<Option<SeqInfo>> {
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
                    Ok(Some(info)) => return Ok(Some(info)),
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
        self.objects[id as usize].anim.blend_params.insert(
            stage,
            AnimBlendParams {
                blend_alpha,
                in_time,
                out_time,
                bone_name,
            },
        );
    }

    /// `Actor.PlayAnim`/`LoopAnim`/`TweenAnim`: start `sequence` on `channel`. `rate <= 0`
    /// falls back to the provider's rate; `tween_time` holds the sequence at frame 0 before it
    /// advances. The `None` sequence stops the channel. Unknown sequences are an explicit error.
    pub(crate) fn start_animation(
        &mut self,
        id: ObjectId,
        sequence: &str,
        rate: f32,
        tween_time: f32,
        channel: u8,
        looping: bool,
    ) -> VmResult<()> {
        if sequence.eq_ignore_ascii_case("None") {
            self.objects[id as usize].anim.channels.remove(&channel);
            self.set_property(id, "AnimSequence", 0, Value::Name("None".into()));
            self.set_property(id, "AnimRate", 0, Value::Float(0.0));
            self.set_property(id, "AnimFrame", 0, Value::Float(0.0));
            return Ok(());
        }
        if self.animation.is_none() {
            return Err(self.err(VmErrorKind::NoAnimationProvider {
                native: "Actor.PlayAnim".into(),
            }));
        }
        // UE2 `AActor::PlayAnim` returns immediately when `Mesh == NULL` (Engine.dll
        // `?PlayAnim@AActor` RVA 0xDF8B0: it tests the mesh pointer at `this+0x138` and jumps
        // straight to the epilogue, logging, when it is null). An actor with no animation
        // source therefore no-ops instead of failing. The diagnostic `FixedAnimation` provider
        // still answers the empty source, so harness diagnostics are unaffected.
        let mesh_less = self.animation_sources(id).is_empty();
        let Some(info) = self.sequence_info(id, sequence)? else {
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
        let rate = if rate > 0.0 { rate } else { info.rate };
        let mut notifies = info.notifies;
        notifies.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        self.objects[id as usize].anim.channels.insert(
            channel,
            AnimChannel {
                sequence: sequence.to_owned(),
                frames: info.frames,
                rate,
                frame: 0.0,
                looping,
                active: info.frames > 0,
                tween_remaining: tween_time.max(0.0),
                notifies,
                notify_idx: 0,
            },
        );
        self.set_property(id, "AnimSequence", 0, Value::Name(sequence.to_owned()));
        self.set_property(id, "AnimRate", 0, Value::Float(rate));
        self.set_property(id, "AnimFrame", 0, Value::Float(0.0));
        self.set_property(id, "bAnimFinished", 0, Value::Bool(false));
        Ok(())
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

    /// `(frame, rate)` of `channel`'s current animation, if the channel exists.
    pub(crate) fn anim_channel_params(&self, id: ObjectId, channel: u8) -> Option<(f32, f32)> {
        self.objects
            .get(id as usize)
            .and_then(|o| o.anim.channels.get(&channel))
            .map(|c| (c.frame, c.rate))
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
        // The actor name is only needed when a notify or animation end fires; the old code
        // cloned it unconditionally, allocating a string for every active actor every tick
        // (most have no active channel).
        let mut notifies: Vec<(u8, String)> = Vec::new();
        let mut ended: Vec<(u8, f32)> = Vec::new();
        {
            let o = &mut self.objects[id as usize];
            for (&channel, st) in o.anim.channels.iter_mut() {
                if !st.active {
                    continue;
                }
                let step_dt = if st.tween_remaining > 0.0 {
                    st.tween_remaining -= dt;
                    if st.tween_remaining > 0.0 {
                        continue;
                    }
                    -st.tween_remaining
                } else {
                    dt
                };
                let old = st.frame;
                st.frame += (st.rate * step_dt).max(0.0);
                while st.notify_idx < st.notifies.len() {
                    let (t, name) = st.notifies[st.notify_idx].clone();
                    let target = t.clamp(0.0, 1.0) * st.frames as f32;
                    if old < target && st.frame >= target {
                        notifies.push((channel, name));
                        st.notify_idx += 1;
                    } else if old >= target {
                        st.notify_idx += 1;
                    } else {
                        break;
                    }
                }
                if st.frames > 0 && st.frame + 1e-4 >= st.frames as f32 {
                    if st.looping {
                        st.frame %= st.frames as f32;
                        st.notify_idx = 0;
                    } else {
                        st.frame = st.frames as f32;
                        st.active = false;
                        ended.push((channel, st.frame));
                    }
                }
            }
        }
        for (channel, function) in notifies {
            let actor = self.objects[id as usize].name.clone();
            self.note(TraceKind::AnimNotify {
                actor,
                function: function.clone(),
                channel,
            });
            self.send_event(id, &function, Vec::new())?;
        }
        for (channel, frame) in ended {
            self.set_property(id, "AnimFrame", 0, Value::Float(frame));
            self.set_property(id, "bAnimFinished", 0, Value::Bool(true));
            let actor = self.objects[id as usize].name.clone();
            self.note(TraceKind::AnimEnd { actor, channel });
            self.send_event(id, "AnimEnd", vec![Value::Int(i32::from(channel))])?;
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

    /// Deterministic PRNG step (splitmix64). Used by `Rand`/`FRand`; the engine's own RNG
    /// sequence is not reproduced (see the registry status).
    pub(crate) fn next_random(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// `FRand`: a deterministic float in `[0, 1)`.
    pub(crate) fn rand_float(&mut self) -> f32 {
        (self.next_random() >> 40) as f32 / (1u64 << 24) as f32
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
