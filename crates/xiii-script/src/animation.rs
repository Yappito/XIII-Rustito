//! Skeletal-animation bridge between the interpreter and decoded animation data.
//!
//! `xiii-script` stays dependency-free: the VM only knows a sequence's *length*, playback
//! rate and notify times through this trait. The real XIII `MeshAnimation` decoder
//! (`xiii-decode`) is deliberately **not** a dependency of this crate; the `MapAnimationProvider`
//! in `xiii-world` wraps it. Coordinates and frame units are the provider's business (the VM
//! plays in frames at `rate` frames per second).
//!
//! A lookup is addressed by an **animation source path** rather than a single `MeshAnimation`:
//! in XIII an actor's animation comes from the `Mesh` (a `SkeletalMesh`, which carries a
//! default `MeshAnimation` reference) and from any number of `Actor.LinkSkelAnim(MeshAnimation)`
//! links. The VM passes each candidate source (linked animations in call order, then the
//! `Mesh`) in turn; a provider resolves a `SkeletalMesh` source through its decoded default
//! animation and interprets a `MeshAnimation` source directly. This is why the trait could not
//! stay a one-object `sequence(mesh, seq)` lookup.
//!
//! The return value distinguishes the two failure modes required by the task:
//! `Ok(None)` = the source or sequence is unknown (the VM decides, and reports it as
//! [`crate::vm::VmErrorKind::UnknownAnimation`]); `Err` = a decode/lookup failure that must be
//! reported explicitly and never silently treated as a missing sequence. No provider installed:
//! every native that needs sequence data fails with
//! [`crate::vm::VmErrorKind::NoAnimationProvider`].

/// Data the VM needs about one animation sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct SeqInfo {
    /// Number of frames in the sequence (0 = an empty/one-instant sequence).
    pub frames: u32,
    /// Playback rate in frames per second when the caller supplies no rate.
    pub rate: f32,
    /// Script notifies: `(time, function)` with `time` normalized to `0.0..=1.0`.
    pub notifies: Vec<(f32, String)>,
}

/// Decoded animation data the VM queries. A provider may cache and is free to be strict about
/// unknown sources/sequences (`Ok(None)` = unknown; the VM turns that into an explicit error).
pub trait AnimationData {
    /// Sequence data for `seq` on animation source `source`.
    ///
    /// `source` is an object path from the actor's script state: a `Mesh` (`SkeletalMesh`,
    /// resolved through its decoded default `MeshAnimation`) or a `LinkSkelAnim` `MeshAnimation`.
    /// `Ok(None)` = the source or the sequence is unknown; `Err(message)` = a decode or
    /// resolution failure that must be reported, never treated as an unknown sequence.
    fn sequence(&mut self, source: &str, seq: &str) -> Result<Option<SeqInfo>, String>;

    /// Mesh-space pose position of one bone, for `Actor.GetBoneCoords`.
    ///
    /// `mesh_source` is the actor's `SkeletalMesh` path (skeleton and mesh transform);
    /// `anim_source` is the animation source the playing sequence was resolved from (a linked
    /// `MeshAnimation` or the mesh itself); `seq`/`frame`/`looping` describe the channel state.
    /// The returned offset is the posed bone position relative to the actor origin in
    /// actor-rotation space: the mesh transform (`RotOrigin`, `MeshOrigin`, scale) is applied
    /// here, the caller adds the actor's own yaw and `Location` on top (the same split as the
    /// pawn renderer's `root_transform`). `Ok(None)` = the mesh, sequence or bone is unknown;
    /// `Err` = a decode failure. The default is `Ok(None)`: providers without pose data keep
    /// `GetBoneCoords` on its documented actor-origin fallback.
    fn bone_offset(
        &mut self,
        mesh_source: &str,
        anim_source: &str,
        seq: &str,
        frame: f32,
        looping: bool,
        bone: &str,
    ) -> Result<Option<[f32; 3]>, String> {
        let _ = (mesh_source, anim_source, seq, frame, looping, bone);
        Ok(None)
    }
}

/// Diagnostic provider: **every** sequence exists with a fixed frame count and rate, and no
/// notifies. It is not the game data; it exists so the headless harness and diagnostics can run
/// past the animation natives (the harness labels its output "diagnostic animation, not the
/// mesh"). The real decoded `MeshAnimation` provider lives in `xiii-world`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FixedAnimation {
    /// Frame count reported for every sequence.
    pub frames: u32,
    /// Rate (frames per second) reported for every sequence.
    pub rate: f32,
}

impl FixedAnimation {
    /// Every sequence has `frames` frames at `rate` frames per second.
    pub fn new(frames: u32, rate: f32) -> Self {
        Self { frames, rate }
    }
}

impl AnimationData for FixedAnimation {
    fn sequence(&mut self, _source: &str, _seq: &str) -> Result<Option<SeqInfo>, String> {
        Ok(Some(SeqInfo {
            frames: self.frames,
            rate: self.rate,
            notifies: Vec::new(),
        }))
    }
}
