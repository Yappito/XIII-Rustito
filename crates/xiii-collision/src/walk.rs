//! UE2-style walking: floor following plus step-up, verified against `Engine.dll` (item1j).
//!
//! [`walk_move`] is a documented **approximation** of `APawn::physWalking` + `stepUp`, not a
//! byte-fidelity claim. It exists because [`crate::move_slide`]'s step-up is gated on a
//! near-vertical contact normal, so a small floor rise seen through a near-horizontal contact
//! (for example a plank edge) never triggers it.
//!
//! Evidence (**from source**, `llvm-objdump`, read-only; raw disassembly in `local/re/`):
//! - `APawn::stepUp` (`Engine.dll` `0x103baa30`) multiplies the requested step vector by the
//!   fixed up magnitude `35.0` (`0x104829c4`). It also compares the incoming check result's Z
//!   against `0.08` (`0x1048341c`); *hypothesis* (not proven): that separates a near-vertical
//!   wall from a walkable surface.
//! - `APawn::physWalking` (`0x103bdac0`) iterates at most `8` move sub-steps (`cmpl $0x8`) and
//!   uses the constants `0x10483420` (1.9) and `0x10483424` (2.4) beside its floor snap.
//!   Walkability is `MINFLOORZ = 0.7` (`0x10483428`), tested against the **floor**
//!   `FCheckResult.Normal.Z` from a separate downward check, not the horizontal blocking
//!   contact.
//! - `APawn::physWalking`/`stepUp` sweep with the pawn's extent box; `AActor::stepUp`
//!   (`0x103bb0a0`) is the same shape without the ground logic.
//! - `ULevel::MoveActor` (`0x1038a770`) moves the box, then checks blocking actors.
//! - `ATerrainInfo::LineCheck` (`0x10409eb0`) clamps the ray to the base heightmap and indexes
//!   `Vertices[HeightmapX*y + x]`; `UModel::LineCheck` (`0x10419b80`) is a BSP ray/segment with
//!   extent; `UStaticMesh::LineCheck` (`0x10402e00`) dispatches to the per-polygon or simplified
//!   box path. All are **line checks with extent**, not a swept box: the extent expands the
//!   segment's endpoint box, it is not swept continuously.
//!
//! The step here is attempted on **any** blocked horizontal move (a walkable-normal hit while
//! moving horizontally is the edge of a step the pawn is running into), and when the up sweep
//! is blocked it lifts by the swept fraction (the hit time) rather than aborting. Gating the
//! step on the horizontal contact normal instead was measured to regress campaign reach
//! (item1j report, `local/reports/item1j-collision-primitive.md`).
//!
//! Per step:
//! 1. Sweep the full delta. On no hit, move and finish.
//! 2. On a hit, back off by `skin`. Try step-up: sweep up by `max_step_height` (or only as far
//!    as the sweep allows), forward by the remaining travel, then down by `max_step_height`
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

        // Step-up on a blocked horizontal move. The engine gates this on the pawn's **floor**
        // (`APawn::physWalking`, `Engine.dll` 0x103be058: `fcomps MINFLOORZ` on a *separate*
        // downward `FCheckResult` normal, `jp` skips `stepUp` when that floor is unwalkable),
        // not on the horizontal blocking contact's normal. Our `walk_move` does not run a
        // separate floor probe inside the loop, so it approximates the engine by attempting the
        // step on **any** horizontal block (the step's own down-sweep then requires a walkable
        // landing, exactly as `stepUp` does). Gating on the horizontal contact normal instead
        // was measured to regress campaign reach (item1j; see the report), because a vertical
        // obstacle face is unwalkable even when the surface above it is a walkable step.
        if params.max_step_height > 1e-6
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
/// by `max_step_height` plus epsilon. If the up sweep is blocked, only the swept fraction (the
/// hit time) is available, so the step can still clear a low obstacle under a low ceiling.
/// Returns the landing position only when the forward path is clear and the landing surface is
/// a walkable floor.
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
    // UE2 `stepUp` sweeps up by `MAXSTEPHEIGHT`; a blocked sweep lifts by the swept fraction
    // (hit time) and carries on, rather than failing outright.
    let rise = match sweep_aabb(world, pos, up_target, half_extents, &SweepParams::default()) {
        Some(h) => {
            let available = h.t * params.max_step_height - params.skin;
            if available <= params.skin {
                return None;
            }
            available
        }
        None => params.max_step_height,
    };
    let raised = add(pos, [0.0, rise, 0.0]);
    let fwd_target = add(raised, mul(dir, travel.max(0.0)));
    if let Some(h) = sweep_aabb(
        world,
        raised,
        fwd_target,
        half_extents,
        &SweepParams::default(),
    ) {
        contacts.push(MoveContact {
            source: h.source,
            triangle: h.triangle,
            t: h.t,
            normal: h.normal,
            height: raised[1] - pos[1],
            position: raised,
        });
        return None;
    }
    let down = [0.0, -(params.max_step_height + params.skin + 1e-4), 0.0];
    let down_target = add(fwd_target, down);
    let land = sweep_aabb(
        world,
        fwd_target,
        down_target,
        half_extents,
        &SweepParams::default(),
    )?;
    (land.normal[1] >= params.min_floor_z).then(|| add(fwd_target, mul(down, land.t)))
}
