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
    /// the lazily built zero-extent sub-world of that actor's own collision triangles plus its
    /// per-source one-sidedness flags, for `WorldPhysics::actor_mesh_hit`. `None` caches "no
    /// mesh data" so the VM's cylinder fallback stays in charge; registered movers always cache
    /// `None` (their triangles live in the moving objects).
    actor_meshes: HashMap<String, Option<(CollisionWorld, Vec<bool>)>>,
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
        physics.source_names = scene.collision_sources.clone();
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
                return ActorMeshHit::NoData;
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
                Some((t, n)) => ActorMeshHit::Hit(WorldHit {
                    location: bevy_to_unreal_position([
                        s[0] + d[0] * t,
                        s[1] + d[1] * t,
                        s[2] + d[2] * t,
                    ]),
                    normal: bevy_to_unreal_direction(n),
                    time: t,
                }),
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
        let point = [
            s[0] + (e[0] - s[0]) * hit.t,
            s[1] + (e[1] - s[1]) * hit.t,
            s[2] + (e[2] - s[2]) * hit.t,
        ];
        ActorMeshHit::Hit(WorldHit {
            location: bevy_to_unreal_position(point),
            normal: bevy_to_unreal_direction(hit.normal),
            time: hit.t,
        })
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
        // The mover's triangles must exist in exactly one place (the same invariant the host's
        // `movers::partition` applies to the player world): drop the static base-pose copies from
        // both query soups, otherwise every pawn move/trace keeps hitting a frozen copy of the
        // closed brush after the mover has left its base pose (item27k blocker B-1).
        self.box_world.remove_static_source(source);
        self.line_world.remove_static_source(source);
        let bevy: Vec<Triangle> = triangles.iter().map(|t| t.map(to_bevy_position)).collect();
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
                bevy, source, center, rows,
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
}
