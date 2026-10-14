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
use xiii_script::physics::{
    ActorMeshHit, MoveOutcome, OverlapRecord, SpotHit, WorldHit, WorldPhysics,
};

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
    /// Lazy level-only soups (the BSP, with actor-mesh/terrain sources removed) for
    /// [`WorldPhysics::trace_level`], built on first use. Invalidated when a mover registers so a
    /// removed base-pose copy is not retained.
    level_worlds: Option<(CollisionWorld, CollisionWorld)>,
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
    actor_point_meshes: HashMap<String, CollisionWorld>,
    mesh_cache: Option<crate::PackageCache>,
    mesh_assets: HashMap<String, MeshCollisionAsset>,
    dynamic_mesh_poses: HashMap<String, MeshPose>,
    actor_mesh_bounds: HashMap<String, ([f32; 3], [f32; 3])>,
    actor_brushes: HashMap<String, xiii_script::brush::BrushPrimitive>,
}

struct MeshCollisionAsset {
    point: Vec<Triangle>,
    line: Vec<Triangle>,
    bounds: xiii_collision::Aabb,
}

#[derive(Clone, PartialEq)]
struct MeshPose {
    asset: String,
    location: [f32; 3],
    rotation: [i32; 3],
    scale: [f32; 3],
    pre_pivot: [f32; 3],
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

/// AABB-vs-triangle penetration for [`WorldPhysics::point_check`]: the depth along the triangle's
/// unit plane normal (oriented from the plane toward the box centre) that separates the box, and
/// that normal. `None` for a degenerate triangle or a box that does not penetrate the plane.
fn aabb_triangle_penetration(
    center: BevyVec3,
    half: BevyVec3,
    tri: &Triangle,
) -> Option<(f32, [f32; 3])> {
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
    if len <= 1e-9 {
        return None;
    }
    n = [n[0] / len, n[1] / len, n[2] / len];
    let d = n[0] * (center[0] - tri[0][0])
        + n[1] * (center[1] - tri[0][1])
        + n[2] * (center[2] - tri[0][2]);
    let support = half[0] * n[0].abs() + half[1] * n[1].abs() + half[2] * n[2].abs();
    let depth = support - d.abs();
    if depth <= 1e-9 {
        return None;
    }
    let s = if d < 0.0 { -1.0 } else { 1.0 };
    Some((depth, [n[0] * s, n[1] * s, n[2] * s]))
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
            level_worlds: None,
            line_one_sided_sources: Vec::new(),
            movers: Vec::new(),
            mover_by_name: HashMap::new(),
            source_names: Vec::new(),
            mover_names_by_source: HashMap::new(),
            actor_meshes: HashMap::new(),
            actor_point_meshes: HashMap::new(),
            mesh_cache: None,
            mesh_assets: HashMap::new(),
            dynamic_mesh_poses: HashMap::new(),
            actor_mesh_bounds: HashMap::new(),
            actor_brushes: HashMap::new(),
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
        for (name, triangles) in &scene.actor_point_primitives {
            physics.actor_point_meshes.insert(
                name.clone(),
                CollisionWorld::new(triangles.iter().copied().map(|tri| (tri, 0))),
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

    /// Installation-backed adapter that can import runtime-spawned mesh primitives.
    pub fn from_scene_and_install(
        scene: &crate::WorldScene,
        game_dir: &std::path::Path,
        map: &str,
    ) -> Result<Self, String> {
        let mut physics = Self::from_scene(scene);
        physics.mesh_cache = Some(crate::PackageCache::open(game_dir)?);
        for brush in crate::volume_brushes::import(game_dir, map)?.brushes {
            physics
                .actor_brushes
                .insert(brush.name.to_ascii_lowercase(), brush.primitive);
        }
        Ok(physics)
    }

    /// Test/diagnostic helper: overrides the collision-source labels (the map path sets them
    /// from the importer's `"<actor> -> <mesh>"` labels).
    #[cfg(test)]
    fn with_source_names(mut self, names: &[&str]) -> Self {
        self.source_names = names.iter().map(|s| (*s).to_owned()).collect();
        self.level_worlds = None;
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

    /// Lazy level-only soups for [`WorldPhysics::trace_level`]: the BSP `UModel` with placed-mesh
    /// actor and terrain sources removed (the probe `ULevel::FindSpot`/`CheckSlice` uses, actors
    /// flag off). Movers are not part of this probe (the collision hash is not consulted). Built on
    /// first use and rebuilt whenever a mover registers so no stale base-pose copy is used.
    fn level_worlds(&mut self) -> &(CollisionWorld, CollisionWorld) {
        if self.level_worlds.is_none() {
            let source_names = &self.source_names;
            let is_actor = |source: u32| {
                source_names
                    .get(source as usize)
                    .is_some_and(|label| label.contains(" -> ") || label.ends_with("(terrain)"))
            };
            let build = |world: &CollisionWorld| {
                CollisionWorld::new((0..world.triangle_count()).filter_map(|i| {
                    let source = world.source(i as u32);
                    (!is_actor(source)).then(|| (*world.triangle(i as u32), source))
                }))
            };
            let built = (build(&self.box_world), build(&self.line_world));
            self.level_worlds = Some(built);
        }
        self.level_worlds.as_ref().expect("just built")
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
    fn actor_brush_overlap(&self, actor: &str, center: [f32; 3], extent: [f32; 3]) -> Option<bool> {
        self.actor_brushes
            .get(&actor.to_ascii_lowercase())
            .map(|brush| brush.overlaps_box(center, extent))
    }

    fn prepare_actor_mesh(
        &mut self,
        actor: &str,
        asset: &str,
        location: [f32; 3],
        rotation: [i32; 3],
        scale: [f32; 3],
        pre_pivot: [f32; 3],
    ) -> Result<(), String> {
        let key = actor.to_ascii_lowercase();
        if self.mesh_cache.is_none()
            && self.actor_point_meshes.contains_key(&key)
            && !self.dynamic_mesh_poses.contains_key(&key)
        {
            return Ok(());
        }
        let pose = MeshPose {
            asset: asset.to_ascii_lowercase(),
            location,
            rotation,
            scale,
            pre_pivot,
        };
        if self.dynamic_mesh_poses.get(&key) == Some(&pose) {
            return Ok(());
        }
        if !self.mesh_assets.contains_key(&pose.asset) {
            let cache = self
                .mesh_cache
                .as_mut()
                .ok_or("no installation for runtime mesh import")?;
            let (package_name, object_path) =
                asset.split_once('.').ok_or("mesh path has no package")?;
            let package = cache.get(package_name)?;
            let export = (0..package.package.exports().len())
                .find(|&i| {
                    package
                        .package
                        .object_path(xiii_package::ObjectRef::Export(i as u32))
                        .is_some_and(|path| path.eq_ignore_ascii_case(object_path))
                })
                .ok_or_else(|| format!("mesh {asset} not found"))?;
            let mesh = xiii_decode::static_mesh::decode_static_mesh(
                &package.package,
                &package.data,
                export,
            )
            .map_err(|error| format!("mesh {asset}: {error}"))?;
            let (line_simple, point_simple, _) = crate::mesh_collision_flags(&package, export);
            let simple_present = !mesh.collision[1].triangles.is_empty();
            let triangles = |simple: bool| {
                let set = &mesh.collision[usize::from(simple && simple_present)];
                set.triangles
                    .iter()
                    .map(|triangle| {
                        triangle
                            .vertices
                            .map(|index| to_bevy_position(set.vertices[index as usize]))
                    })
                    .collect()
            };
            self.mesh_assets.insert(
                pose.asset.clone(),
                MeshCollisionAsset {
                    point: triangles(point_simple),
                    line: triangles(line_simple),
                    bounds: xiii_collision::Aabb {
                        min: mesh.primitive.bounding_box.min,
                        max: mesh.primitive.bounding_box.max,
                    },
                },
            );
        }
        let mesh = &self.mesh_assets[&pose.asset];
        let origin = to_bevy_position(location);
        let pivot = to_bevy_position(pre_pivot);
        let scale = to_bevy_scale(scale);
        let rows = rotation_rows(rotation);
        let transform = |triangle: &Triangle| {
            triangle.map(|vertex| {
                let local = std::array::from_fn::<_, 3, _>(|i| (vertex[i] - pivot[i]) * scale[i]);
                std::array::from_fn(|i| {
                    origin[i]
                        + rows[i][0] * local[0]
                        + rows[i][1] * local[1]
                        + rows[i][2] * local[2]
                })
            })
        };
        let mut bounds = xiii_collision::Aabb::empty();
        for x in [mesh.bounds.min[0], mesh.bounds.max[0]] {
            for y in [mesh.bounds.min[1], mesh.bounds.max[1]] {
                for z in [mesh.bounds.min[2], mesh.bounds.max[2]] {
                    bounds.include(transform(&[to_bevy_position([x, y, z]); 3])[0]);
                }
            }
        }
        let center = bevy_to_unreal_position(bounds.center());
        let half: [f32; 3] = std::array::from_fn(|i| (bounds.max[i] - bounds.min[i]) * 0.5);
        let extent = [
            -bevy_to_unreal_position(half)[0],
            bevy_to_unreal_position(half)[1],
            bevy_to_unreal_position(half)[2],
        ];
        self.actor_mesh_bounds.insert(key.clone(), (center, extent));
        self.actor_point_meshes.insert(
            key.clone(),
            CollisionWorld::new(mesh.point.iter().map(|t| (transform(t), 0))),
        );
        self.actor_meshes.insert(
            key.clone(),
            Some((
                CollisionWorld::new(mesh.line.iter().map(|t| (transform(t), 0))),
                vec![true],
            )),
        );
        // Registration may precede runtime mesh preparation. Keep world movement and
        // per-actor primitive queries on the same mesh, including script mesh/scale swaps.
        // Pose-only changes update the existing objects; they must not accumulate colliders.
        if let Some(&slot) = self.mover_by_name.get(&key) {
            let replace = self.dynamic_mesh_poses.get(&key).is_none_or(|old| {
                old.asset != pose.asset
                    || old.scale != pose.scale
                    || old.pre_pivot != pose.pre_pivot
            });
            if replace {
                let (old_box, old_line) = self.movers[slot];
                let box_object = self
                    .box_world
                    .moving(old_box)
                    .ok_or("registered mover box missing")?;
                let source = box_object.source();
                let box_enabled = box_object.is_enabled();
                let line_enabled = self
                    .line_world
                    .moving(old_line)
                    .ok_or("registered mover line missing")?
                    .is_enabled();
                self.box_world.set_moving_enabled(old_box, false);
                self.line_world.set_moving_enabled(old_line, false);
                let box_index = self
                    .box_world
                    .add_moving(MovingObject::from_world_triangles(
                        mesh.point.iter().map(transform),
                        source,
                        origin,
                        rows,
                    ));
                let line_index = self
                    .line_world
                    .add_moving(MovingObject::from_world_triangles(
                        mesh.line.iter().map(transform),
                        source,
                        origin,
                        rows,
                    ));
                self.box_world.set_moving_enabled(box_index, box_enabled);
                self.line_world.set_moving_enabled(line_index, line_enabled);
                self.movers[slot] = (box_index, line_index);
            } else {
                self.set_mover(actor, location, rotation);
            }
        }
        self.dynamic_mesh_poses.insert(key, pose);
        Ok(())
    }

    fn actor_mesh_bounds(&self, actor: &str) -> Option<([f32; 3], [f32; 3])> {
        self.actor_mesh_bounds
            .get(&actor.to_ascii_lowercase())
            .copied()
    }

    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        self.swept_hit(start, end, extent)
    }

    fn trace_level(
        &mut self,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> Option<WorldHit> {
        let s = to_bevy_position(start);
        let e = to_bevy_position(end);
        let half = unreal_extent_to_bevy(extent);
        let zero = half.iter().all(|x| x.abs() < 1e-9);
        let hit = {
            let (box_world, line_world) = self.level_worlds();
            if zero {
                line_world.ray_with_one_sided_sources(s, e, &[])
            } else {
                box_world.sweep(s, e, half)
            }
        }?;
        let t = hit.t;
        let point = [
            s[0] + (e[0] - s[0]) * t,
            s[1] + (e[1] - s[1]) * t,
            s[2] + (e[2] - s[2]) * t,
        ];
        Some(WorldHit {
            location: bevy_to_unreal_position(point),
            normal: bevy_to_unreal_direction(hit.normal),
            time: t,
        })
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

    fn actor_mesh_overlap(
        &mut self,
        actor: &str,
        center: [f32; 3],
        extent: [f32; 3],
    ) -> Option<bool> {
        let key = actor.to_ascii_lowercase();
        let center = to_bevy_position(center);
        let extent = unreal_extent_to_bevy(extent);
        if self.dynamic_mesh_poses.contains_key(&key) {
            return self
                .actor_point_meshes
                .get(&key)
                .map(|world| world.overlaps_aabb(center, extent));
        }
        if let Some(&i) = self.mover_by_name.get(&key) {
            let &(box_index, _) = self.movers.get(i)?;
            let object = self.box_world.moving(box_index)?;
            return Some(
                object
                    .world_triangles()
                    .any(|tri| xiii_collision::aabb_overlaps_triangle(center, extent, &tri)),
            );
        }
        self.actor_point_meshes
            .get(&key)
            .map(|world| world.overlaps_aabb(center, extent))
    }

    fn actor_mesh_sweep(
        &mut self,
        actor: &str,
        start: [f32; 3],
        end: [f32; 3],
        extent: [f32; 3],
    ) -> ActorMeshHit {
        if extent == [0.0; 3] {
            return self.actor_mesh_hit(actor, start, end);
        }
        let key = actor.to_ascii_lowercase();
        let s = to_bevy_position(start);
        let e = to_bevy_position(end);
        let half = unreal_extent_to_bevy(extent);
        let moving_world;
        let world = if self.dynamic_mesh_poses.contains_key(&key) {
            &self.actor_point_meshes[&key]
        } else if let Some(&i) = self.mover_by_name.get(&key) {
            let (box_index, _) = self.movers[i];
            let Some(object) = self.box_world.moving(box_index) else {
                return ActorMeshHit::NoData;
            };
            let bounds = xiii_collision::Aabb::from_center_half(s, half)
                .union(&xiii_collision::Aabb::from_center_half(e, half));
            if !bounds.overlaps(&object.bounds()) {
                return ActorMeshHit::Miss;
            }
            moving_world = CollisionWorld::new(object.world_triangles().map(|tri| (tri, 0)));
            &moving_world
        } else if let Some(world) = self.actor_point_meshes.get(&key) {
            world
        } else {
            return ActorMeshHit::NoData;
        };
        match xiii_collision::sweep_aabb(
            world,
            s,
            e,
            half,
            &xiii_collision::SweepParams {
                skip_start_penetration: false,
                ..Default::default()
            },
        ) {
            Some(hit) => ActorMeshHit::Hit(WorldHit {
                time: hit.t,
                location: [
                    start[0] + (end[0] - start[0]) * hit.t,
                    start[1] + (end[1] - start[1]) * hit.t,
                    start[2] + (end[2] - start[2]) * hit.t,
                ],
                normal: bevy_to_unreal_direction(hit.normal),
            }),
            None => ActorMeshHit::Miss,
        }
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

    fn point_check(&mut self, location: [f32; 3], extent: [f32; 3]) -> Option<SpotHit> {
        let center = to_bevy_position(location);
        let half = unreal_extent_to_bevy(extent);
        let zero = half.iter().all(|x| x.abs() < 1e-9);
        let hits = if zero {
            self.line_world.overlap_aabb(center, half)
        } else {
            self.box_world.overlap_aabb(center, half)
        };
        // `ULevel::FindSpot`/`CheckSlice` probe the level through `SinglePointCheck`
        // (`0x10387db0`) -> `MultiPointCheck` (`0x10387f90`), which checks only the BSP `UModel`
        // (`[LevelInfo+0x80]+0x8c`, vtable `+0x6c`) and — only when the actors flag is set — the
        // collision hash (`ULevel+0xec`, vtable `+0x1c`); there is no separate terrain check. The
        // FindSpot call passes the actors flag as 0, so placed **actor** collision (static-mesh
        // actors and the `ATerrainInfo` terrain, both actors) is not seen — only the BSP level
        // model and registered movers. (item95: the Plage00 PlayerStart pawn box reaches the
        // terrain heightfield, and the Plage01 box an actor-mesh ceiling; the level probe ignores
        // both.) A registered mover's source stays in (its triangles are the moving brush,
        // `triangle == u32::MAX`).
        let is_actor_geometry = |source: u32| {
            let Some(label) = self.source_names.get(source as usize) else {
                return false;
            };
            (label.contains(" -> ") || label.ends_with("(terrain)"))
                && !self.mover_names_by_source.contains_key(&source)
        };
        // Deepest penetrating triangle wins; its normal is oriented from the surface toward the
        // box centre so moving along it separates the box (`0x10389d7e` pushes horizontally along
        // these components).
        let static_world = if zero {
            &self.line_world
        } else {
            &self.box_world
        };
        let mut best: Option<(f32, [f32; 3])> = None;
        for hit in &hits {
            if is_actor_geometry(hit.source) {
                continue;
            }
            if hit.triangle == u32::MAX {
                // A moving-object overlap: `Hit` carries only the source, so re-test that mover's
                // world triangles (as `dump_overlap` does).
                let Some(m) = self.movers.iter().find_map(|(b, _)| {
                    static_world.moving(*b).filter(|m| m.source() == hit.source)
                }) else {
                    continue;
                };
                for tri in m.world_triangles() {
                    if let Some((depth, n)) = aabb_triangle_penetration(center, half, &tri)
                        && best.is_none_or(|(bd, _)| depth > bd)
                    {
                        best = Some((depth, n));
                    }
                }
            } else if let Some((depth, n)) =
                aabb_triangle_penetration(center, half, static_world.triangle(hit.triangle))
                && best.is_none_or(|(bd, _)| depth > bd)
            {
                best = Some((depth, n));
            }
        }
        best.map(|(depth, n)| SpotHit {
            location,
            normal: bevy_to_unreal_direction(n),
            penetration: depth * UNREAL_UNITS_PER_METER,
        })
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
        // The level-only soups cache is built from the static soups, so rebuild it after a mover
        // registration has removed that mover's base-pose copy.
        self.level_worlds = None;
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
    fn runtime_mesh_preparation_updates_transform_and_rejects_missing_assets() {
        let triangle = [[0.0, -10.0, -10.0], [0.0, 10.0, -10.0], [0.0, 0.0, 10.0]];
        let mut physics = WorldPhysicsAdapter::from_entries([], []);
        physics.mesh_assets.insert(
            "fixture.panel".into(),
            MeshCollisionAsset {
                point: vec![triangle.map(to_bevy_position)],
                line: vec![triangle.map(to_bevy_position)],
                bounds: xiii_collision::Aabb {
                    min: [0.0, -10.0, -10.0],
                    max: [0.0, 10.0, 10.0],
                },
            },
        );
        physics
            .prepare_actor_mesh(
                "Spawned",
                "Fixture.Panel",
                [40.0, 0.0, 0.0],
                [0; 3],
                [1.0; 3],
                [0.0; 3],
            )
            .unwrap();
        assert_eq!(
            physics.actor_mesh_overlap("Spawned", [40.0, 0.0, 0.0], [1.0; 3]),
            Some(true)
        );
        let (center, extent) = physics.actor_mesh_bounds("Spawned").unwrap();
        assert!((center[0] - 40.0).abs() < 0.0001);
        assert!((extent[1] - 10.0).abs() < 0.0001);
        // Move, rotate and anisotropically scale the cached local asset; no stale base pose.
        physics
            .prepare_actor_mesh(
                "Spawned",
                "Fixture.Panel",
                [80.0, 0.0, 0.0],
                [0, 16384, 0],
                [1.0, 2.0, 1.0],
                [0.0; 3],
            )
            .unwrap();
        assert_eq!(
            physics.actor_mesh_overlap("Spawned", [40.0, 0.0, 0.0], [1.0; 3]),
            Some(false)
        );
        assert_eq!(
            physics.actor_mesh_overlap("Spawned", [80.0, 0.0, 0.0], [1.0; 3]),
            Some(true)
        );
        assert!(
            physics
                .prepare_actor_mesh(
                    "Missing",
                    "Fixture.Missing",
                    [0.0; 3],
                    [0; 3],
                    [1.0; 3],
                    [0.0; 3]
                )
                .is_err()
        );
        assert_eq!(
            physics.actor_mesh_overlap("Missing", [0.0; 3], [1.0; 3]),
            None
        );
    }

    #[test]
    fn registered_mover_runtime_mesh_and_world_queries_share_pose_and_mesh_swaps() {
        let tri = [[0.0, -10.0, -10.0], [0.0, 10.0, -10.0], [0.0, 0.0, 10.0]];
        let bevy = tri.map(to_bevy_position);
        let mut p = WorldPhysicsAdapter::from_entries([(bevy, 0)], [(bevy, 0)]);
        p.register_mover("Panel", 0, &[tri], [0.0; 3], [0; 3]);
        for (asset, triangles) in [("fixture.panel", vec![bevy]), ("fixture.empty", Vec::new())] {
            p.mesh_assets.insert(
                asset.into(),
                MeshCollisionAsset {
                    point: triangles.clone(),
                    line: triangles,
                    bounds: xiii_collision::Aabb {
                        min: [0.0, -10.0, -10.0],
                        max: [0.0, 10.0, 10.0],
                    },
                },
            );
        }
        p.prepare_actor_mesh(
            "Panel",
            "Fixture.Panel",
            [0.0; 3],
            [0; 3],
            [1.0; 3],
            [0.0; 3],
        )
        .unwrap();
        let count = p.box_world.moving_count();
        for step in 1..=200 {
            let location = [step as f32, 0.0, -100.0];
            p.prepare_actor_mesh(
                "Panel",
                "Fixture.Panel",
                location,
                [0, 16384, 0],
                [1.0; 3],
                [0.0; 3],
            )
            .unwrap();
            let start = [location[0], -20.0, location[2]];
            let end = [location[0], 20.0, location[2]];
            assert!(p.trace(start, end, [0.0; 3]).is_some());
            assert!(matches!(
                p.actor_mesh_hit("Panel", start, end),
                ActorMeshHit::Hit(_)
            ));
            assert!(p.actor_mesh_overlap("Panel", location, [1.0; 3]).unwrap());
            assert!(!p.point_free(location, [1.0; 3]));
            assert!(
                p.trace([-20.0, 0.0, 0.0], [20.0, 0.0, 0.0], [0.0; 3])
                    .is_none(),
                "old registration pose still collides"
            );
        }
        assert_eq!(
            p.box_world.moving_count(),
            count,
            "pose-only updates must reuse collider storage"
        );
        // A script swap to a decoded empty wreck cannot retain intact movement geometry.
        p.prepare_actor_mesh(
            "Panel",
            "Fixture.Empty",
            [200.0, 0.0, -100.0],
            [0, 16384, 0],
            [1.0; 3],
            [0.0; 3],
        )
        .unwrap();
        assert!(p.point_free([200.0, 0.0, -100.0], [1.0; 3]));
        assert!(
            p.trace([200.0, -20.0, -100.0], [200.0, 20.0, -100.0], [0.0; 3])
                .is_none()
        );
        assert!(matches!(
            p.actor_mesh_hit("Panel", [200.0, -20.0, -100.0], [200.0, 20.0, -100.0]),
            ActorMeshHit::Miss
        ));
        // Replacing a non-colliding mover must preserve its disabled state.
        p.set_mover_collision("Panel", false);
        p.prepare_actor_mesh(
            "Panel",
            "Fixture.Panel",
            [0.0; 3],
            [0; 3],
            [2.0; 3],
            [0.0; 3],
        )
        .unwrap();
        assert!(p.point_free([0.0; 3], [1.0; 3]));
        p.set_mover_collision("Panel", true);
        assert!(!p.point_free([0.0; 3], [1.0; 3]));
    }

    #[test]
    fn mesh_point_overlap_uses_box_triangles_and_current_mover_pose() {
        let triangle = [[40.0, -10.0, -10.0], [40.0, 10.0, -10.0], [40.0, 0.0, 10.0]];
        let mut scene = crate::WorldScene::default();
        scene
            .actor_point_primitives
            .insert("door".into(), vec![triangle.map(to_bevy_position)]);
        scene.actor_point_primitives.insert("empty".into(), vec![]);
        let mut physics = WorldPhysicsAdapter::from_scene(&scene);
        assert_eq!(
            physics.actor_mesh_overlap("Door", [0.0; 3], [1.0; 3]),
            Some(false)
        );
        assert_eq!(
            physics.actor_mesh_overlap("Door", [40.0, 0.0, 0.0], [1.0; 3]),
            Some(true)
        );
        let ActorMeshHit::Hit(hit) =
            physics.actor_mesh_sweep("Door", [0.0; 3], [80.0, 0.0, 0.0], [1.0; 3])
        else {
            panic!("crossed thin primitive");
        };
        assert!((hit.time - 39.0 / 80.0).abs() < 0.0001);
        assert!(matches!(
            physics.actor_mesh_sweep("Empty", [0.0; 3], [80.0, 0.0, 0.0], [1.0; 3]),
            ActorMeshHit::Miss
        ));
        let ActorMeshHit::Hit(hit) =
            physics.actor_mesh_sweep("Door", [40.0, 0.0, 0.0], [80.0, 0.0, 0.0], [1.0; 3])
        else {
            panic!("overlapping start must be a candidate");
        };
        assert_eq!(hit.time, 0.0);
        // Outside the triangle but inside its bounds: a bounds-only implementation over-fires.
        assert_eq!(
            physics.actor_mesh_overlap("Door", [40.0, 9.0, 9.0], [0.1; 3]),
            Some(false)
        );
        assert_eq!(
            physics.actor_mesh_overlap("Empty", [0.0; 3], [1.0; 3]),
            Some(false)
        );
        assert_eq!(
            physics.actor_mesh_overlap("Unknown", [0.0; 3], [1.0; 3]),
            None
        );
        physics.register_mover("Door", 0, &[triangle], [0.0; 3], [0; 3]);
        physics.set_mover("Door", [100.0, 0.0, 0.0], [0; 3]);
        assert_eq!(
            physics.actor_mesh_overlap("Door", [40.0, 0.0, 0.0], [1.0; 3]),
            Some(false)
        );
        assert_eq!(
            physics.actor_mesh_overlap("Door", [140.0, 0.0, 0.0], [1.0; 3]),
            Some(true)
        );
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
    fn point_check_reports_the_floor_normal_and_penetration() {
        // tiny_soup floor at Bevy y=0 (Unreal z=0). A 40-half box centred 30 UU above it has
        // its bottom 10 UU inside: normal +Z, penetration 10.
        let mut p = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup());
        let hit = p
            .point_check([0.0, 0.0, 30.0], [40.0, 40.0, 40.0])
            .expect("the sunk floor box must block");
        assert!(hit.normal[2] > 0.9, "normal {:?}", hit.normal);
        assert!(
            (hit.penetration - 10.0).abs() < 1.0,
            "penetration {}",
            hit.penetration
        );
        // A free box returns no contact.
        assert!(
            p.point_check([0.0, 0.0, 200.0], [40.0, 40.0, 40.0])
                .is_none()
        );
    }

    #[test]
    fn find_spot_lifts_a_box_sunk_into_the_floor() {
        let mut p = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup());
        let extent = [40.0, 40.0, 40.0];
        // Bottom 10 UU inside the floor; the (r, r, 1) slice at the centre is free, so
        // CheckSlice settles the box onto the floor.
        let spot = p
            .find_spot([0.0, 0.0, 30.0], extent)
            .expect("the sunk box is lifted");
        assert!(spot[2] > 30.0, "lifted: {spot:?}");
        assert!(
            p.point_check(spot, extent).is_none(),
            "the lifted spot must be free: {spot:?}"
        );
        // A free spot is used unchanged.
        let free = p
            .find_spot([0.0, 0.0, 200.0], extent)
            .expect("a free spot is placed");
        assert_eq!(free, [0.0, 0.0, 200.0]);
    }

    #[test]
    fn point_check_ignores_actor_and_terrain_sources_but_sees_the_bsp() {
        // `ULevel::SinglePointCheck` (`0x10387db0`) -> `MultiPointCheck` (`0x10387f90`) checks
        // only the BSP `UModel` and (when the actors flag is set, which FindSpot passes as 0)
        // the actor collision hash. Placed static-mesh actors and the `ATerrainInfo` terrain are
        // actors, so a box sunk into them is free to the level probe; the same soup under a BSP
        // label blocks. `point_free`/`trace` (movement) still see the actor geometry.
        let extent = [40.0, 40.0, 40.0];
        let sunk = [0.0, 0.0, 30.0];

        let mut terrain = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup())
            .with_source_names(&["TerrainInfo1 (terrain)", "Wall"]);
        assert!(
            terrain.point_check(sunk, extent).is_none(),
            "the level probe must ignore terrain actor collision"
        );
        assert!(
            !terrain.point_free(sunk, extent),
            "movement's point_free still sees the terrain"
        );

        let mut actor = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup())
            .with_source_names(&["StaticMeshActor1 -> StaticPlage.GR_floor", "Wall"]);
        assert!(
            actor.point_check(sunk, extent).is_none(),
            "placed static-mesh actor collision is not in the level probe"
        );

        let mut bsp = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup())
            .with_source_names(&["Model333 (BSP)", "Wall"]);
        assert!(
            bsp.point_check(sunk, extent).is_some(),
            "a BSP floor blocks the level probe"
        );
    }

    #[test]
    fn trace_level_ignores_actor_and_terrain_surfaces_but_sees_the_bsp() {
        // `ULevel::CheckSlice`'s downward `SingleLineCheck` on the FindSpot path probes the BSP
        // `UModel` only (actors flag 0). Three stacked Bevy-space floors: a BSP floor at y=0, a
        // placed static-mesh floor at y=0.5 and a terrain heightfield at y=1.0. A downward ray
        // sees the highest (terrain) through `trace` (movement) but `trace_level` skips the actor
        // surfaces and lands on the BSP.
        let quad = |y: f32, source: u32| -> Vec<(Triangle, u32)> {
            vec![
                ([[-1.0, y, -1.0], [1.0, y, -1.0], [1.0, y, 1.0]], source),
                ([[-1.0, y, -1.0], [1.0, y, 1.0], [-1.0, y, 1.0]], source),
            ]
        };
        let soup: Vec<(Triangle, u32)> = quad(0.0, 0)
            .into_iter()
            .chain(quad(0.5, 1))
            .chain(quad(1.0, 2))
            .collect();
        let mut p = WorldPhysicsAdapter::from_entries(soup.clone(), soup).with_source_names(&[
            "Model333 (BSP)",
            "StaticMeshActor1 -> StaticPlage.GR_floor",
            "TerrainInfo1 (terrain)",
        ]);
        let start = u([0.0, 2.0, 0.0]);
        let end = u([0.0, -1.0, 0.0]);
        let movement = p
            .trace(start, end, [0.0; 3])
            .expect("movement's trace hits the terrain");
        assert!(
            (movement.location[2] - u([0.0, 1.0, 0.0])[2]).abs() < 1e-3,
            "trace must see the terrain: {movement:?}"
        );
        let level = p
            .trace_level(start, end, [0.0; 3])
            .expect("the level probe hits the BSP");
        assert!(
            (level.location[2] - u([0.0, 0.0, 0.0])[2]).abs() < 1e-3,
            "trace_level must skip the actor surfaces: {level:?}"
        );
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
