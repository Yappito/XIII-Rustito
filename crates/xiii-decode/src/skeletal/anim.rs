//! Raw `Engine.MeshAnimation` payload (XIII, package version 100).
//!
//! The header (`Version`, `RefBones`) and the sequence list follow UModel `UnMesh2.h`
//! (`UMeshAnimation`, `FNamedBone`, `FMeshAnimSeq`, `FMeshAnimNotify`; version < 112 so no
//! notify object, < 115 so no leading float) at gildor2/UEViewer@a0bfb468. UModel does not
//! support XIII animation: XIII's `MotionChunk` replaces the per-bone `AnalogTrack` arrays with
//! a packed form that was established from the bytes and verified on all 96 GOG instances:
//!
//! ```text
//! MotionChunk:
//!   FVector RootSpeed3D; float TrackTime; i32 StartBone; u32 Flags; TArray<i32> BoneIndices;
//!   TArray<{u16 PosStart, u16 QuatStart, u16 QuatCount, u16 PosCount}> Tracks;  // one per RefBone
//!   TArray<{i16 x, y, z, w}> Quats;   // component / 32767
//!   TArray<FVector> Positions;
//!   TArray<float> QuatTimes;         // frames; one per quat (shared by positions when PosCount > 1)
//! ```

use super::error::{SkelError, SkelErrorKind};
use super::reader::Reader;

/// `FNamedBone`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NamedBone {
    /// Bone name.
    pub name: String,
    /// Flags.
    pub flags: u32,
    /// Parent index (0 for the root).
    pub parent: i32,
}

/// Per-bone key ranges in a [`MotionChunk`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackRange {
    /// First index into [`MotionChunk::positions`].
    pub pos_start: u16,
    /// First index into [`MotionChunk::quats`] and [`MotionChunk::quat_times`].
    pub quat_start: u16,
    /// Number of rotation keys.
    pub quat_count: u16,
    /// Number of position keys (1 = constant, otherwise equal to `quat_count`).
    pub pos_count: u16,
}

/// XIII packed motion chunk (one per sequence).
#[derive(Debug, Clone, PartialEq)]
pub struct MotionChunk {
    /// Net root speed (always zero observed).
    pub root_speed: [f32; 3],
    /// Track length in frames (equals the sequence's frame count).
    pub track_time: f32,
    /// Lowest animated bone (always 0 observed).
    pub start_bone: i32,
    /// Flags (always 0 observed).
    pub flags: u32,
    /// Bone index table (identity observed).
    pub bone_indices: Vec<i32>,
    /// Key ranges, one per reference bone.
    pub tracks: Vec<TrackRange>,
    /// Packed rotation keys, `i16 / 32767` per component.
    pub quats: Vec<[i16; 4]>,
    /// Position keys.
    pub positions: Vec<[f32; 3]>,
    /// Rotation key times in frames.
    pub quat_times: Vec<f32>,
}

/// `FMeshAnimNotify` (version 100: time and function name only).
#[derive(Debug, Clone, PartialEq)]
pub struct AnimNotify {
    /// Normalized time (0..1 expected).
    pub time: f32,
    /// Function name.
    pub function: String,
}

/// `FMeshAnimSeq`.
#[derive(Debug, Clone, PartialEq)]
pub struct AnimSeq {
    /// Sequence name.
    pub name: String,
    /// Groups.
    pub groups: Vec<String>,
    /// Start frame (always 0 observed).
    pub start_frame: i32,
    /// Frame count.
    pub num_frames: i32,
    /// Notifies.
    pub notifies: Vec<AnimNotify>,
    /// Playback rate in frames per second.
    pub rate: f32,
}

/// Decoded `Engine.MeshAnimation` payload.
#[derive(Debug, Clone, PartialEq)]
pub struct RawMeshAnimation {
    /// `Version` (always 0 observed).
    pub version: i32,
    /// Animated skeleton.
    pub ref_bones: Vec<NamedBone>,
    /// Motion chunks; `moves[i]` belongs to `sequences[i]`.
    pub moves: Vec<MotionChunk>,
    /// Sequences.
    pub sequences: Vec<AnimSeq>,
}

fn motion_chunk(r: &mut Reader<'_>) -> Result<MotionChunk, SkelError> {
    let root_speed = r.vec3("move.root_speed")?;
    let track_time = r.f32("move.track_time")?;
    let start_bone = r.i32("move.start_bone")?;
    let flags = r.u32("move.flags")?;
    let bone_indices = r.array("move.bone_indices", 4, |r| r.i32("move.bone_indices"))?;
    let tracks_pos = r.pos();
    let tracks = r.array("move.tracks", 8, |r| {
        Ok(TrackRange {
            pos_start: r.u16("move.tracks")?,
            quat_start: r.u16("move.tracks")?,
            quat_count: r.u16("move.tracks")?,
            pos_count: r.u16("move.tracks")?,
        })
    })?;
    let quats = r.array("move.quats", 8, |r| {
        Ok([
            r.i16("move.quats")?,
            r.i16("move.quats")?,
            r.i16("move.quats")?,
            r.i16("move.quats")?,
        ])
    })?;
    let positions = r.array("move.positions", 12, |r| r.vec3("move.positions"))?;
    let times_pos = r.pos();
    let quat_times = r.array("move.quat_times", 4, |r| r.f32("move.quat_times"))?;
    if quat_times.len() != quats.len() {
        return Err(r.err_at(
            "move.quat_times",
            times_pos,
            SkelErrorKind::Invalid(format!(
                "{} key times for {} rotation keys",
                quat_times.len(),
                quats.len()
            )),
        ));
    }
    for (i, t) in tracks.iter().enumerate() {
        let q_end = usize::from(t.quat_start) + usize::from(t.quat_count);
        let p_end = usize::from(t.pos_start) + usize::from(t.pos_count);
        if q_end > quats.len() || p_end > positions.len() {
            return Err(r.err_at(
                "move.tracks",
                tracks_pos,
                SkelErrorKind::Invalid(format!(
                    "track {i} keys out of range (quats {}..{q_end} of {}, positions {}..{p_end} of {})",
                    t.quat_start,
                    quats.len(),
                    t.pos_start,
                    positions.len()
                )),
            ));
        }
    }
    Ok(MotionChunk {
        root_speed,
        track_time,
        start_bone,
        flags,
        bone_indices,
        tracks,
        quats,
        positions,
        quat_times,
    })
}

/// Decodes the class-native part of a MeshAnimation payload (after the property block).
pub(crate) fn read(r: &mut Reader<'_>) -> Result<RawMeshAnimation, SkelError> {
    let version = r.i32("anim.version")?;
    let ref_bones = r.array("anim.ref_bones", 9, |r| {
        Ok(NamedBone {
            name: r.name("anim.ref_bones.name")?,
            flags: r.u32("anim.ref_bones.flags")?,
            parent: r.i32("anim.ref_bones.parent")?,
        })
    })?;
    // Minimum chunk: 24 fixed bytes + five one-byte array counts.
    let moves = r.array("anim.moves", 29, motion_chunk)?;
    let sequences = r.array("anim.sequences", 15, |r| {
        let name = r.name("seq.name")?;
        let groups = r.array("seq.groups", 1, |r| r.name("seq.groups"))?;
        let start_frame = r.i32("seq.start_frame")?;
        let num_frames = r.i32("seq.num_frames")?;
        let notifies = r.array("seq.notifies", 5, |r| {
            Ok(AnimNotify {
                time: r.f32("seq.notify.time")?,
                function: r.name("seq.notify.function")?,
            })
        })?;
        let rate = r.f32("seq.rate")?;
        Ok(AnimSeq {
            name,
            groups,
            start_frame,
            num_frames,
            notifies,
            rate,
        })
    })?;
    r.finish("end")?;
    Ok(RawMeshAnimation {
        version,
        ref_bones,
        moves,
        sequences,
    })
}
