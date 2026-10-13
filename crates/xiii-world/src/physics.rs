//! Unreal-space physics adapter over the imported collision soup.
//!
//! [`WorldPhysicsAdapter`] implements `xiii_script::physics::WorldPhysics`, whose coordinates
//! are Unreal units and axes (X forward, Y right, **Z up**). `xiii-collision` works in Bevy
//! metres (Y up), so every crossing converts with the single coordinate policy in
//! `xiii_decode::common` — the scale constant and axis mapping are never duplicated here.
//!
//! The adapter is deliberately not wired into the VM (`xiii-tool`) yet; it is unit-tested
//! with a synthetic soup and, opt-in, against the imported Plage01 map.

use std::collections::HashMap;

use xiii_collision::{
    CollisionWorld, MovingObject, Triangle, Vec3 as BevyVec3, WalkParams, walk_move,
};
use xiii_decode::common::{
    UNREAL_UNITS_PER_METER, to_bevy_direction, to_bevy_position, to_bevy_scale,
};
use xiii_script::physics::{ActorMeshHit, MoveOutcome, OverlapRecord, WorldHit, WorldPhysics};

/// Rotation-matrix rows in Bevy space of an Unreal rotator (roll X / pitch Y / yaw Z), the
/// axis-permutation conjugate `P R P^-1` of `FRotationMatrix`. Shared by the moving-brush
/// collision in `xiii-app`.
pub fn rotation_rows(rot: [i32; 3]) -> [[f32; 3]; 3] {
    let to_rad = |u: i32| (u as f32) * std::f32::consts::TAU / 65536.0;
    let (p, y, rl) = (to_rad(rot[0]), to_rad(rot[1]), to_rad(rot[2]));
    let (sp, cp) = (p.sin(), p.cos());
    let (sy, cy) = (y.sin(), y.cos());
    let (sr, cr) = (rl.sin(), rl.cos());
    let bx = [cp * cy, cp * sy, sp];
    let by = [sr * sp * cy - cr * sy, sr * sp * sy + cr * cy, -sr * cp];
    let bz = [-(cr * sp * cy + sr * sy), cy * sr - cr * sp * sy, cr * cp];
    let cb = [
        to_bevy_direction(bx),
        to_bevy_direction(by),
        to_bevy_direction(bz),
    ];
    let col = [cb[1], cb[2], [-cb[0][0], -cb[0][1], -cb[0][2]]];
    [
        [col[0][0], col[1][0], col[2][0]],
        [col[0][1], col[1][1], col[2][1]],
        [col[0][2], col[1][2], col[2][2]],
    ]
}

/// Converts a Bevy-space position (metres) back into Unreal space: the inverse of
/// `xiii_decode::common::to_bevy_position`, using the same scale constant.
pub fn bevy_to_unreal_position(b: BevyVec3) -> [f32; 3] {
    let s = UNREAL_UNITS_PER_METER;
    [-b[2] * s, b[0] * s, b[1] * s]
}

/// Converts a Bevy-space direction (unit or not) back into Unreal axes: the inverse of
/// `xiii_decode::common::to_bevy_direction`.
pub fn bevy_to_unreal_direction(b: BevyVec3) -> [f32; 3] {
    [-b[2], b[0], b[1]]
}

/// Converts an Unreal half-extent into the Bevy-space half-extent in metres: the axis
/// permutation of `to_bevy_scale` divided by the same scale constant.
pub fn unreal_extent_to_bevy(e: [f32; 3]) -> BevyVec3 {
    let s = 1.0 / UNREAL_UNITS_PER_METER;
    let p = to_bevy_scale(e);
    [p[0] * s, p[1] * s, p[2] * s]
}

/// A [`WorldPhysics`] provider backed by the imported collision triangles.
///
/// The provider is stateless across queries. It holds two soups: extent (box) queries use the
/// mesh's `UseSimpleBoxCollision` selection, zero-extent (line/ray) queries its
/// `UseSimpleLineCollision` selection (see [`crate::WorldScene`]).
pub struct WorldPhysicsAdapter {
    box_world: CollisionWorld,
    line_world: CollisionWorld,
    /// Per collision source: whether its engine collision faces reject a ray from their back
    /// side (static-mesh sources, `UStaticMesh::LineCheck`).
    line_one_sided_sources: Vec<bool>,
    /// Per registered mover: `(box moving-object index, line moving-object index)`.
    movers: Vec<(usize, usize)>,
    mover_by_name: HashMap<String, usize>,
    /// Collision-source labels (`"<actor> -> <mesh>"`) by source id, for the overlap dump.
    source_names: Vec<String>,
    /// Registered mover actor names by their collision source id (reverse of `mover_by_name`).
    mover_names_by_source: HashMap<u32, String>,
    /// item53: per placed-mesh actor (lowercased name prefix of a `"<actor> -> <mesh>"` source),
    /// the decoded zero-extent sub-world plus per-source one-sidedness flags, for
    /// `WorldPhysics::actor_mesh_hit`. Scene construction includes non-blocking and empty
    /// products; entry-only adapters build this lazily. `None` means missing mesh data.
    /// Registered movers query moving objects first, retaining this base-pose line product
    /// for registration independently of the movement hull.
    actor_meshes: HashMap<String, Option<(CollisionWorld, Vec<bool>)>>,
}

/// UStaticMesh::LineCheck 0x104030e0..0x1040314d pulls the reported hit back
/// by min(0.1 / |End-Start|, 0.1) in trace-time units, then clamps to [0,1].
/// Endpoints are Unreal units: using the presentation scale changes the clearance.
fn static_mesh_line_hit(start: [f32; 3], end: [f32; 3], time: f32, normal: [f32; 3]) -> WorldHit {
    let d = [end[0] - start[0], end[1] - start[1], end[2] - start[2]];
    let length = (d[0] * d[0] + d[1] * d[1] + d[2] * d[2]).sqrt();
    let backoff = if length > 0.0 {
        (0.1 / length).min(0.1)
    } else {
        0.1
    };
    let time = (time - backoff).clamp(0.0, 1.0);
    // 0x104031c0..0x1040321d flips a normal pointing along the trace.
    let normal = if d[0] * normal[0] + d[1] * normal[1] + d[2] * normal[2] > 0.0 {
        [-normal[0], -normal[1], -normal[2]]
    } else {
        normal
    };
    WorldHit {
        location: [
            start[0] + d[0] * time,
            start[1] + d[1] * time,
            start[2] + d[2] * time,
        ],
        normal,
        time,
    }
}

impl WorldPhysicsAdapter {
    /// Builds the adapter from the extent-query and zero-extent-query `(triangle, source id)`
    /// pairs in Bevy space (metres) produced by the importer.
    pub fn from_entries(
        box_entries: impl IntoIterator<Item = (Triangle, u32)>,
        line_entries: impl IntoIterator<Item = (Triangle, u32)>,
    ) -> Self {
        Self {
            box_world: CollisionWorld::new(box_entries),
            line_world: CollisionWorld::new(line_entries),
            line_one_sided_sources: Vec::new(),
            movers: Vec::new(),
            mover_by_name: HashMap::new(),
            source_names: Vec::new(),
            mover_names_by_source: HashMap::new(),
            actor_meshes: HashMap::new(),
        }
    }

    fn from_scene_with_line_sidedness(scene: &crate::WorldScene) -> Self {
        let mut physics = Self::from_entries(scene.box_collision(), scene.line_collision());
        physics.line_one_sided_sources = scene
            .collision_sources
            .iter()
            .map(|path| path.contains(" -> "))
            .collect();
        // The flags drive the zero-extent ray path only; swept movement queries are
        // two-sided for every source. Registered movers clear their flag (brush collision
        // is two-sided, see `register_mover`).
        physics
            .box_world
            .set_one_sided_sources(physics.line_one_sided_sources.clone());
        physics.source_names = scene.collision_sources.clone();
        for (name, triangles) in &scene.actor_line_primitives {
            physics.actor_meshes.insert(
                name.clone(),
                Some((
                    CollisionWorld::new(triangles.iter().copied().map(|tri| (tri, 0))),
                    vec![true],
                )),
            );
        }
        physics
    }

    /// Number of movers registered with [`WorldPhysics::register_mover`].
    pub fn mover_count(&self) -> usize {
        self.movers.len()
    }

    /// Builds the adapter from an imported world's query-specific collision soups.
    pub fn from_scene(scene: &crate::WorldScene) -> Self {
        Self::from_scene_with_line_sidedness(scene)
    }

    /// Test/diagnostic helper: overrides the collision-source labels (the map path sets them
    /// from the importer's `"<actor> -> <mesh>"` labels).
    #[cfg(test)]
    fn with_source_names(mut self, names: &[&str]) -> Self {
        self.source_names = names.iter().map(|s| (*s).to_owned()).collect();
        self
    }

    /// The extent-query (box) collision world (for diagnostics and tests).
    pub fn box_world(&self) -> &CollisionWorld {
        &self.box_world
    }

    /// The zero-extent-query (line) collision world (for diagnostics and tests).
    pub fn line_world(&self) -> &CollisionWorld {
        &self.line_world
    }

    /// Shared implementation of [`WorldPhysics::trace`] and [`WorldPhysics::move_box`]: a
    /// swept (or zero-extent ray) query in Unreal space, returned as a [`WorldHit`].
    fn swept_hit(&self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        self.swept_hit_source(start, end, extent)
            .map(|(hit, _)| hit)
    }

    /// [`Self::swept_hit`] plus the collision source id of the hit triangle.
    fn swept_hit_source(
        &self,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> Option<(WorldHit, u32)> {
        let s = to_bevy_position(start);
        let e = to_bevy_position(end);
        let half = unreal_extent_to_bevy(extent);
        let zero = half.iter().all(|x| x.abs() < 1e-9);
        let hit = if zero {
            self.line_world
                .ray_with_one_sided_sources(s, e, &self.line_one_sided_sources)
        } else {
            self.box_world.sweep(s, e, half)
        }?;
        let t = hit.t;
        let point = [
            s[0] + (e[0] - s[0]) * t,
            s[1] + (e[1] - s[1]) * t,
            s[2] + (e[2] - s[2]) * t,
        ];
        Some((
            WorldHit {
                location: bevy_to_unreal_position(point),
                normal: bevy_to_unreal_direction(hit.normal),
                time: t,
            },
            hit.source,
        ))
    }
}

impl WorldPhysics for WorldPhysicsAdapter {
    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        self.swept_hit(start, end, extent)
    }

    fn trace_with_mover(
        &mut self,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> (Option<WorldHit>, Option<String>) {
        match self.swept_hit_source(start, end, extent) {
            Some((hit, source)) => {
                // A registered (moving) mover by its name; otherwise the actor part of the static
                // `"<actor> -> <mesh>"` source label (a mover still at its base pose keeps its
                // triangles in the static soup until it first moves). The VM decides whether the
                // named actor is a mover that blocks the trace.
                let actor = self
                    .mover_names_by_source
                    .get(&source)
                    .cloned()
                    .or_else(|| {
                        self.source_names
                            .get(source as usize)
                            .and_then(|label| label.split_once(" -> "))
                            .map(|(actor, _)| actor.to_owned())
                    });
                (Some(hit), actor)
            }
            None => (None, None),
        }
    }

    fn mover_is_registered(&self, actor: &str) -> bool {
        self.mover_by_name.contains_key(&actor.to_ascii_lowercase())
    }

    fn actor_mesh_hit(&mut self, actor: &str, start: [f32; 3], end: [f32; 3]) -> ActorMeshHit {
        let key = actor.to_ascii_lowercase();
        // item54: a registered mover's collision triangles live in its moving object (the static
        // base-pose copy is dropped at registration), so the per-actor ray cast must query that
        // object's current pose. The nominal class-default cylinder (the crawl grille
        // BreakableMover13's r/h=160 spans the whole tunnel) is not the mover's collision shape;
        // Engine.dll dispatches the per-actor line check through the actor's own primitive
        // virtual (see vm.rs item52 note) unless bUseCylinderCollision is set.
        if let Some(&i) = self.mover_by_name.get(&key)
            && let Some(&(_, line_index)) = self.movers.get(i)
            && let Some(object) = self.line_world.moving(line_index)
        {
            if object.triangle_count() == 0 {
                // A decoded empty collision set is a known miss, not missing mesh data.
                // Retrying the class-default cylinder invents geometry (e.g. light fixtures).
                return ActorMeshHit::Miss;
            }
            let s = to_bevy_position(start);
            let e = to_bevy_position(end);
            let d = [e[0] - s[0], e[1] - s[1], e[2] - s[2]];
            let mut best: Option<(f32, [f32; 3])> = None;
            for tri in object.world_triangles() {
                // Segment-bound: a hit beyond `end` is not a candidate (the nominal 160-cylinder
                // spans far past the crosshair segment).
                if let Some(t) = crate::ray_triangle(s, d, &tri).filter(|t| (0.0..=1.0).contains(t))
                {
                    let e1 = [
                        tri[1][0] - tri[0][0],
                        tri[1][1] - tri[0][1],
                        tri[1][2] - tri[0][2],
                    ];
                    let e2 = [
                        tri[2][0] - tri[0][0],
                        tri[2][1] - tri[0][1],
                        tri[2][2] - tri[0][2],
                    ];
                    let mut n = [
                        e1[1] * e2[2] - e1[2] * e2[1],
                        e1[2] * e2[0] - e1[0] * e2[2],
                        e1[0] * e2[1] - e1[1] * e2[0],
                    ];
                    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
                    if len > 1e-9 {
                        n = [n[0] / len, n[1] / len, n[2] / len];
                    }
                    if best.is_none_or(|(bt, _)| t < bt) {
                        best = Some((t, n));
                    }
                }
            }
            return match best {
                Some((t, n)) => ActorMeshHit::Hit(static_mesh_line_hit(
                    start,
                    end,
                    t,
                    bevy_to_unreal_direction(n),
                )),
                None => ActorMeshHit::Miss,
            };
        }
        if !self.actor_meshes.contains_key(&key) {
            // A registered mover's triangles live in the moving objects (its static base-pose
            // copies were dropped at registration), so the per-actor static cache must stay
            // empty for it.
            let is_registered_mover = self.mover_by_name.contains_key(&key);
            let sub = if is_registered_mover {
                None
            } else {
                let mut tris = Vec::new();
                for (sid, name) in self.source_names.iter().enumerate() {
                    let Some((owner, _)) = name.split_once(" -> ") else {
                        continue;
                    };
                    if !owner.eq_ignore_ascii_case(actor) {
                        continue;
                    }
                    let source = sid as u32;
                    for i in 0..self.line_world.triangle_count() {
                        let i = i as u32;
                        if self.line_world.source(i) == source {
                            tris.push((*self.line_world.triangle(i), source));
                        }
                    }
                }
                (!tris.is_empty()).then(|| {
                    let one_sided = self.line_one_sided_sources.clone();
                    (CollisionWorld::new(tris), one_sided)
                })
            };
            self.actor_meshes.insert(key.clone(), sub);
        }
        let Some(Some((world, one_sided))) = self.actor_meshes.get(&key) else {
            return ActorMeshHit::NoData;
        };
        let s = to_bevy_position(start);
        let e = to_bevy_position(end);
        let Some(hit) = world.ray_with_one_sided_sources(s, e, one_sided) else {
            return ActorMeshHit::Miss;
        };
        ActorMeshHit::Hit(static_mesh_line_hit(
            start,
            end,
            hit.t,
            bevy_to_unreal_direction(hit.normal),
        ))
    }

    fn move_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        let end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        match self.swept_hit(start, end, extent) {
            Some(hit) => MoveOutcome {
                end: [
                    start[0] + delta[0] * hit.time,
                    start[1] + delta[1] * hit.time,
                    start[2] + delta[2] * hit.time,
                ],
                hit: Some(hit),
            },
            None => MoveOutcome { end, hit: None },
        }
    }

    fn walk_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        let center = to_bevy_position(start);
        let end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        let end_bevy = to_bevy_position(end);
        let movement = [
            end_bevy[0] - center[0],
            end_bevy[1] - center[1],
            end_bevy[2] - center[2],
        ];
        let half = unreal_extent_to_bevy(extent);
        let params = WalkParams {
            max_step_height: 35.0 / UNREAL_UNITS_PER_METER,
            ..WalkParams::default()
        };
        let result = walk_move(&self.box_world, center, movement, half, &params);
        let hit = result.contacts.first().map(|contact| WorldHit {
            location: bevy_to_unreal_position(contact.position),
            normal: bevy_to_unreal_direction(contact.normal),
            time: contact.t,
        });
        MoveOutcome {
            end: bevy_to_unreal_position(result.position),
            hit,
        }
    }

    fn point_free(&mut self, location: [f32; 3], extent: [f32; 3]) -> bool {
        let center = to_bevy_position(location);
        let half = unreal_extent_to_bevy(extent);
        let zero = half.iter().all(|x| x.abs() < 1e-9);
        let overlaps = if zero {
            self.line_world.overlaps_aabb(center, half)
        } else {
            self.box_world.overlaps_aabb(center, half)
        };
        !overlaps
    }

    fn dump_overlap(&mut self, location: [f32; 3], extent: [f32; 3]) -> Vec<OverlapRecord> {
        let center = to_bevy_position(location);
        let half = unreal_extent_to_bevy(extent);
        if half.iter().all(|x| x.abs() < 1e-9) {
            return Vec::new();
        }
        let name = |source: u32| -> String {
            if let Some(mover) = self.mover_names_by_source.get(&source) {
                mover.clone()
            } else {
                self.source_names
                    .get(source as usize)
                    .cloned()
                    .unwrap_or_else(|| format!("<source {source}>"))
            }
        };
        let mut out = Vec::new();
        for hit in self.box_world.overlap_aabb(center, half) {
            if hit.triangle == u32::MAX {
                // A moving-object hit: `Hit` carries only the source, so re-test that mover's
                // world triangles against the box and report each overlapping one.
                let mover = self.movers.iter().find_map(|(b, _)| {
                    self.box_world
                        .moving(*b)
                        .filter(|m| m.source() == hit.source)
                });
                if let Some(m) = mover {
                    for tri in m.world_triangles() {
                        if xiii_collision::aabb_overlaps_triangle(center, half, &tri) {
                            out.push(OverlapRecord {
                                kind: "moving",
                                source: name(hit.source),
                                triangle: tri.map(bevy_to_unreal_position),
                            });
                        }
                    }
                }
            } else {
                out.push(OverlapRecord {
                    kind: "static",
                    source: name(hit.source),
                    triangle: (*self.box_world.triangle(hit.triangle)).map(bevy_to_unreal_position),
                });
            }
        }
        out
    }

    fn register_mover(
        &mut self,
        actor: &str,
        source: u32,
        triangles: &[[[f32; 3]; 3]],
        origin: [f32; 3],
        rotation: [i32; 3],
    ) {
        if triangles.is_empty() {
            return;
        }
        // Movers are brushes in the engine: their collision (the mover brush polys) is
        // two-sided. The one-sided `UStaticMesh::LineCheck` tail applies to static-mesh
        // actors only, so a registered mover's source must not keep the static-mesh
        // one-sided flag (measured: the chair swing must break BreakAbleMover16 from the
        // room side, and the fist must break BreakableMover9 from zone 15 - both hit the
        // grille from the side its mesh winding points away from).
        self.box_world.set_one_sided_source(source, false);
        self.line_world.set_one_sided_source(source, false);
        if let Some(f) = self.line_one_sided_sources.get_mut(source as usize) {
            *f = false;
        }
        // The mover's triangles must exist in exactly one place (the same invariant the host's
        // `movers::partition` applies to the player world): drop the static base-pose copies from
        // both query soups, otherwise every pawn move/trace keeps hitting a frozen copy of the
        // closed brush after the mover has left its base pose (item27k blocker B-1).
        self.box_world.remove_static_source(source);
        self.line_world.remove_static_source(source);
        let bevy: Vec<Triangle> = triangles.iter().map(|t| t.map(to_bevy_position)).collect();
        // The registration input is the movement (box) set. Keep the separately decoded
        // line set; using the simplified box hull here screens interaction rays.
        let line_bevy = self
            .actor_meshes
            .get(&actor.to_ascii_lowercase())
            .and_then(|entry| entry.as_ref())
            .map(|(world, _)| {
                (0..world.triangle_count())
                    .map(|i| *world.triangle(i as u32))
                    .collect()
            })
            .unwrap_or_else(|| bevy.clone());
        let center = to_bevy_position(origin);
        let rows = rotation_rows(rotation);
        let box_index = self
            .box_world
            .add_moving(MovingObject::from_world_triangles(
                bevy.clone(),
                source,
                center,
                rows,
            ));
        let line_index = self
            .line_world
            .add_moving(MovingObject::from_world_triangles(
                line_bevy, source, center, rows,
            ));
        self.mover_names_by_source
            .insert(source, actor.to_ascii_lowercase());
        self.mover_by_name
            .insert(actor.to_ascii_lowercase(), self.movers.len());
        self.movers.push((box_index, line_index));
    }

    fn set_mover_collision(&mut self, actor: &str, enabled: bool) {
        let Some(&i) = self.mover_by_name.get(&actor.to_ascii_lowercase()) else {
            return;
        };
        let (box_index, line_index) = self.movers[i];
        self.box_world.set_moving_enabled(box_index, enabled);
        self.line_world.set_moving_enabled(line_index, enabled);
    }

    fn set_mover(&mut self, actor: &str, location: [f32; 3], rotation: [i32; 3]) {
        let Some(&i) = self.mover_by_name.get(&actor.to_ascii_lowercase()) else {
            return;
        };
        let center = to_bevy_position(location);
        let rows = rotation_rows(rotation);
        let (box_index, line_index) = self.movers[i];
        self.box_world.set_moving_transform(box_index, center, rows);
        self.line_world
            .set_moving_transform(line_index, center, rows);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PackageCache, import_map};
    use xiii_decode::common::to_bevy_direction;

    fn close(a: [f32; 3], b: [f32; 3], eps: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < eps)
    }

    #[test]
    fn position_and_direction_round_trip() {
        let s = UNREAL_UNITS_PER_METER;
        let u = [123.0, -45.5, 678.25];
        let b = to_bevy_position(u);
        assert!(close(bevy_to_unreal_position(b), u, 1e-3));
        // Bevy (x,y,z) = (u.y, u.z, -u.x)/s.
        assert!(close(b, [u[1] / s, u[2] / s, -u[0] / s], 1e-6));
        let dir = [0.0, 1.0, 0.0];
        assert!(close(
            bevy_to_unreal_direction(to_bevy_direction(dir)),
            dir,
            1e-6
        ));
        // Extent permutation: (x,y,z)/s -> (y,z,x)/s, magnitudes preserved.
        let e = [2.0, 3.0, 5.0];
        assert!(close(
            unreal_extent_to_bevy(e),
            [e[1] / s, e[2] / s, e[0] / s],
            1e-6
        ));
    }

    /// A 20x20 m floor at Bevy Y = 0 plus a 0.5 m tall wall at Bevy X = -5, in Bevy space.
    fn tiny_soup() -> Vec<(Triangle, u32)> {
        let floor = vec![
            (
                [[-10.0, 0.0, -10.0], [10.0, 0.0, -10.0], [10.0, 0.0, 10.0]],
                0,
            ),
            (
                [[-10.0, 0.0, -10.0], [10.0, 0.0, 10.0], [-10.0, 0.0, 10.0]],
                0,
            ),
        ];
        // Wall at x = -5, spanning z in [-1,1], y in [0,0.5].
        let wall = vec![
            ([[-5.0, 0.0, -1.0], [-5.0, 0.5, -1.0], [-5.0, 0.5, 1.0]], 1),
            ([[-5.0, 0.0, -1.0], [-5.0, 0.5, 1.0], [-5.0, 0.0, 1.0]], 1),
        ];
        floor.into_iter().chain(wall).collect()
    }

    /// Unreal coordinates of a Bevy point (via the adapter's public inverse).
    fn u(p: [f32; 3]) -> [f32; 3] {
        bevy_to_unreal_position(p)
    }

    #[test]
    fn registered_mover_rays_hit_from_both_sides() {
        // A mover is a brush in the engine: two-sided. Even when the one-sided flag was set
        // from the static-mesh label rule, registering the mover must exempt its source so
        // traces (the chair swing breaking BreakAbleMover16 from the room side) hit it.
        let tri: Triangle = [
            [16.0 / 90.0, 8.0 / 90.0, 2175.0 / 90.0],
            [16.0 / 90.0, 126.0 / 90.0, 2175.0 / 90.0],
            [140.0 / 90.0, 126.0 / 90.0, 2175.0 / 90.0],
        ];
        let mut p = WorldPhysicsAdapter::from_entries(vec![(tri, 133)], vec![(tri, 133)]);
        // Simulate the static-mesh label rule flagging this source one-sided (both the
        // worlds' sweep flags and the adapter's ray flag array).
        p.line_world.set_one_sided_source(133, true);
        p.box_world.set_one_sided_source(133, true);
        p.line_one_sided_sources = vec![false; 134];
        p.line_one_sided_sources[133] = true;
        let tri_unreal: Vec<Triangle> = vec![tri.map(bevy_to_unreal_position)];
        p.register_mover(
            "BreakableMover9",
            133,
            &tri_unreal,
            [-2175.0, 78.0, 67.0],
            [0, 0, 0],
        );
        // Unreal-space rays eastward (along the winding) and westward (against it) must
        // both hit the registered brush.
        let east = p.trace(
            [-2260.0, 78.0, 100.0],
            [-2160.0, 78.0, 100.0],
            [0.0, 0.0, 0.0],
        );
        let west = p.trace(
            [-2100.0, 78.0, 100.0],
            [-2190.0, 78.0, 100.0],
            [0.0, 0.0, 0.0],
        );
        assert!(east.is_some(), "eastward ray must hit the registered brush");
        assert!(west.is_some(), "westward ray must hit the registered brush");
    }

    #[test]
    fn decoded_empty_mover_mesh_is_a_miss_not_missing_data() {
        let mut scene = crate::WorldScene::default();
        scene
            .actor_line_primitives
            .insert("empty".into(), Vec::new());
        let mut p = WorldPhysicsAdapter::from_scene_with_line_sidedness(&scene);
        assert!(matches!(
            p.actor_mesh_hit("Empty", [-10.0, 0.0, 0.0], [10.0, 0.0, 0.0]),
            ActorMeshHit::Miss
        ));
        assert!(matches!(
            p.actor_mesh_hit("Empty", [0.0; 3], [0.0; 3]),
            ActorMeshHit::Miss
        ));
        assert!(matches!(
            p.actor_mesh_hit("Unknown", [0.0; 3], [0.0; 3]),
            ActorMeshHit::NoData
        ));
    }

    #[test]
    fn static_mesh_line_hit_backoff_preserves_unreal_clearance_and_boundaries() {
        let hit = static_mesh_line_hit([0.0; 3], [160.0, 0.0, 0.0], 0.5, [-1.0, 0.0, 0.0]);
        assert!((hit.location[0] - 79.9).abs() < 1e-5);
        let short = static_mesh_line_hit([0.0; 3], [0.5, 0.0, 0.0], 0.5, [-1.0, 0.0, 0.0]);
        assert!((short.location[0] - 0.2).abs() < 1e-6);
        let touching = static_mesh_line_hit([0.0; 3], [160.0, 0.0, 0.0], 0.0, [-1.0, 0.0, 0.0]);
        assert_eq!(touching.time, 0.0);
        assert_eq!(touching.location, [0.0; 3]);
        let backface = static_mesh_line_hit([0.0; 3], [160.0, 0.0, 0.0], 0.5, [1.0, 0.0, 0.0]);
        assert_eq!(backface.normal, [-1.0, 0.0, 0.0]);
        let degenerate = static_mesh_line_hit([1.0; 3], [1.0; 3], 0.0, [0.0; 3]);
        assert_eq!(degenerate.location, [1.0; 3]);
    }

    #[test]
    fn registered_mover_retains_line_set_instead_of_box_hull() {
        let box_tri = [[0.0, -10.0, -10.0], [0.0, 10.0, -10.0], [0.0, 0.0, 10.0]];
        let line_tri = [
            [100.0, -10.0, -10.0],
            [100.0, 10.0, -10.0],
            [100.0, 0.0, 10.0],
        ];
        let mut scene = crate::WorldScene::default();
        scene
            .actor_line_primitives
            .insert("door".into(), vec![line_tri.map(to_bevy_position)]);
        let mut p = WorldPhysicsAdapter::from_scene_with_line_sidedness(&scene);
        p.register_mover("Door", 0, &[box_tri], [0.0; 3], [0; 3]);
        assert!(matches!(
            p.actor_mesh_hit("Door", [-50.0, 0.0, 0.0], [50.0, 0.0, 0.0]),
            ActorMeshHit::Miss
        ));
        let ActorMeshHit::Hit(hit) = p.actor_mesh_hit("Door", [-50.0, 0.0, 0.0], [150.0, 0.0, 0.0])
        else {
            panic!("line set hit");
        };
        assert!((hit.location[0] - 99.9).abs() < 1e-4);
        p.set_mover("Door", [50.0, 0.0, 0.0], [0; 3]);
        assert!(matches!(
            p.actor_mesh_hit("Door", [-50.0, 0.0, 0.0], [120.0, 0.0, 0.0]),
            ActorMeshHit::Miss
        ));
    }

    #[test]
    fn register_mover_removes_the_stale_static_base_pose_copy() {
        // item27k blocker B-1: the adapter used to keep a mover's base-pose triangles in the
        // static soups AND add them as moving objects, so a pawn kept hitting a frozen closed
        // brush after the mover had left its base pose. `register_mover` must drop the static
        // copies; the brush must then block only where it currently is.
        // A slab across the corridor in Unreal space: x in [-100,100], y in [-10,10],
        // z in [0,200] (a 6-face closed box), collision source id 3.
        let slab = |source: u32| -> Vec<(Triangle, u32)> {
            let c = [
                [-100.0, -10.0, 0.0],
                [100.0, -10.0, 0.0],
                [100.0, 10.0, 0.0],
                [-100.0, 10.0, 0.0],
                [-100.0, -10.0, 200.0],
                [100.0, -10.0, 200.0],
                [100.0, 10.0, 200.0],
                [-100.0, 10.0, 200.0],
            ];
            let f = [
                [0, 3, 2],
                [0, 2, 1],
                [4, 5, 6],
                [4, 6, 7],
                [0, 1, 5],
                [0, 5, 4],
                [2, 3, 7],
                [2, 7, 6],
                [1, 2, 6],
                [1, 6, 5],
                [0, 4, 7],
                [0, 7, 3],
            ];
            f.map(|t| (t.map(|i| to_bevy_position(c[i])), source))
                .to_vec()
        };
        let world_tris: Vec<[[[f32; 3]; 3]; 12]> = vec![];
        let _ = world_tris;
        let slab_unreal: Vec<Triangle> = slab(3)
            .iter()
            .map(|(t, _)| t.map(bevy_to_unreal_position))
            .collect();
        let mut p = WorldPhysicsAdapter::from_entries(slab(3), slab(3));
        let start = [0.0, -225.0, 100.0];
        let extent = [34.0, 34.0, 75.0];
        let delta = [0.0, 450.0, 0.0];
        // Before registration the static base-pose copy blocks the corridor.
        assert!(
            p.move_box(start, delta, extent).hit.is_some(),
            "the static slab must block before registration"
        );
        p.register_mover("XIIIMover14", 3, &slab_unreal, [0.0, 0.0, 0.0], [0, 0, 0]);
        // Still blocked: the moving object sits at the same base pose.
        assert!(
            p.move_box(start, delta, extent).hit.is_some(),
            "the brush at its base pose must still block"
        );
        // After the brush leaves its base pose the corridor must be clear: with the stale
        // static copy this used to stay blocked forever (B-1).
        p.set_mover("XIIIMover14", [0.0, 0.0, 1000.0], [0, 0, 0]);
        let clear = p.move_box(start, delta, extent);
        assert!(
            clear.hit.is_none(),
            "stale static copy still blocks: {clear:?}"
        );
        // The brush blocks where it currently is.
        p.set_mover("XIIIMover14", [0.0, 100.0, 0.0], [0, 0, 0]);
        let moved = p.move_box(start, delta, extent);
        assert!(moved.hit.is_some(), "the brush at its new pose must block");
        // The brush's near face is now at y = 90; the pawn's half-extent is 34.
        assert!(moved.end[1] < 56.0 + 1.0, "{moved:?}");
    }

    #[test]
    fn mover_trace_names_the_mover_and_disabled_collision_clears_it() {
        // item40e: a world hit on a registered mover's geometry names that mover (the VM then
        // returns the mover actor, as UE2 traces return collision-hash actors), and a mover
        // whose actor was destroyed or stopped colliding no longer blocks traces or boxes.
        let mut p = WorldPhysicsAdapter::from_entries(
            Vec::<(Triangle, u32)>::new(),
            Vec::<(Triangle, u32)>::new(),
        );
        let wall_u: Vec<[[f32; 3]; 3]> = vec![
            [
                [0.0, -100.0, -100.0],
                [0.0, 100.0, -100.0],
                [0.0, 100.0, 100.0],
            ],
            [
                [0.0, -100.0, -100.0],
                [0.0, 100.0, 100.0],
                [0.0, -100.0, 100.0],
            ],
        ];
        p.register_mover("Grille", 4, &wall_u, [0.0; 3], [0, 0, 0]);
        let (start, end) = ([-200.0, 0.0, 0.0], [200.0, 0.0, 0.0]);
        let (hit, actor) = p.trace_with_mover(start, end, [0.0; 3]);
        assert!(hit.is_some());
        assert_eq!(actor.as_deref(), Some("grille"));
        p.set_mover_collision("Grille", false);
        assert_eq!(p.trace_with_mover(start, end, [0.0; 3]), (None, None));
        assert!(p.point_free([0.0; 3], [10.0, 10.0, 10.0]));
        p.set_mover_collision("grille", true);
        assert!(p.trace(start, end, [5.0, 5.0, 5.0]).is_some());
        // Unknown names are ignored, not a panic.
        p.set_mover_collision("NoSuchMover", false);
        assert!(p.trace(start, end, [0.0; 3]).is_some());
    }

    #[test]
    fn trace_down_hits_floor_at_expected_unreal_height() {
        let mut p = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup());
        // Bevy floor y=0 => Unreal z=0. Start Bevy y=4 m and go to y=-1 m.
        let start = u([0.0, 4.0, 0.0]);
        let end = u([0.0, -1.0, 0.0]);
        let hit = p.trace(start, end, [0.0; 3]).expect("downward trace hits");
        assert!(hit.location[2].abs() < 1e-2, "z={}", hit.location[2]);
        assert!(hit.normal[2] > 0.9, "normal={:?}", hit.normal);
        // 4 m down over a 5 m segment: t = 0.8.
        assert!((hit.time - 0.8).abs() < 1e-3, "{hit:?}");
    }

    #[test]
    fn trace_with_extent_stops_at_wall() {
        let mut p = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup());
        // Move a 0.1 m half-extent box from Bevy x=-4 to x=-6 into the wall at x=-5.
        let start = u([-4.0, 0.25, 0.0]);
        let end = u([-6.0, 0.25, 0.0]);
        let hit = p
            .trace(
                start,
                end,
                [0.1 * xiii_decode::common::UNREAL_UNITS_PER_METER; 3],
            )
            .expect("moving into the wall with an extent must hit");
        assert!(hit.time < 1.0, "{hit:?}");
        // Contact when the box's leading face reaches the wall: center Bevy x = -4.9 => t=0.45.
        assert!((hit.time - 0.45).abs() < 0.02, "t={}", hit.time);
    }

    #[test]
    fn move_box_stops_at_first_block_and_ticks_leave_free_space() {
        let mut p = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup());
        let start = u([-4.0, 0.25, 0.0]);
        let delta = u([-2.0, 0.0, 0.0]);
        let out = p.move_box(start, delta, [10.0, 10.0, 10.0]);
        let hit = out.hit.expect("move_box into the wall must report a hit");
        assert!(hit.time < 1.0);
        let full_end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        assert!(!close(out.end, full_end, 1e-6), "cut short");
        // A move on the open half of the floor is unobstructed.
        let free_start = u([0.0, 4.0, 0.0]);
        let free_delta = u([1.0, 0.0, 0.0]);
        let free = p.move_box(free_start, free_delta, [0.0; 3]);
        assert!(free.hit.is_none(), "{free:?}");
        assert_eq!(
            free.end,
            [
                free_start[0] + free_delta[0],
                free_start[1] + free_delta[1],
                free_start[2] + free_delta[2]
            ]
        );
        // Point-free: in the air above the floor yes, inside the wall no.
        assert!(p.point_free(u([0.0, 4.0, 0.0]), [10.0, 10.0, 10.0]));
        assert!(!p.point_free(u([-5.0, 0.25, 0.0]), [10.0, 10.0, 10.0]));
    }

    #[test]
    fn walk_box_steps_up_within_engine_step_height_and_rejects_higher_steps() {
        // Floor at Bevy Y=0; a 0.3 m ledge is below 35 UU (0.3889 m), while a 0.5 m
        // ledge is above it. Each ledge is represented by its top and vertical face.
        let ledge = |height: f32| -> Vec<(Triangle, u32)> {
            vec![
                ([[-2.0, 0.0, -2.0], [0.5, 0.0, -2.0], [0.5, 0.0, 2.0]], 0),
                ([[-2.0, 0.0, -2.0], [0.5, 0.0, 2.0], [-2.0, 0.0, 2.0]], 0),
                (
                    [[0.5, 0.0, -2.0], [0.5, height, -2.0], [0.5, height, 2.0]],
                    1,
                ),
                ([[0.5, 0.0, -2.0], [0.5, height, 2.0], [0.5, 0.0, 2.0]], 1),
                (
                    [[0.5, height, -2.0], [4.0, height, -2.0], [4.0, height, 2.0]],
                    1,
                ),
                (
                    [[0.5, height, -2.0], [4.0, height, 2.0], [0.5, height, 2.0]],
                    1,
                ),
            ]
        };
        let s = UNREAL_UNITS_PER_METER;
        let start = u([-1.0, 0.9, 0.0]);
        let extent = [0.2 * s, 0.2 * s, 0.9 * s];
        let delta = [0.0, 2.0 * s, 0.0];
        let mut low = WorldPhysicsAdapter::from_entries(ledge(0.3), Vec::new());
        let stepped = low.walk_box(start, delta, extent);
        assert!(stepped.end[1] > start[1] + 1.0 * s, "{stepped:?}");
        assert!(stepped.end[2] >= 0.3 * s - 2.0, "{stepped:?}");

        let mut high = WorldPhysicsAdapter::from_entries(ledge(0.5), Vec::new());
        let blocked = high.walk_box(start, delta, extent);
        assert!(blocked.end[1] < start[1] + 1.5 * s, "{blocked:?}");
    }

    #[test]
    fn extent_uses_box_soup_and_zero_extent_uses_line_soup() {
        // Two walls at Bevy x = -5: the box soup has a tall one (y in [0,2]), the line soup a
        // short one (y in [0,0.5]). A query at y = 1.0 distinguishes them.
        let box_soup = vec![
            (
                [[-5.0, 0.0, -1.0], [-5.0, 2.0, -1.0], [-5.0, 2.0, 1.0]],
                0u32,
            ),
            (
                [[-5.0, 0.0, -1.0], [-5.0, 2.0, 1.0], [-5.0, 0.0, 1.0]],
                0u32,
            ),
        ];
        let line_soup = vec![
            (
                [[-5.0, 0.0, -1.0], [-5.0, 0.5, -1.0], [-5.0, 0.5, 1.0]],
                1u32,
            ),
            (
                [[-5.0, 0.0, -1.0], [-5.0, 0.5, 1.0], [-5.0, 0.0, 1.0]],
                1u32,
            ),
        ];
        let mut p = WorldPhysicsAdapter::from_entries(box_soup, line_soup);
        let start = u([-4.0, 1.0, 0.0]);
        let end = u([-6.0, 1.0, 0.0]);
        // Zero extent: the short line wall does not reach y = 1.0, and there is no floor.
        assert!(
            p.trace(start, end, [0.0; 3]).is_none(),
            "a zero-extent query must not see the box soup's tall wall"
        );
        // Extent trace: the tall box wall blocks.
        let hit = p
            .trace(start, end, [0.1 * UNREAL_UNITS_PER_METER; 3])
            .expect("an extent query must see the box soup's tall wall");
        assert!(hit.time < 1.0, "{hit:?}");
        // The line world is the short one; the box world the tall one (diagnostics).
        assert_eq!(p.line_world().triangle_count(), 2);
        assert_eq!(p.box_world().triangle_count(), 2);
    }

    #[test]
    fn registered_mover_blocks_then_moving_it_clears_a_trace() {
        // Empty static soup: the only geometry is the registered mover.
        let mut p = WorldPhysicsAdapter::from_entries(
            Vec::<(Triangle, u32)>::new(),
            Vec::<(Triangle, u32)>::new(),
        );
        // A vertical wall in Unreal at X=0, registered as a mover `Door`.
        let wall_u: Vec<[[f32; 3]; 3]> = vec![
            [
                [0.0, -100.0, -100.0],
                [0.0, 100.0, -100.0],
                [0.0, 100.0, 100.0],
            ],
            [
                [0.0, -100.0, -100.0],
                [0.0, 100.0, 100.0],
                [0.0, -100.0, 100.0],
            ],
        ];
        p.register_mover("Door", 7, &wall_u, [0.0, 0.0, 0.0], [0, 0, 0]);
        assert_eq!(p.mover_count(), 1);
        let start = [-200.0, 0.0, 0.0];
        let end = [200.0, 0.0, 0.0];
        let hit = p
            .trace(start, end, [0.0; 3])
            .expect("the registered mover must block a zero-extent trace");
        assert!(hit.time < 1.0, "{hit:?}");
        // A box query sees it too.
        assert!(!p.point_free([0.0, 0.0, 0.0], [10.0, 10.0, 10.0]));
        // Move it away: both queries clear.
        p.set_mover("Door", [100_000.0, 0.0, 0.0], [0, 0, 0]);
        assert!(p.trace(start, end, [0.0; 3]).is_none());
        assert!(p.point_free([0.0, 0.0, 0.0], [10.0, 10.0, 10.0]));
    }

    #[test]
    fn actor_mesh_hit_answers_from_the_placed_mesh_source_and_misses_the_opening() {
        // item53: a wall panel in the Unreal YZ plane at x=40 with a window band
        // (z in [60,120]) removed, placed as source "PorteDecors18 -> statitoit.Tportet3".
        // The provider must answer the actor's own mesh: a ray through the opening misses,
        // a ray through the panel hits, an unknown actor has no data.
        let panel = |z0: f32, z1: f32| -> Vec<(Triangle, u32)> {
            let c = [
                [40.0, -50.0, z0],
                [40.0, 50.0, z0],
                [40.0, 50.0, z1],
                [40.0, -50.0, z1],
            ];
            vec![
                ([c[0], c[1], c[2]].map(to_bevy_position), 0),
                ([c[0], c[2], c[3]].map(to_bevy_position), 0),
            ]
        };
        let soup = [panel(-100.0, 60.0), panel(120.0, 400.0)].concat();
        let mut p = WorldPhysicsAdapter::from_entries(soup.clone(), soup)
            .with_source_names(&["PorteDecors18 -> statitoit.Tportet3"]);
        let start = [0.0, 0.0, 90.0];
        let end = [400.0, 0.0, 90.0];
        assert_eq!(
            p.actor_mesh_hit("PorteDecors18", start, end),
            ActorMeshHit::Miss,
            "the ray through the window opening must miss the door's mesh"
        );
        let start = [0.0, 0.0, 0.0];
        let ActorMeshHit::Hit(world) = p.actor_mesh_hit("PorteDecors18", start, end) else {
            panic!("the ray through the panel must hit");
        };
        assert!((world.location[0] - 40.0).abs() < 0.5, "{world:?}");
        assert_eq!(
            p.actor_mesh_hit("NoSuchActor", start, end),
            ActorMeshHit::NoData
        );
    }

    #[test]
    fn opt_in_plage01_player_start_trace_hits_floor() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        // A relative value is resolved against the workspace root (the test CWD is the crate
        // directory), so `XIII_GOG_DIR=XIII_Game cargo test` works from anywhere.
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        let scene = import_map(&mut cache, "Plage01").expect("import Plage01");
        let (ps_bevy, _rot) = scene.player_start.expect("Plage01 has a PlayerStart");
        let mut p = WorldPhysicsAdapter::from_scene(&scene);
        // The PlayerStart is at Unreal Z 1196.0; trace down from 100 UU above the floor.
        let ps = bevy_to_unreal_position(ps_bevy);
        let hit = p
            .trace(ps, [ps[0], ps[1], ps[2] - 200.0], [0.0; 3])
            .expect("a downward trace from the PlayerStart must hit the floor");
        println!(
            "Plage01 PlayerStart {:?} -> floor trace hit at {:?} (normal {:?}, time {:.3})",
            ps, hit.location, hit.normal, hit.time
        );
        // Measured (item1-collision/item1c): floor 12 UU below => Unreal Z = 1184.0.
        assert!(
            (hit.location[2] - 1184.0).abs() < 2.0,
            "floor Z {} (expected ~1184)",
            hit.location[2]
        );
        assert!(hit.normal[2] > 0.9, "floor normal {:?}", hit.normal);
    }

    /// Count of a lift cage's triangles lying in one horizontal plane (Unreal Z), per soup.
    ///
    /// item96 regression: a mover's mesh whose simplified collision set is absent takes the
    /// per-triangle set for BOTH query kinds; a future importer change that drops those
    /// triangles (or a partition that misses the mover) leaves the cage without a floor and
    /// the ride carries nobody. The plane test is rotation-invariant (yaw/pitch keep Z).
    fn cage_floor_planes(scene: &crate::WorldScene, line_soup: bool, z_unreal: f32) -> usize {
        use xiii_decode::common::UNREAL_UNITS_PER_METER;
        let entries: Vec<([[f32; 3]; 3], u32)> = if line_soup {
            scene.line_collision().collect()
        } else {
            scene.box_collision().collect()
        };
        entries
            .into_iter()
            .filter(|(tri, source)| {
                scene
                    .collision_sources
                    .get(*source as usize)
                    .is_some_and(|s| s.starts_with("LiftCage0 -> "))
                    && tri
                        .iter()
                        .all(|v| (v[1] * UNREAL_UNITS_PER_METER - z_unreal).abs() < 2.0)
            })
            .count()
    }

    #[test]
    fn opt_in_banque01_and_hual02_lift_cage_floors_reach_both_soups() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let mut cache = PackageCache::open(&path).expect("open install");

        // Banque01 (item96): the `Staticbanque.bankascenseur` cage (BasePos z 1027) has its
        // cabin floor slab at world Z 991 (local -36) and inner floor at 1007 (local -20).
        // Measured planes: `xiii-tool box-probe Banque01 --from 1296,-1920,400 --to
        // 1296,-1920,1200` lists tris 6125/6126 at 991 and 6132/6133/6379/6380 at 1007 under
        // `LiftCage0 -> Staticbanque.bankascenseur`.
        let scene = import_map(&mut cache, "Banque01").expect("import Banque01");
        for (z, min, what) in [(991.0, 2, "cabin floor"), (1007.0, 2, "inner floor")] {
            let (box_tris, line_tris) = (
                cage_floor_planes(&scene, false, z),
                cage_floor_planes(&scene, true, z),
            );
            println!("Banque01 cage {what} z={z}: box {box_tris}, line {line_tris}");
            assert!(
                box_tris >= min,
                "Banque01 lift cage {what} (z={z}) missing from the box soup: {box_tris}"
            );
            assert!(
                line_tris >= min,
                "Banque01 lift cage {what} (z={z}) missing from the line soup: {line_tris}"
            );
        }

        // Hual02 (item75's working ride): the `fredprivate.ascenseur` cage (BasePos z -4725)
        // HAS a simplified collision set (10 triangles), so the box soup takes the simplified
        // hull while the line soup keeps the 72-triangle per-triangle set (measured:
        // box 10 / line 72). The hull planes are local -132.1/+131.9 (world -4857/-4593); the
        // per-triangle interior floor is local -22.7 (world -4747.7) with the bottom slab at
        // -4869.3. Both selections must keep their floor planes or the ride breaks.
        let scene = import_map(&mut cache, "Hual02").expect("import Hual02");
        // Measured soup membership per plane (the hull planes exist in both sets — the
        // per-triangle set repeats the hull's slab planes at local -132.1/+131.9 — while the
        // interior planes are per-triangle only). The lock: the box soup (what a walking
        // player lands on) keeps the hull planes and keeps the interior floor OUT of it; a
        // change that put set 0 into the box soup would silently move the rider 109.4 UU up,
        // and a change that dropped the hull would remove the ride's standing surface.
        for (z, want_box, want_line, what) in [
            (-4857.0, true, true, "hull floor"),
            (-4593.0, true, true, "hull roof"),
            (-4869.3, false, true, "bottom slab"),
            (-4747.7, false, true, "interior floor"),
        ] {
            let (box_tris, line_tris) = (
                cage_floor_planes(&scene, false, z),
                cage_floor_planes(&scene, true, z),
            );
            println!("Hual02 cage {what} z={z}: box {box_tris}, line {line_tris}");
            assert_eq!(
                box_tris >= 2,
                want_box,
                "Hual02 lift cage {what} (z={z}) box-soup membership is wrong: box {box_tris}, line {line_tris}"
            );
            assert_eq!(
                line_tris >= 2,
                want_line,
                "Hual02 lift cage {what} (z={z}) line-soup membership is wrong: box {box_tris}, line {line_tris}"
            );
        }
    }
}
