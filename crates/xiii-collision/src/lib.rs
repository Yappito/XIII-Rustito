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
mod sweep;

pub use bvh::{Aabb, Bvh};
pub use sweep::{SweepHit, SweepParams, sweep_aabb};

/// A triangle in Bevy space (metres).
pub type Triangle = [[f32; 3]; 3];

/// A point/direction in Bevy space (metres).
pub type Vec3 = [f32; 3];

/// Default movement skin in metres. UE2 offsets the pawn from the surface by a small amount;
/// 1 mm is small relative to the imported unit scale (50 units/m) and documented as an
/// approximation, not a measured engine constant.
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

/// The collision world: triangles, their source ids and a broad-phase BVH.
pub struct CollisionWorld {
    triangles: Vec<Triangle>,
    sources: Vec<u32>,
    bvh: Bvh,
    degenerate: usize,
}

impl CollisionWorld {
    /// Builds a world from `(triangle, source id)` pairs. Degenerate triangles (near-zero
    /// area or non-finite coordinates) are dropped and counted.
    pub fn new(entries: impl IntoIterator<Item = (Triangle, u32)>) -> Self {
        let mut triangles = Vec::new();
        let mut sources = Vec::new();
        let mut degenerate = 0usize;
        for (t, s) in entries {
            if is_degenerate(&t) {
                degenerate += 1;
                continue;
            }
            triangles.push(t);
            sources.push(s);
        }
        let bvh = Bvh::build(&triangles);
        Self {
            triangles,
            sources,
            bvh,
            degenerate,
        }
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
            if let Some(t) = ray_triangle(start, d, self.triangle(i))
                && best.is_none_or(|b| t < b.t)
            {
                let n = normalize(triangle_normal(self.triangle(i)));
                // Orient the normal against the ray so it points toward the origin side.
                let normal = if dot(n, d) > 0.0 { mul(n, -1.0) } else { n };
                best = Some(SweepHit {
                    t,
                    normal,
                    triangle: i,
                    source: self.source(i),
                    start_penetrating: false,
                });
            }
        });
        best
    }

    /// All triangles overlapping `(center, half_extents)`, in BVH traversal order.
    pub fn overlap_aabb(&self, center: Vec3, half_extents: Vec3) -> Vec<Hit> {
        let q = Aabb::from_center_half(center, half_extents);
        let mut out = Vec::new();
        self.for_each_candidate(q, |i| {
            if aabb_triangle_overlap(center, half_extents, self.triangle(i)).is_some() {
                let n = normalize(triangle_normal(self.triangle(i)));
                out.push(Hit {
                    triangle: i,
                    source: self.source(i),
                    normal: n,
                });
            }
        });
        out
    }

    /// True when any triangle overlaps `(center, half_extents)`.
    pub fn overlaps_aabb(&self, center: Vec3, half_extents: Vec3) -> bool {
        let q = Aabb::from_center_half(center, half_extents);
        let mut hit = false;
        self.for_each_candidate(q, |i| {
            if !hit && aabb_triangle_overlap(center, half_extents, self.triangle(i)).is_some() {
                hit = true;
            }
        });
        hit
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
    /// Whether the result rests on the floor (final downward probe hit).
    pub on_floor: bool,
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
