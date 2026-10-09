//! Physics-provider bridge between the interpreter and outside world collision.
//!
//! The VM keeps Unreal units and Unreal axes (X east, Y north, **Z up**); every coordinate
//! crossing this trait is Unreal. Any metre/axis conversion is the provider implementation's
//! business, never the VM's.
//!
//! No provider set: every native that needs one fails with
//! [`crate::vm::VmErrorKind::NoPhysicsProvider`] — never a silent success.

/// World-geometry hit (upstream `FCheckResult`-like fields the VM needs).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WorldHit {
    /// Hit location (Unreal units).
    pub location: [f32; 3],
    /// Surface normal at the hit (unit length when the provider can supply one).
    pub normal: [f32; 3],
    /// Fraction of the swept segment at which the hit occurred, `0.0..=1.0`.
    pub time: f32,
}

/// Result of a provider [`WorldPhysics::move_box`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveOutcome {
    /// Where the box ended up when the move was blocked (`start + delta * hit.time`).
    pub end: [f32; 3],
    /// The blocking world hit, if any. `time < 1.0` means blocked.
    pub hit: Option<WorldHit>,
}

/// One world primitive a diagnostic box-overlap dump found at a location
/// ([`WorldPhysics::dump_overlap`]). Everything is Unreal units/axes; the source label is the
/// provider's own naming (for the map adapter: `"<actor> -> <mesh>"`, or the mover actor's name).
#[derive(Debug, Clone, PartialEq)]
pub struct OverlapRecord {
    /// `"static"` (world soup) or `"moving"` (registered mover brush).
    pub kind: &'static str,
    /// Provider-specific source label.
    pub source: String,
    /// The overlapping triangle's vertices (Unreal units).
    pub triangle: [[f32; 3]; 3],
}

/// World collision the VM calls into. Implemented by the host (Bevy app, tests, diagnostics);
/// the VM only holds `Box<dyn WorldPhysics>`.
pub trait WorldPhysics {
    /// Swept **world-geometry-only** query from `start` to `end` with half-extent `extent`
    /// (a zero extent is a line in upstream terms). Returns the first blocking hit.
    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit>;

    /// [`WorldPhysics::trace`] that also names the actor whose collision geometry produced the
    /// hit, if the provider knows it (a registered mover, see [`WorldPhysics::register_mover`],
    /// or a placed mesh actor). The VM keeps the name only for movers: UE2 movers are
    /// collision-hash actors whose primitive is their own brush/static mesh, so `Actor.Trace`
    /// returns the mover itself rather than the level. Providers without per-actor sources keep
    /// the default (no actor).
    fn trace_with_mover(
        &mut self,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> (Option<WorldHit>, Option<String>) {
        (self.trace(start, end, extent), None)
    }

    /// UE2 `MoveActor`-like swept box move: from `start`, try to move by `delta` with
    /// half-extents `extent`, stopping at the **first blocking** world hit. No sliding —
    /// sliding is physics/script logic layered above this. Returns the end position and the
    /// blocking hit, if any (`MoveOutcome.time` < 1 means the move was cut short).
    fn move_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome;

    /// Pawn walking movement. Providers backed by `xiii-collision` override this with its
    /// UE2-style step-up/floor-follow primitive; the default preserves point/diagnostic providers.
    fn walk_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        self.move_box(start, delta, extent)
    }

    /// Point/free placement test: can a box of the given half-extents sit at `location`
    /// without overlapping world geometry? For `SetLocation`/spawn placement checks.
    fn point_free(&mut self, location: [f32; 3], extent: [f32; 3]) -> bool;

    /// Registers a UE2 `Mover`'s collision triangles so `trace`/`move_box`/`point_free` consider
    /// them: `triangles` are world-space at the base pose `(origin, rotation)` (Unreal units and
    /// rotator units). A provider that does not model moving brushes may ignore this (default
    /// no-op).
    fn register_mover(
        &mut self,
        _actor: &str,
        _source: u32,
        _triangles: &[[[f32; 3]; 3]],
        _origin: [f32; 3],
        _rotation: [i32; 3],
    ) {
    }

    /// Updates a registered mover's pose (Unreal units/rotators). Default no-op.
    fn set_mover(&mut self, _actor: &str, _location: [f32; 3], _rotation: [i32; 3]) {}

    /// Enables or disables a registered mover's collision (the actor was destroyed or its
    /// `bCollideActors` changed). Default no-op.
    fn set_mover_collision(&mut self, _actor: &str, _enabled: bool) {}

    /// Diagnostic (item27k): every world primitive overlapping the box `(location, extent)`,
    /// with its kind, source label and triangle. Empty by default; providers backed by a
    /// triangle soup implement it so a blocked pawn move can name what it hit.
    fn dump_overlap(&mut self, _location: [f32; 3], _extent: [f32; 3]) -> Vec<OverlapRecord> {
        Vec::new()
    }
}

/// Hit-zone resolution for `Actor.GetLastTraceBone` (item14).
///
/// The decoded `SkeletalMesh` carries a trailing per-bone hit-box array
/// (`xiii-decode::skeletal::mesh::RawSkeletalMesh::bone_boxes`), but the boxes are expressed in
/// each bone's local space and therefore require the **animated** pose to become world-space
/// volumes. The headless VM does not evaluate a pose; it only has each actor's collision
/// cylinder. This trait is the small provider seam: the default [`CylinderZones`] classifies the
/// world hit point against the target's cylinder, and a host that evaluates the decoded pose can
/// install a per-bone provider without changing the VM.
pub trait HitZones {
    /// Bone name for a world-space `hit` on an actor whose collision cylinder is
    /// `(center, radius, half_height)`. Returns a name constant (`X Head`, `X Spine1`, `X Spine`,
    /// or `None`).
    fn bone_at(&self, center: [f32; 3], radius: f32, half_height: f32, hit: [f32; 3]) -> String;

    /// Nearest decoded per-bone hit-box along the ray `start..end` on `actor`, when the provider
    /// holds a posed skeleton for it. Returns the bone name `Actor.GetLastTraceBone` should give
    /// (`X Head`, `X Spine1`, ...). `None` (the default) means the provider has no per-bone data
    /// for this actor and the caller falls back to [`HitZones::bone_at`]. The default
    /// [`CylinderZones`] never overrides it.
    fn ray_bone(&self, actor: crate::ObjectId, start: [f32; 3], end: [f32; 3]) -> Option<String> {
        let _ = (actor, start, end);
        None
    }
}

/// Default hit-zone model over the collision cylinder. XIII's `XIIIPawn.GetDamageLocation`
/// already classifies a hit by its offset from the pawn centre (head / spine / below), so the
/// only information the script's `LastBoneHit` gate needs is the vertical band. The top half of
/// the cylinder maps to `X Head`, the middle band to `X Spine1`, the rest to `X Spine`.
///
/// **Partial** (documented): this is not the decoded bone-box test; see [`HitZones`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CylinderZones;

impl HitZones for CylinderZones {
    fn bone_at(&self, center: [f32; 3], _radius: f32, half_height: f32, hit: [f32; 3]) -> String {
        if half_height <= 0.0 {
            return "None".to_owned();
        }
        let dz = hit[2] - center[2];
        if dz > half_height * 0.5 {
            "X Head".to_owned()
        } else if dz > -half_height * 0.5 {
            "X Spine1".to_owned()
        } else {
            "X Spine".to_owned()
        }
    }
}

/// Diagnostic provider: a single infinite floor plane at Unreal Z `floor_z`, nothing else.
///
/// This is **not** the map: it exists so the headless harness and diagnostics can run past
/// movement/trace natives (the harness labels its output "diagnostic physics (flat floor),
/// not the map"). The real decoded triangle soup provider belongs to a later task.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FlatPhysics {
    /// Floor height in Unreal units (Z up).
    pub floor_z: f32,
}

impl FlatPhysics {
    /// Floor plane at Unreal Z `floor_z`.
    pub fn new(floor_z: f32) -> Self {
        Self { floor_z }
    }
}

impl WorldPhysics for FlatPhysics {
    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        let ez = extent[2];
        let bottom_end = end[2] - ez;
        if start[2] - ez < self.floor_z {
            // Already below the floor: report contact at the segment start.
            return Some(WorldHit {
                location: [start[0], start[1], self.floor_z],
                normal: [0.0, 0.0, 1.0],
                time: 0.0,
            });
        }
        if end[2] < start[2] && bottom_end < self.floor_z {
            let dz = end[2] - start[2];
            let t = ((self.floor_z + ez) - start[2]) / dz;
            let t = t.clamp(0.0, 1.0);
            return Some(WorldHit {
                location: [
                    start[0] + (end[0] - start[0]) * t,
                    start[1] + (end[1] - start[1]) * t,
                    self.floor_z,
                ],
                normal: [0.0, 0.0, 1.0],
                time: t,
            });
        }
        None
    }

    fn move_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        let end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        match self.trace(start, end, extent) {
            Some(hit) => MoveOutcome {
                end: [
                    start[0] + delta[0] * hit.time,
                    start[1] + delta[1] * hit.time,
                    self.floor_z + extent[2],
                ],
                hit: Some(hit),
            },
            None => MoveOutcome { end, hit: None },
        }
    }

    fn point_free(&mut self, location: [f32; 3], extent: [f32; 3]) -> bool {
        location[2] - extent[2] >= self.floor_z
    }
}
