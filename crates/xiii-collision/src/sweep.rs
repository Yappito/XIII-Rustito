//! Swept axis-aligned box vs triangle with the separating-axis theorem.
//!
//! Relative-motion formulation: the box is held at `start` and the triangle moves by
//! `-(end - start)`. For each of the 13 candidate axes (3 box axes, the triangle normal and
//! the 9 box-edge x triangle-edge cross products) the box and triangle intervals move
//! linearly; intersecting the per-axis entry/exit time windows gives the first time of
//! impact. Near-zero axes are skipped. Triangles are two-sided: the normal is oriented to
//! oppose the sweep direction, never taken from the winding.

use crate::{Aabb, CollisionWorld, Triangle, Vec3, cross, dot, length, mul, normalize, sub};

/// Interval overlap below which a contact on an axis the box does not move along counts as
/// touching (separated), in metres (0.0009 UU; well below the movement skin of 0.05 UU).
const TOUCH_EPS: f32 = 1e-5;

/// Sweep options.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepParams {
    /// When true (default), a triangle the box already overlaps/touches at `t = 0` is
    /// ignored **only if the motion does not go into it** (`dot(d, n_plane) >= -eps`,
    /// `n_plane` oriented from the triangle toward the box centre). This lets a resting pawn
    /// ignore the floor it stands on and a box sliding along a wall it touches, but still
    /// blocks motion that drives into a surface it is already flush with. Set false to always
    /// report a `t = 0` `start_penetrating` hit (used by `ray`).
    pub skip_start_penetration: bool,
    /// When `Some(z)`, a `t = 0` `start_penetrating` hit whose normal up-component is at least
    /// `z` (a **walkable floor the box is resting on**) is always ignored. A box walking on a
    /// slightly inclined floor grazes that floor on every horizontal sub-step; without this the
    /// grazing `t = 0` contact is the nearest hit and hides the real obstacle behind it
    /// (item1k: the walkable-ledge stall). `None` (default) keeps the plain behaviour.
    pub ignore_resting_floor_z: Option<f32>,
    /// When true, a triangle the box already overlaps/touches at `t = 0` **never blocks** —
    /// the sweep continues and reports only geometry the box reaches while clear. This is the
    /// engine's measured `ULevel::MoveActor` behaviour (Engine.dll 0x1038a89a-0x1038ac05): the
    /// world check runs along a segment extended 2 UU behind the start (`+2.0` at 0x1038a981)
    /// and a hit inside that back-off — `(2+|delta|)*t_hit <= 2`, branch at 0x1038aba8 — is
    /// discarded (0x1038abaf) instead of stopping the move, letting a mover escape geometry it
    /// starts flush with/inside. For a true continuous sweep this reduces exactly to discarding
    /// start-penetrating hits: any `t = 0` contact lies at/behind the start box, i.e. within
    /// the back-off, while every hit found from a clear box maps to a contact strictly ahead of
    /// the start and still blocks. A surface whose normal up-component reaches
    /// [`Self::walkable_floor_z`] is exempt: the engine rides walkable floors through its own
    /// floor machinery (`physWalking`'s floor snap), not through the move back-off. `false`
    /// (default) keeps the plain behaviour; walk movement turns it on.
    pub discard_start_penetration: bool,
    /// Walkability threshold for [`Self::discard_start_penetration`] exemptions: a
    /// start-penetrating contact with `normal.y >= z` follows the plain (non-discarding) rules
    /// so ramp/slope riding keeps working. `None` discards every start-penetrating contact.
    pub walkable_floor_z: Option<f32>,
}

impl Default for SweepParams {
    fn default() -> Self {
        Self {
            skip_start_penetration: true,
            ignore_resting_floor_z: None,
            discard_start_penetration: false,
            walkable_floor_z: None,
        }
    }
}

/// Result of a swept query. `t` is the fraction of `end - start` in [0,1].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SweepHit {
    /// Time of impact in [0,1].
    pub t: f32,
    /// Unit contact normal pointing from the triangle toward the box.
    pub normal: Vec3,
    /// Triangle index.
    pub triangle: u32,
    /// Source id of the triangle.
    pub source: u32,
    /// True when the box overlapped/touched the triangle at `t = 0`.
    pub start_penetrating: bool,
}

/// Nearest swept-AABB hit against the world, or `None`.
///
/// Handles zero-length sweeps (a static overlap becomes a `t = 0` `start_penetrating` hit
/// when `skip_start_penetration` is false) and start-overlap (reported, never panics).
pub fn sweep_aabb(
    world: &CollisionWorld,
    start: Vec3,
    end: Vec3,
    half_extents: Vec3,
    params: &SweepParams,
) -> Option<SweepHit> {
    let d = sub(end, start);
    let mut query = Aabb::empty();
    query.include([
        start[0] - half_extents[0],
        start[1] - half_extents[1],
        start[2] - half_extents[2],
    ]);
    query.include([
        start[0] + half_extents[0],
        start[1] + half_extents[1],
        start[2] + half_extents[2],
    ]);
    query.include([
        end[0] - half_extents[0],
        end[1] - half_extents[1],
        end[2] - half_extents[2],
    ]);
    query.include([
        end[0] + half_extents[0],
        end[1] + half_extents[1],
        end[2] + half_extents[2],
    ]);

    let mut best: Option<SweepHit> = None;
    world.for_each_candidate(query, |i| {
        let t = world.triangle(i);
        if let Some(h) = sweep_triangle(start, d, half_extents, t, i, world.source(i)) {
            consider_sweep_hit(&mut best, h, params, d, start, t);
        }
    });
    world.for_each_dynamic_candidate(query, |t, source, _idx| {
        if let Some(h) = sweep_triangle(start, d, half_extents, &t, u32::MAX, source) {
            consider_sweep_hit(&mut best, h, params, d, start, &t);
        }
    });
    best
}

/// Keeps a swept hit when it passes the start-penetration filter and is nearer than `best`.
fn consider_sweep_hit(
    best: &mut Option<SweepHit>,
    mut h: SweepHit,
    params: &SweepParams,
    d: Vec3,
    start: Vec3,
    tri: &Triangle,
) {
    if h.start_penetrating {
        // The engine's MoveActor discards hits within the 2-UU back-off behind the start
        // (see `SweepParams::discard_start_penetration`): the mover escapes geometry it starts
        // flush with/inside instead of deadlocking against it.
        if params.discard_start_penetration
            && params.walkable_floor_z.is_none_or(|z| h.normal[1] < z)
        {
            return;
        }
        if params.skip_start_penetration && !start_drives_into(d, start, tri) {
            return;
        }
        // A walkable floor the box is already touching never blocks: the box can walk along it
        // (and must be free to reach whatever obstacle is behind the grazing contact).
        if let Some(z) = params.ignore_resting_floor_z
            && h.normal[1] >= z
        {
            return;
        }
    }
    if h.t < 0.0 {
        h.t = 0.0;
        h.start_penetrating = true;
    }
    if best.is_none_or(|b| h.t < b.t) {
        *best = Some(h);
    }
}

/// Whether a sweep that starts flush/inside a triangle drives into it: the motion along the
/// triangle's plane normal, oriented from the triangle toward the box start, is negative
/// (beyond a small epsilon so a resting box ignores the floor it stands on).
pub(crate) fn start_drives_into(d: Vec3, start: Vec3, tri: &Triangle) -> bool {
    let n = normalize(cross(sub(tri[1], tri[0]), sub(tri[2], tri[0])));
    let centroid = [
        (tri[0][0] + tri[1][0] + tri[2][0]) / 3.0,
        (tri[0][1] + tri[1][1] + tri[2][1]) / 3.0,
        (tri[0][2] + tri[1][2] + tri[2][2]) / 3.0,
    ];
    let plane = if dot(sub(start, centroid), n) >= 0.0 {
        n
    } else {
        mul(n, -1.0)
    };
    dot(d, plane) < -1e-6
}

/// One AABB/triangle sweep. `d` is the full translation (end - start).
pub(crate) fn sweep_triangle(
    start: Vec3,
    d: Vec3,
    half: Vec3,
    tri: &Triangle,
    index: u32,
    source: u32,
) -> Option<SweepHit> {
    let n = cross(sub(tri[1], tri[0]), sub(tri[2], tri[0]));
    let edges = [
        sub(tri[1], tri[0]),
        sub(tri[2], tri[1]),
        sub(tri[0], tri[2]),
    ];
    let box_axes = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

    // Fixed 13 SAT axes: 3 box axes, the triangle normal, 9 box-edge x triangle-edge.
    let mut axes = [n; 13];
    axes[0] = box_axes[0];
    axes[1] = box_axes[1];
    axes[2] = box_axes[2];
    axes[3] = n;
    let mut k = 4;
    for e in box_axes {
        for f in edges {
            axes[k] = cross(e, f);
            k += 1;
        }
    }

    let mut t_enter = f32::NEG_INFINITY;
    let mut t_exit = f32::INFINITY;
    let mut enter_axis: Option<Vec3> = None;

    for axis in axes {
        let len = length(axis);
        if len < 1e-9 {
            continue;
        }
        let a = mul(axis, 1.0 / len);
        let cb = dot(start, a);
        let rb = half[0] * a[0].abs() + half[1] * a[1].abs() + half[2] * a[2].abs();
        let p = [dot(tri[0], a), dot(tri[1], a), dot(tri[2], a)];
        let tmin0 = p[0].min(p[1]).min(p[2]);
        let tmax0 = p[0].max(p[1]).max(p[2]);
        let vd = dot(d, a);
        // Interval of the triangle (moving by -d) vs the static box interval [cb-rb, cb+rb].
        let lo = tmin0 - cb - rb;
        let hi = tmax0 - cb + rb;
        let (enter, exit) = if vd.abs() < 1e-12 {
            // No motion along this axis. A triangle lying flat in a plane perpendicular to the
            // axis (a floor coplanar with the box bottom, a wall flush with a box side) that only
            // touches the box face is separated: otherwise a box resting on a floor made of
            // several coplanar triangles "hits" the next triangle's leading edge with a
            // horizontal normal and stalls where the step-up has no headroom (item40e: crouching
            // into a 128-UU duct). Any other touching contact keeps the inclusive test (a step
            // landing flush with a deck edge still finds the deck).
            let flat = tmax0 - tmin0 <= TOUCH_EPS;
            let overlapping = if flat {
                lo < -TOUCH_EPS && hi > TOUCH_EPS
            } else {
                lo <= 0.0 && hi >= 0.0
            };
            if overlapping {
                (f32::NEG_INFINITY, f32::INFINITY)
            } else {
                return None;
            }
        } else {
            let a1 = lo / vd;
            let a2 = hi / vd;
            (a1.min(a2), a1.max(a2))
        };
        if enter > t_enter {
            t_enter = enter;
            enter_axis = Some(a);
        }
        if exit < t_exit {
            t_exit = exit;
        }
        if t_enter > t_exit {
            return None;
        }
    }

    // Not reached in the swept segment.
    if t_exit < 0.0 || t_enter > 1.0 {
        return None;
    }

    let mut start_penetrating = false;
    let t = if t_enter <= 0.0 {
        // Overlapping or touching at the start.
        start_penetrating = true;
        0.0
    } else {
        t_enter
    };

    let axis = enter_axis.unwrap_or_else(|| normalize(n));
    let normal = orient_normal(axis, d, start, tri);
    Some(SweepHit {
        t,
        normal,
        triangle: index,
        source,
        start_penetrating,
    })
}

/// Orients a candidate axis as the contact normal pointing from the triangle toward the box.
fn orient_normal(axis: Vec3, d: Vec3, start: Vec3, tri: &Triangle) -> Vec3 {
    let along = dot(axis, d);
    if along > 1e-9 {
        mul(axis, -1.0)
    } else if along < -1e-9 {
        axis
    } else {
        let centroid = [
            (tri[0][0] + tri[1][0] + tri[2][0]) / 3.0,
            (tri[0][1] + tri[1][1] + tri[2][1]) / 3.0,
            (tri[0][2] + tri[1][2] + tri[2][2]) / 3.0,
        ];
        if dot(sub(start, centroid), axis) >= 0.0 {
            axis
        } else {
            mul(axis, -1.0)
        }
    }
}
