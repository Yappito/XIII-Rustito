//! UE2-style walking: floor following plus step-up on non-walkable obstacles.
//!
//! [`walk_move`] is a documented **approximation** of `UPawn::physWalking` + `stepUp`, not a
//! fidelity claim. It exists because [`crate::move_slide`]'s step-up is gated on a
//! near-vertical contact normal, so a small floor rise seen through a near-horizontal contact
//! (for example a plank edge) never triggers it. `walk_move` instead gates step-up on the
//! contact surface being **not walkable** (its up component below `min_floor_z`), and follows
//! the floor after every step.
//!
//! Per step:
//! 1. Sweep the full delta. On no hit, move and finish.
//! 2. On a hit, back off by `skin`. If the hit surface is not walkable, try step-up: sweep up
//!    by `max_step_height`, forward by the remaining travel, then down by `max_step_height`
//!    plus a small epsilon; accept when the landing surface is a walkable floor.
//! 3. Otherwise slide along the hit plane as [`crate::move_slide`] does.
//!
//! After the move, floor-follow: sweep down by `max_step_height`; if a walkable floor is
//! found, snap to it; otherwise report `falling`.

use crate::{
    CollisionWorld, DEFAULT_SKIN, MoveContact, MoveResult, SweepParams, Vec3, add, dot, length,
    mul, sub, sweep_aabb,
};

/// Parameters for [`walk_move`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WalkParams {
    /// Back-off from a surface after impact, in metres (as in [`crate::MoveParams`]).
    pub skin: f32,
    /// Maximum slide iterations per call.
    pub max_iterations: u32,
    /// Maximum step height the walker may climb, in metres. Zero disables step-up and
    /// floor-follow (the result then matches [`crate::move_slide`]'s floor probe).
    pub max_step_height: f32,
    /// A surface is walkable ("floor") when its unit normal's up component is at least this.
    /// UE2's `MINFLOORZ`; the upstream constant is **0.7**.
    pub min_floor_z: f32,
}

impl Default for WalkParams {
    fn default() -> Self {
        Self {
            skin: DEFAULT_SKIN,
            max_iterations: 4,
            max_step_height: 0.0,
            min_floor_z: 0.7,
        }
    }
}

/// Moves an extent box like [`crate::move_slide`], but with UE2-style step-up over
/// non-walkable obstacles and floor following after the move. See the module documentation
/// for the exact approximation.
pub fn walk_move(
    world: &CollisionWorld,
    start: Vec3,
    delta: Vec3,
    half_extents: Vec3,
    params: &WalkParams,
) -> MoveResult {
    let mut pos = start;
    let mut remaining = delta;
    let mut blocked = false;
    let mut contacts = Vec::new();
    let mut iterations = 0u32;

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

        // Step-up on a surface that cannot simply be walked on. Unlike `move_slide`, this is
        // not gated on the hit normal being near-vertical: any hit whose up component is below
        // `min_floor_z` triggers it, including a near-horizontal surface that still blocks
        // forward motion. A genuinely walkable surface is slid along instead.
        let walkable = hit.normal[1] >= params.min_floor_z;
        if params.max_step_height > 1e-6
            && !walkable
            && let Some(stepped) =
                try_step_walk(world, pos, dir, travel, half_extents, params, &mut contacts)
        {
            pos = stepped;
            break;
        }

        // Slide along the contact plane, as `move_slide`.
        let mut leftover = mul(remaining, (1.0 - hit.t).max(0.0));
        let into = dot(leftover, hit.normal);
        if into < 0.0 {
            leftover = sub(leftover, mul(hit.normal, into));
        }
        pos = at;
        remaining = leftover;
    }

    // Floor follow: snap down onto a walkable floor within a step height, else falling.
    let mut on_floor = false;
    let mut falling = false;
    if params.max_step_height > 1e-9 {
        let down = add(
            pos,
            [0.0, -(params.max_step_height + params.skin + 1e-4), 0.0],
        );
        match sweep_aabb(world, pos, down, half_extents, &SweepParams::default()) {
            Some(h) if h.normal[1] >= params.min_floor_z => {
                on_floor = true;
                pos = add(pos, mul(sub(down, pos), h.t));
            }
            _ => falling = true,
        }
    } else if let Some(h) = sweep_aabb(
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
        falling,
    }
}

/// Attempts the UE2 three-sweep step: up by `max_step_height`, forward by `travel`, then down
/// by `max_step_height` plus epsilon. Returns the landing position only when the forward path
/// is clear and the landing surface is a walkable floor.
#[allow(clippy::too_many_arguments)]
fn try_step_walk(
    world: &CollisionWorld,
    pos: Vec3,
    dir: Vec3,
    travel: f32,
    half_extents: Vec3,
    params: &WalkParams,
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
    let down_target = add(
        fwd_target,
        [0.0, -(params.max_step_height + params.skin + 1e-4), 0.0],
    );
    let land = sweep_aabb(
        world,
        fwd_target,
        down_target,
        half_extents,
        &SweepParams::default(),
    )?;
    (land.normal[1] >= params.min_floor_z)
        .then(|| add(fwd_target, mul(sub(down_target, fwd_target), land.t)))
}
