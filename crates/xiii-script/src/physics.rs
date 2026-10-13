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

/// A blocking world-geometry contact from [`WorldPhysics::point_check`] (upstream
/// `FCheckResult`-like placement fields): where a box first touches geometry, the unit surface
/// normal and the penetration depth along that normal. Unreal units/axes (X east, Y north, Z up).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SpotHit {
    /// Contact location (Unreal units).
    pub location: [f32; 3],
    /// Unit surface normal pointing away from the geometry toward the box centre.
    pub normal: [f32; 3],
    /// Depth along `normal` (Unreal units) the box must move to separate from the surface.
    pub penetration: f32,
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

/// Result of a provider [`WorldPhysics::actor_mesh_hit`] query (item53): a placed-mesh actor's
/// own collision triangles either hold the provider's data (`NoData` — the VM falls back to the
/// cylinder approximation), miss the ray (`Miss` — the actor does not block this line, the UE2
/// kDOP behaviour), or hit (`Hit`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ActorMeshHit {
    /// The provider holds no collision triangles for this actor's placed mesh.
    NoData,
    /// The actor's mesh is traceable and the zero-extent ray misses it.
    Miss,
    /// The actor's mesh blocks the ray.
    Hit(WorldHit),
}

/// World collision the VM calls into. Implemented by the host (Bevy app, tests, diagnostics);
/// the VM only holds `Box<dyn WorldPhysics>`.
pub trait WorldPhysics {
    /// Swept **world-geometry-only** query from `start` to `end` with half-extent `extent`
    /// (a zero extent is a line in upstream terms). Returns the first blocking hit.
    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit>;

    /// Level-geometry-only swept query, matching the engine's `ULevel::SingleLineCheck` on the
    /// `FindSpot`/`CheckSlice` path, which probes the BSP `UModel` with the actors flag off. Like
    /// [`WorldPhysics::trace`] but excludes placed-actor collision (static-mesh actors and the
    /// `ATerrainInfo` terrain heightfield). Providers that do not distinguish actor geometry keep
    /// the default ([`WorldPhysics::trace`]).
    fn trace_level(
        &mut self,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> Option<WorldHit> {
        self.trace(start, end, extent)
    }

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

    /// Zone gravity at `location` for the scripted-physics integration (item53). The engine
    /// reads the enclosing `PhysicsVolume`'s `Gravity`; the VM tracks no volumes, so providers
    /// without zone data keep the measured `Engine.PhysicsVolume` class default `(0, 0, -950)`.
    fn zone_gravity(&mut self, _location: [f32; 3]) -> [f32; 3] {
        [0.0, 0.0, -950.0]
    }

    /// Zero-extent (line) trace against one named placed-mesh actor's own collision triangles
    /// (item53). UE2 traces a static-mesh actor against its kDOP triangles — not a collision
    /// cylinder — so a ray through a mesh's opening (Toits01's window in `PorteDecors18`)
    /// must not hit the actor. The provider answers from its placed-mesh soup (sources labelled
    /// `"<actor> -> <mesh>"`); [`ActorMeshHit::NoData`] (the default) means no per-actor mesh
    /// data, and the VM falls back to the cylinder approximation.
    fn actor_mesh_hit(&mut self, _actor: &str, _start: [f32; 3], _end: [f32; 3]) -> ActorMeshHit {
        ActorMeshHit::NoData
    }

    /// PointCheck against a named actor's extent-query primitive. `None` means required
    /// geometry is unavailable, distinct from a decoded empty primitive (`Some(false)`).
    fn actor_mesh_overlap(
        &mut self,
        _actor: &str,
        _center: [f32; 3],
        _extent: [f32; 3],
    ) -> Option<bool> {
        None
    }

    /// Supplies the current static-mesh identity and transform for runtime-spawned actors.
    /// Providers with pre-registered geometry may keep the default; installation-backed
    /// providers decode the selected asset on demand and report failures to the VM.
    fn prepare_actor_mesh(
        &mut self,
        _actor: &str,
        _asset: &str,
        _location: [f32; 3],
        _rotation: [i32; 3],
        _scale: [f32; 3],
        _pre_pivot: [f32; 3],
    ) -> Result<(), String> {
        Ok(())
    }

    /// Transformed UStaticMesh primitive bounds, for GetEncroachCenter/Extent.
    fn actor_mesh_bounds(&self, _actor: &str) -> Option<([f32; 3], [f32; 3])> {
        None
    }

    /// Map brush PointCheck available before actor startup/spawn callbacks run.
    fn actor_brush_overlap(
        &self,
        _actor: &str,
        _center: [f32; 3],
        _extent: [f32; 3],
    ) -> Option<bool> {
        None
    }

    /// LineCheck of a named actor's primitive with a nonzero moving extent.
    fn actor_mesh_sweep(
        &mut self,
        actor: &str,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> ActorMeshHit {
        if extent == [0.0; 3] {
            self.actor_mesh_hit(actor, start, end)
        } else {
            ActorMeshHit::NoData
        }
    }

    /// Whether the named actor is held as a registered mover (see
    /// [`WorldPhysics::register_mover`]): its collision triangles live in the moving world, so a
    /// world trace answers from the mover's real mesh. A mover that is NOT registered has no
    /// mesh the VM can reach — its per-actor mesh query returns
    /// [`ActorMeshHit::NoData`] with nothing behind it — so the VM keeps the cylinder
    /// approximation for those.
    fn mover_is_registered(&self, _actor: &str) -> bool {
        false
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

    /// Box placement check with the blocking contact (upstream `ULevel::SinglePointCheck`,
    /// `Engine.dll` vtable `+0xcc`): returns the deepest overlapping triangle's normal and
    /// penetration, or `None` when the box is free. [`WorldPhysics::find_spot`] and
    /// [`WorldPhysics::check_slice`] consume the normal; providers that only model
    /// [`WorldPhysics::point_free`] keep the conservative default below (a block with the axis-up
    /// normal and zero penetration, never a silent success).
    fn point_check(&mut self, location: [f32; 3], extent: [f32; 3]) -> Option<SpotHit> {
        if self.point_free(location, extent) {
            None
        } else {
            Some(SpotHit {
                location,
                normal: [0.0, 0.0, 1.0],
                penetration: 0.0,
            })
        }
    }

    /// UE2 `ULevel::CheckSlice` (`Engine.dll` `0x10389b80`) over `point_check`/`trace`: verify a
    /// box of half-extents `extent` centred at `location` clears the `(r, r, 1)` slice, settle it
    /// onto the surface below with a downward `(r, r, 1)` sweep of `2 * extent.z`, then retry the
    /// full extent and finally push out along the contact normal's horizontal components.
    /// Returns the adjusted centre, or `None`. A **blocked slice** sets `*encroach` so
    /// [`WorldPhysics::find_spot`] can accumulate half-radius corners. Branches carry the decoded
    /// addresses (`XIII_Game/system/Engine.dll`, image base `0x10300000`).
    fn check_slice(
        &mut self,
        location: [f32; 3],
        extent: [f32; 3],
        encroach: &mut bool,
    ) -> Option<[f32; 3]> {
        let slice = [extent[0], extent[1], 1.0];
        // 0x10389c26: SinglePointCheck(Hit, Location, (r, r, 1), 0, Level, 0).
        if self.point_check(location, slice).is_some() {
            // 0x1038a060: the blocked slice hands the search to FindSpot's corners.
            *encroach = true;
            return None;
        }
        // 0x10389c85: SingleLineCheck(Hit, NULL, Location - (0,0,2*z), Location, 0x86, (r,r,1), 0).
        // The FindSpot path probes the level only (actors flag off), so use [`Self::trace_level`]:
        // a placed-mesh or terrain surface under the box must not settle it.
        let down = [location[0], location[1], location[2] - 2.0 * extent[2]];
        let Some(hit) = self.trace_level(location, down, slice) else {
            // 0x10389e21: nothing within reach; drop by the half-height and re-validate.
            let mut loc = location;
            loc[2] -= extent[2];
            return self.point_check(loc, extent).is_none().then_some(loc);
        };
        // 0x10389d1c / 0x10389e6b: `z += (1 - 2*t) * extent.z + 1` (the 1 UU correction). The two
        // decoded hit-time branches (`t > 0.5` vs `<= 0.5`) are algebraically the same offset.
        let mut loc = [
            location[0],
            location[1],
            location[2] + (1.0 - 2.0 * hit.time) * extent[2] + 1.0,
        ];
        // 0x10389d6f: retry the full extent at the settled height.
        if self.point_check(loc, extent).is_none() {
            return Some(loc);
        }
        // 0x10389d7e: push out along the contact normal's horizontal components.
        loc[0] += hit.normal[0] * extent[0];
        loc[1] += hit.normal[1] * extent[1];
        if self.point_check(loc, extent).is_none() {
            return Some(loc);
        }
        None
    }

    /// UE2 `ULevel::FindSpot` (`Engine.dll` `0x1038a080`, vtable `+0xc0`): place a box of
    /// half-extents `extent` near `location`, returning the adjusted centre or `None` when no
    /// valid spot exists. The requested spot is used unchanged when free; otherwise
    /// [`WorldPhysics::check_slice`] settles it, and when the `(r, r, 1)` slice is blocked the
    /// four half-radius corners are searched (at most two free, one reflected through the
    /// original). The `bEncroachmentCheck`-style horizontal-search flag is the `encroach`
    /// out-parameter of `check_slice`. Branches carry the decoded addresses.
    fn find_spot(&mut self, location: [f32; 3], extent: [f32; 3]) -> Option<[f32; 3]> {
        // 0x1038a0df: SinglePointCheck of the full extent at the requested location.
        if self.point_check(location, extent).is_none() {
            return Some(location);
        }
        // 0x1038a12c: a zero extent fails.
        if extent.iter().all(|v| *v == 0.0) {
            return None;
        }
        let original = location;
        // 0x1038a1aa: CheckSlice(Location, Extent, &bEncroach).
        let mut encroach = false;
        if let Some(loc) = self.check_slice(location, extent, &mut encroach) {
            return Some(loc);
        }
        // 0x1038a1b8: only an encroaching failure allows the horizontal corner search.
        if !encroach {
            return None;
        }
        // 0x1038a1bf: the four half-radius corners, relative to the evolving location.
        let half = [0.5 * extent[0], 0.5 * extent[1], 1.0];
        let mut loc = location;
        let mut found = 0usize;
        'corners: for j in [-1.0f32, 1.0] {
            for i in [-1.0f32, 1.0] {
                if found >= 2 {
                    break 'corners;
                }
                let test = [
                    loc[0] + 0.5 * extent[0] * j,
                    loc[1] + 0.5 * extent[1] * i,
                    loc[2],
                ];
                if self.point_check(test, half).is_none() {
                    loc = test;
                    found += 1;
                }
            }
        }
        // 0x1038a2cf: the whole neighbourhood is blocked.
        if found == 0 {
            return None;
        }
        // 0x1038a2d8: `Location = 2*Location - Original` for a single free corner.
        if found == 1 {
            loc = [
                2.0 * loc[0] - original[0],
                2.0 * loc[1] - original[1],
                2.0 * loc[2] - original[2],
            ];
        }
        // 0x1038a338: re-run CheckSlice at the chosen corner.
        let mut encroach2 = false;
        let loc = self.check_slice(loc, extent, &mut encroach2)?;
        // 0x1038a346: a reflected sweep bounds the correction: a clear mirror keeps the original
        // (0x1038a3ae), a blocked one keeps the adjusted spot (0x1038a3d7).
        let mirror = [
            2.0 * original[0] - loc[0],
            2.0 * original[1] - loc[1],
            2.0 * original[2] - loc[2],
        ];
        if self.trace_level(loc, mirror, extent).is_none() {
            Some(original)
        } else {
            Some(loc)
        }
    }

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
