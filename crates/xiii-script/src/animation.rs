//! Skeletal-animation bridge between the interpreter and decoded animation data.
//!
//! `xiii-script` stays dependency-free: the VM only knows a sequence's *length*, playback
//! rate and notify times through this trait. The real XIV `MeshAnimation` decoder
//! (`xiii-decode`) is deliberately **not** a dependency of this crate; a later task installs a
//! provider that wraps it. Coordinates and frame units are the provider's business (the VM
//! plays in frames at `rate` frames per second).
//!
//! No provider installed: every native that actually needs sequence data fails with
//! [`crate::vm::VmErrorKind::NoAnimationProvider`] — never a silent success.

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
/// unknown meshes/sequences (`None` = unknown; the VM turns that into an explicit error).
pub trait AnimationData {
    /// Sequence data for `seq` on the mesh named `mesh` (the `Mesh` object's path), or `None`.
    fn sequence(&mut self, mesh: &str, seq: &str) -> Option<SeqInfo>;
}

/// Diagnostic provider: **every** sequence exists with a fixed frame count and rate, and no
/// notifies. It is not the game data; it exists so the headless harness and diagnostics can run
/// past the animation natives (the harness labels its output "diagnostic animation, not the
/// mesh"). The real decoded `MeshAnimation` provider belongs to a later task.
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
    fn sequence(&mut self, _mesh: &str, _seq: &str) -> Option<SeqInfo> {
        Some(SeqInfo {
            frames: self.frames,
            rate: self.rate,
            notifies: Vec::new(),
        })
    }
}
