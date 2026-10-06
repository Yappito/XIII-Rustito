//! Filesystem-free collision queries against a decoded world triangle soup.
//!
//! XIII/UE2 moves pawns against world geometry as an axis-aligned *extent box* of
//! half-size `(CollisionRadius, CollisionRadius, CollisionHeight)` in Unreal units (the
//! cylinder is only used for actor-vs-actor). This crate therefore implements a swept
//! **AABB vs triangle** query with the separating-axis theorem, plus a small `move_slide`
//! helper that approximates `UPawn::physWalking` well enough for a doorway test.
//!
//! Design:
//! - [`CollisionWorld`] stores triangles in Bevy space (metres) with a `u32` source id
//!   each and builds a bounding-volume hierarchy once. Queries traverse only overlapping
//!   nodes, not every triangle.
//! - Triangles are two-sided: the soup carries no reliable facing for collision, so the
//!   contact normal is chosen to oppose the sweep direction, not from the winding.
//! - No Bevy, no filesystem, no external crates.

#![warn(missing_docs)]

mod bvh;
mod spot;
mod sweep;
mod walk;

pub use bvh::{Aabb, Bvh};
pub use spot::{FindSpot, FindSpotError, FindSpotParams, find_spot};
pub use sweep::{SweepHit, SweepParams, sweep_aabb};
pub use walk::{FLOOR_PROBE_RATIO, WalkParams, walk_move};

/// A triangle in Bevy space (metres).
pub type Triangle = [[f32; 3]; 3];

/// A point/direction in Bevy space (metres).
pub type Vec3 = [f32; 3];

/// Default movement skin in metres. UE2 offsets the pawn from the surface by a small amount;
/// 1 mm is small relative to the imported unit scale (90 units/m) and documented as an
/// approximation, not a measured engine constant. The engine's `APawn::stepUp` (`Engine.dll`
/// `0x103baa30`) has no separate skin constant; `xiii-world::reach` supplies its own empirical
/// `SKIN_UU = 0.05`, so this crate default is only for callers that do not override it.
pub const DEFAULT_SKIN: f32 = 0.001;

/// One overlap hit against a triangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Hit {
    /// Triangle index in [`CollisionWorld::triangle`].
    pub triangle: u32,
    /// Source id recorded for that triangle.
    pub source: u32,
    /// Unit triangle normal (two-sided, winding-derived).
    pub normal: Vec3,
}

/// A moving collision object: the brush of a UE2 `Mover`/`PHYS_MovingBrush`. Its triangles are
/// kept in the object's local frame and a rigid world transform (translation of the object
/// origin plus a rotation) is refreshed from the VM each tick. Queries test it together with the
/// static BVH; a per-object world AABB (rebuilt on every transform update) culls it cheaply.
///
/// The caller must **not** also leave the mover's triangles in the static soup; this type cannot
/// know which collision source belongs to a mover. `MovingObject::source` identifies it in query
/// hits.
#[derive(Debug, Clone)]
pub struct MovingObject {
    local: Vec<Triangle>,
    source: u32,
    origin: Vec3,
    /// Rows of the rotation matrix: `world = origin + rows * local`.
    rotation: [Vec3; 3],
    bounds: Aabb,
}

impl MovingObject {
    /// Builds a moving object from triangles already in world space at their initial pose. The
    /// local frame is `rotation^T (triangle - origin)`, where the rows of `rotation` are the
    /// initial local axes in world space.
    pub fn from_world_triangles(
        triangles: impl IntoIterator<Item = Triangle>,
        source: u32,
        origin: Vec3,
        rotation: [Vec3; 3],
    ) -> Self {
        let inv = transpose(rotation);
        let local: Vec<Triangle> = triangles
            .into_iter()
            .map(|t| t.map(|p| mul_mat(inv, sub(p, origin))))
            .collect();
        let mut object = Self {
            local,
            source,
            origin,
            rotation,
            bounds: Aabb::empty(),
        };
        object.rebuild_bounds();
        object
    }

    /// Updates the world transform (origin translation and rotation rows).
    pub fn set_transform(&mut self, origin: Vec3, rotation: [Vec3; 3]) {
        self.origin = origin;
        self.rotation = rotation;
        self.rebuild_bounds();
    }

    /// Collision source id shared by every triangle.
    pub fn source(&self) -> u32 {
        self.source
    }

    /// Number of local triangles.
    pub fn triangle_count(&self) -> usize {
        self.local.len()
    }

    /// World-space AABB of the current pose.
    pub fn bounds(&self) -> Aabb {
        self.bounds
    }

    fn rebuild_bounds(&mut self) {
        let mut b = Aabb::empty();
        for t in &self.local {
            for p in t {
                b.include(self.to_world(*p));
            }
        }
        self.bounds = b;
    }

    fn to_world(&self, p: Vec3) -> Vec3 {
        add(self.origin, mul_mat(self.rotation, p))
    }

    fn world_triangle(&self, i: usize) -> Triangle {
        self.local[i].map(|p| self.to_world(p))
    }
}

/// The collision world: triangles, their source ids and a broad-phase BVH, plus the dynamic
/// (moving) objects.
pub struct CollisionWorld {
    triangles: Vec<Triangle>,
    sources: Vec<u32>,
    /// Position of each retained triangle in the input sequence passed to [`CollisionWorld::new`]
    /// (degenerate triangles are dropped, so this does not shift). Lets a caller map a hit back
    /// to its source product (for example the surface material of a floor triangle).
    origins: Vec<u32>,
    bvh: Bvh,
    degenerate: usize,
    dynamic: Vec<MovingObject>,
}

impl CollisionWorld {
    /// Builds a world from `(triangle, source id)` pairs. Degenerate triangles (near-zero
    /// area or non-finite coordinates) are dropped and counted.
    pub fn new(entries: impl IntoIterator<Item = (Triangle, u32)>) -> Self {
        let mut triangles = Vec::new();
        let mut sources = Vec::new();
        let mut origins = Vec::new();
        let mut degenerate = 0usize;
        for (origin, (t, s)) in entries.into_iter().enumerate() {
            if is_degenerate(&t) {
                degenerate += 1;
                continue;
            }
            triangles.push(t);
            sources.push(s);
            origins.push(origin as u32);
        }
        let bvh = Bvh::build(&triangles);
        Self {
            triangles,
            sources,
            origins,
            bvh,
            degenerate,
            dynamic: Vec::new(),
        }
    }

    /// Position of triangle `index` in the input iterator of [`CollisionWorld::new`].
    pub fn origin(&self, index: u32) -> u32 {
        self.origins[index as usize]
    }

    /// Number of retained triangles.
    pub fn triangle_count(&self) -> usize {
        self.triangles.len()
    }

    /// Number of degenerate triangles dropped at build time.
    pub fn degenerate_count(&self) -> usize {
        self.degenerate
    }

    /// Number of broad-phase nodes (for diagnostics).
    pub fn bvh_node_count(&self) -> usize {
        self.bvh.node_count()
    }

    /// Triangle by index.
    pub fn triangle(&self, index: u32) -> &Triangle {
        &self.triangles[index as usize]
    }

    /// Source id of a triangle.
    pub fn source(&self, index: u32) -> u32 {
        self.sources[index as usize]
    }

    pub(crate) fn for_each_candidate(&self, query: Aabb, f: impl FnMut(u32)) {
        self.bvh.traverse(query, f);
    }

    /// Adds a moving collision object, returning its index.
    pub fn add_moving(&mut self, object: MovingObject) -> usize {
        self.dynamic.push(object);
        self.dynamic.len() - 1
    }

    /// Number of moving collision objects.
    pub fn moving_count(&self) -> usize {
        self.dynamic.len()
    }

    /// A moving object by index.
    pub fn moving(&self, index: usize) -> Option<&MovingObject> {
        self.dynamic.get(index)
    }

    /// Replaces a moving object's world transform; `false` when the index is out of range.
    pub fn set_moving_transform(
        &mut self,
        index: usize,
        origin: Vec3,
        rotation: [Vec3; 3],
    ) -> bool {
        match self.dynamic.get_mut(index) {
            Some(object) => {
                object.set_transform(origin, rotation);
                true
            }
            None => false,
        }
    }

    /// Calls `f(world_triangle, source, local_index)` for every moving-object triangle whose
    /// per-object AABB overlaps `query` (simple cull). The returned index is the triangle's
    /// index within its object; `sweep`/`ray` report `u32::MAX` as the triangle field for
    /// dynamic hits (the `source` names the mover).
    pub(crate) fn for_each_dynamic_candidate(
        &self,
        query: Aabb,
        mut f: impl FnMut(Triangle, u32, u32),
    ) {
        for object in &self.dynamic {
            if !object.bounds.overlaps(&query) {
                continue;
            }
            for i in 0..object.local.len() {
                f(object.world_triangle(i), object.source, i as u32);
            }
        }
    }

    /// Continuous swept AABB query with default parameters. See [`sweep_aabb`].
    pub fn sweep(&self, start: Vec3, end: Vec3, half_extents: Vec3) -> Option<SweepHit> {
        sweep_aabb(self, start, end, half_extents, &SweepParams::default())
    }

    /// Nearest ray hit along `start -> end` (zero extent). `t` in [0,1] is the fraction of
    /// the segment; both triangle sides are tested. Uses Möller-Trumbore per triangle over
    /// BVH candidates, identical in result to the viewer's `ray_triangle` probe. The swept
    /// SAT is not used here because a zero-extent box at an exact point can be rejected by a
    /// near-degenerate edge axis.
    pub fn ray(&self, start: Vec3, end: Vec3) -> Option<SweepHit> {
        let d = sub(end, start);
        let mut query = Aabb::empty();
        query.include(start);
        query.include(end);
        let mut best: Option<SweepHit> = None;
        self.for_each_candidate(query, |i| {
            consider_ray(&mut best, start, d, self.triangle(i), i, self.source(i));
        });
        self.for_each_dynamic_candidate(query, |t, source, _idx| {
            consider_ray(&mut best, start, d, &t, u32::MAX, source);
        });
        best
    }

    /// All triangles overlapping `(center, half_extents)`, in BVH order then moving objects.
    pub fn overlap_aabb(&self, center: Vec3, half_extents: Vec3) -> Vec<Hit> {
        let q = Aabb::from_center_half(center, half_extents);
        let mut out = Vec::new();
        self.for_each_candidate(q, |i| {
            consider_overlap(
                &mut out,
                center,
                half_extents,
                self.triangle(i),
                i,
                self.source(i),
            );
        });
        self.for_each_dynamic_candidate(q, |t, source, _idx| {
            consider_overlap(&mut out, center, half_extents, &t, u32::MAX, source);
        });
        out
    }

    /// True when any (static or moving) triangle overlaps `(center, half_extents)`.
    pub fn overlaps_aabb(&self, center: Vec3, half_extents: Vec3) -> bool {
        let q = Aabb::from_center_half(center, half_extents);
        let mut hit = false;
        self.for_each_candidate(q, |i| {
            if !hit && aabb_triangle_overlap(center, half_extents, self.triangle(i)).is_some() {
                hit = true;
            }
        });
        self.for_each_dynamic_candidate(q, |t, _source, _idx| {
            if !hit && aabb_triangle_overlap(center, half_extents, &t).is_some() {
                hit = true;
            }
        });
        hit
    }
}

/// Keeps the nearest ray hit: fills `best` when `t` is a valid hit closer than the current one.
fn consider_ray(
    best: &mut Option<SweepHit>,
    start: Vec3,
    d: Vec3,
    tri: &Triangle,
    index: u32,
    source: u32,
) {
    if let Some(t) = ray_triangle(start, d, tri)
        && best.is_none_or(|b| t < b.t)
    {
        let n = normalize(triangle_normal(tri));
        // Orient the normal against the ray so it points toward the origin side.
        let normal = if dot(n, d) > 0.0 { mul(n, -1.0) } else { n };
        *best = Some(SweepHit {
            t,
            normal,
            triangle: index,
            source,
            start_penetrating: false,
        });
    }
}

/// Appends an overlap hit for a triangle (static or moving) to `out`.
fn consider_overlap(
    out: &mut Vec<Hit>,
    center: Vec3,
    half_extents: Vec3,
    tri: &Triangle,
    index: u32,
    source: u32,
) {
    if aabb_triangle_overlap(center, half_extents, tri).is_some() {
        out.push(Hit {
            triangle: index,
            source,
            normal: normalize(triangle_normal(tri)),
        });
    }
}

/// A contact recorded by [`move_slide`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveContact {
    /// Source id of the blocking triangle.
    pub source: u32,
    /// Triangle index of the blocking triangle.
    pub triangle: u32,
    /// Time of impact of the failing sweep, in [0,1] of that sweep segment.
    pub t: f32,
    /// Unit contact normal.
    pub normal: Vec3,
    /// Height of the contact point above the movement start (Bevy Y, metres).
    pub height: f32,
    /// Box position at impact (Bevy metres).
    pub position: Vec3,
}

/// Parameters for [`move_slide`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MoveParams {
    /// Back-off from a surface after impact, in metres. Prevents re-collision with the same
    /// triangle on the next iteration.
    pub skin: f32,
    /// Maximum slide iterations.
    pub max_iterations: u32,
    /// Maximum step height to attempt (metres). Zero disables step-up.
    pub max_step_height: f32,
}

impl Default for MoveParams {
    fn default() -> Self {
        Self {
            skin: DEFAULT_SKIN,
            max_iterations: 4,
            max_step_height: 0.0,
        }
    }
}

/// Result of [`move_slide`].
#[derive(Debug, Clone, PartialEq)]
pub struct MoveResult {
    /// Final position (Bevy metres).
    pub position: Vec3,
    /// True when the movement was stopped or deflected by geometry.
    pub blocked: bool,
    /// Contacts in order.
    pub contacts: Vec<MoveContact>,
    /// Iterations actually used.
    pub iterations: u32,
    /// Whether the result rests on a walkable floor (final downward probe).
    pub on_floor: bool,
    /// Set by [`walk_move`] when the post-move floor follow found no walkable floor below:
    /// the pawn is airborne (or over a non-walkable surface). Always `false` for
    /// [`move_slide`].
    pub falling: bool,
}

impl MoveResult {
    /// Distinct source ids that were hit, in first-contact order.
    pub fn sources(&self) -> Vec<u32> {
        let mut v: Vec<u32> = self.contacts.iter().map(|c| c.source).collect();
        v.dedup();
        v
    }
}

fn dot(a: Vec3, b: Vec3) -> f32 {
    a[0] * b[0] + a[1] * b[1] + a[2] * b[2]
}

fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}

fn mul(a: Vec3, s: f32) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn length(a: Vec3) -> f32 {
    dot(a, a).sqrt()
}

fn normalize(a: Vec3) -> Vec3 {
    let l = length(a);
    if l > 1e-20 { mul(a, 1.0 / l) } else { [0.0; 3] }
}

fn finite(v: Vec3) -> bool {
    v.iter().all(|x| x.is_finite())
}

fn cross(a: Vec3, b: Vec3) -> Vec3 {
    [
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    ]
}

/// Matrix-vector product with the matrix given by rows.
fn mul_mat(rows: [Vec3; 3], v: Vec3) -> Vec3 {
    [dot(rows[0], v), dot(rows[1], v), dot(rows[2], v)]
}

/// Transpose of a matrix given by rows (its rows become the columns).
fn transpose(m: [Vec3; 3]) -> [Vec3; 3] {
    [
        [m[0][0], m[1][0], m[2][0]],
        [m[0][1], m[1][1], m[2][1]],
        [m[0][2], m[1][2], m[2][2]],
    ]
}

fn triangle_normal(t: &Triangle) -> Vec3 {
    cross(sub(t[1], t[0]), sub(t[2], t[0]))
}

/// True when a triangle is too small to give a stable normal or has non-finite coordinates.
fn is_degenerate(t: &Triangle) -> bool {
    if !finite(t[0]) || !finite(t[1]) || !finite(t[2]) {
        return true;
    }
    length(triangle_normal(t)) <= 1e-12
}

/// Moves an extent box along `delta`, sliding along surfaces, with optional step-up.
///
/// This is a documented **approximation** of `UPawn::physWalking` / `stepUp`, not a fidelity
/// claim: the sweep is a single AABB against two-sided triangles, the skin is empirical and
/// step-up is a three-sweep heuristic when enabled.
pub fn move_slide(
    world: &CollisionWorld,
    start: Vec3,
    delta: Vec3,
    half_extents: Vec3,
    params: &MoveParams,
) -> MoveResult {
    let mut pos = start;
    let mut remaining = delta;
    let mut blocked = false;
    let mut contacts = Vec::new();
    let mut iterations = 0u32;
    let mut on_floor = false;

    for _ in 0..params.max_iterations.max(1) {
        iterations += 1;
        let travel = length(remaining);
        if travel <= 1e-9 {
            break;
        }
        let dir = mul(remaining, 1.0 / travel);
        let target = add(pos, remaining);
        let hit = match sweep_aabb(world, pos, target, half_extents, &SweepParams::default()) {
            Some(h) => h,
            None => {
                pos = target;
                break;
            }
        };
        blocked = true;
        let at = add(pos, mul(dir, (hit.t * travel - params.skin).max(0.0)));
        contacts.push(MoveContact {
            source: hit.source,
            triangle: hit.triangle,
            t: hit.t,
            normal: hit.normal,
            height: at[1] - start[1],
            position: at,
        });

        // Optional step-up when blocked by a near-vertical surface.
        if params.max_step_height > 1e-6
            && hit.normal[1].abs() < 0.3
            && let Some(stepped) =
                try_step(world, pos, dir, travel, half_extents, params, &mut contacts)
        {
            pos = stepped;
            break;
        }

        // Slide the remainder along the contact plane.
        let mut leftover = mul(remaining, (1.0 - hit.t).max(0.0));
        let into = dot(leftover, hit.normal);
        if into < 0.0 {
            leftover = sub(leftover, mul(hit.normal, into));
        }
        pos = at;
        remaining = leftover;
    }

    if let Some(h) = sweep_aabb(
        world,
        pos,
        add(pos, [0.0, -0.05, 0.0]),
        half_extents,
        &SweepParams::default(),
    ) {
        on_floor = h.normal[1] > 0.5;
    }

    MoveResult {
        position: pos,
        blocked,
        contacts,
        iterations,
        on_floor,
        falling: false,
    }
}

/// Attempts to step up over a near-vertical surface: up, forward, down. Returns the landing
/// position when the path clears and a floor is found.
#[allow(clippy::too_many_arguments)]
fn try_step(
    world: &CollisionWorld,
    pos: Vec3,
    dir: Vec3,
    travel: f32,
    half_extents: Vec3,
    params: &MoveParams,
    contacts: &mut Vec<MoveContact>,
) -> Option<Vec3> {
    let up = [0.0, params.max_step_height, 0.0];
    let up_target = add(pos, up);
    if sweep_aabb(world, pos, up_target, half_extents, &SweepParams::default()).is_some() {
        return None;
    }
    let fwd_target = add(up_target, mul(dir, travel.max(0.0)));
    if let Some(h) = sweep_aabb(
        world,
        up_target,
        fwd_target,
        half_extents,
        &SweepParams::default(),
    ) {
        contacts.push(MoveContact {
            source: h.source,
            triangle: h.triangle,
            t: h.t,
            normal: h.normal,
            height: up_target[1] - pos[1],
            position: up_target,
        });
        return None;
    }
    let down_target = add(fwd_target, mul(up, -1.0));
    let down_target = add(down_target, mul([0.0, 1.0, 0.0], -params.skin));
    let land = sweep_aabb(
        world,
        fwd_target,
        down_target,
        half_extents,
        &SweepParams::default(),
    )?;
    if land.normal[1] > 0.5 {
        Some(add(fwd_target, mul(sub(down_target, fwd_target), land.t)))
    } else {
        None
    }
}

/// Static overlap test between an AABB and a triangle with the separating-axis theorem.
fn aabb_triangle_overlap(center: Vec3, half: Vec3, t: &Triangle) -> Option<()> {
    let n = triangle_normal(t);
    let box_axes = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];
    let edges = [sub(t[1], t[0]), sub(t[2], t[1]), sub(t[0], t[2])];
    let mut axes: Vec<Vec3> = Vec::with_capacity(13);
    axes.extend_from_slice(&box_axes);
    axes.push(n);
    for e in box_axes {
        for f in edges {
            axes.push(cross(e, f));
        }
    }
    for axis in axes {
        let l = length(axis);
        if l < 1e-9 {
            continue;
        }
        let a = mul(axis, 1.0 / l);
        let c = dot(center, a);
        let radius = half[0] * a[0].abs() + half[1] * a[1].abs() + half[2] * a[2].abs();
        let (tmin, tmax) = triangle_projection(t, a);
        if (c - radius) > tmax || (c + radius) < tmin {
            return None;
        }
    }
    Some(())
}

/// Möller-Trumbore ray/triangle intersection (both sides). `d` is the segment vector; the
/// returned value is `t` in [0,1] along `start + t*d`.
fn ray_triangle(o: Vec3, d: Vec3, t: &Triangle) -> Option<f32> {
    let e1 = sub(t[1], t[0]);
    let e2 = sub(t[2], t[0]);
    let p = cross(d, e2);
    let det = dot(e1, p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = sub(o, t[0]);
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let time = dot(e2, q) * inv;
    (-1e-6..=1.0 + 1e-6)
        .contains(&time)
        .then_some(time.clamp(0.0, 1.0))
}

fn triangle_projection(t: &Triangle, axis: Vec3) -> (f32, f32) {
    let a = dot(t[0], axis);
    let b = dot(t[1], axis);
    let c = dot(t[2], axis);
    (a.min(b).min(c), a.max(b).max(c))
}

#[cfg(test)]
mod tests;
