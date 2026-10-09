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
//!   Walkability is `MINFLOORZ = 0.7` (`0x10483428`). item1k re-read the branch at `0x103be058`
//!   (`fld Hit.Normal.Z`; `fcomp 0x10483428`; `fnstsw ax`; `test ah,0x5`; `jp 0x103be547`). The
//!   parity test is taken when `Normal.Z > 0.7` (greater or unordered), so **a walkable contact
//!   enters the `0x103be547` body and an unwalkable contact falls through to `0x103be06c`**.
//!   Which body is the stair-step and which is the slope-walk is **hypothesis** (both rebuild a
//!   delta from `Hit.Normal`/`MAXSTEPHEIGHT` and call the extent `Move`). `FCheckResult.Normal.Z`
//!   is at struct offset `0x14` and `Time` at `0x24`.
//! - `APawn::physWalking`/`stepUp` sweep with the pawn's extent box; `AActor::stepUp`
//!   (`0x103bb0a0`) is the same shape without the ground logic.
//! - `ULevel::MoveActor` (`0x1038a770`) moves the box, then checks blocking actors. item27k
//!   measured its world-check back-off, corrected by item27n: the check segment is extended
//!   2 UU forward beyond the requested end (`+2.0` at `0x1038a981`, unit dir from
//!   `1.0/|delta|`) and a world hit inside that back-off
//!   (`(2+|delta|)*t_hit <= 2`, branch at `0x1038aba8`) zeroes the delta and hit time
//!   (`0x1038abaf`), blocking movement. It is not a discard permitting penetration. This
//!   crate's `SweepParams::discard_start_penetration` remains an explicit **host approximation**
//!   for genuinely embedded triangle-soup starts; touching faces now block. Walkable floors
//!   are exempt, as they are handled by the floor machinery.
//! - `ATerrainInfo::LineCheck` (`0x10409eb0`) clamps the ray to the base heightmap and indexes
//!   `Vertices[HeightmapX*y + x]`; `UModel::LineCheck` (`0x10419b80`) is a BSP ray/segment with
//!   extent; `UStaticMesh::LineCheck` (`0x10402e00`) dispatches to the per-polygon or simplified
//!   box path. All are **line checks with extent**, not a swept box: the extent expands the
//!   segment's endpoint box, it is not swept continuously.
//!
//! Per sub-step. **Measured:** the branch at `0x103be058` keys on the horizontal `MoveActor`
//! contact normal (a walkable contact enters `0x103be547`, an unwalkable one falls through to
//! `0x103be06c`). **Approximated (hypothesis, chosen by measurement):** the floor gate, the
//! resting-floor ignore and the down-sweep distance, itemised below.
//!
//! 1. Sweep the full delta. On no hit, move and finish.
//! 2. On a hit, back off by `skin` and record the contact.
//! 3. If a downward probe of `max_step_height * 37/35` finds a walkable floor, attempt the
//!    three-sweep step-up (`try_step_walk`); otherwise slide. The `0x104831ac` = `37.0` UU
//!    constant is **measured**, but no branch using its result to gate `stepUp` was located in
//!    `APawn::physWalking`, so [`probe_floor`] and this gate are a **host approximation** (it
//!    held reach and removed the walkable-ledge stalls), not a proven engine gate. The step's
//!    up/forward sweeps ignore a resting walkable floor, and its down-sweep spans the raise plus
//!    `max_step_height`; both are **host approximations** (labelled on [`try_step_walk`]) that
//!    stop a grazing near-horizontal floor from defeating the step (item1k's walkable-ledge fix).
//! 4. Otherwise slide the remainder along the contact plane, as `move_slide`.
//!
//! item1j measured that gating the step on the contact normal (step only on a walkable contact)
//! regresses reach; item1k measured that *sliding* a walkable contact instead of stepping it
//! regresses far more.
//!
//! After the move, floor-follow: sweep down by `max_step_height`; if a walkable floor is
//! found, snap to it; otherwise report `falling`.

use crate::{
    CollisionWorld, DEFAULT_SKIN, MoveContact, MoveResult, SweepHit, SweepParams, Vec3, add, dot,
    length, mul, sub, sweep_aabb,
};

/// The `37` UU probe distance relative to `max_step_height`. **Measured constant:** `.rdata`
/// `0x104831ac` = `37.0` UU, multiplied by a ±1 sign at `0x103bdd51`; `APawn::stepUp` uses
/// `MAXSTEPHEIGHT` = `35.0` UU (`0x104829c4`). `max_step_height` is the caller's 35 UU in
/// metres, so `37/35` reproduces the 37 UU distance without hard-coding the project's 90 UU/m.
/// **Hypothesis:** that the engine uses this value as a floor check whose result gates `stepUp`
/// is not proven here (no such branch was located); see [`probe_floor`].
pub const FLOOR_PROBE_RATIO: f32 = 37.0 / 35.0;

/// Sweep parameters for walk movement. Overlap recovery for genuinely embedded starts is
/// a host approximation, not `ULevel::MoveActor`'s back-off (see module evidence and
/// [`SweepParams::discard_start_penetration`]). Touching faces still block motion into them.
/// Walk turns recovery on for every sub-sweep; `move_slide` keeps the default.
fn walk_sweep_params(
    ignore_resting_floor_z: Option<f32>,
    walkable_floor_z: Option<f32>,
) -> SweepParams {
    SweepParams {
        ignore_resting_floor_z,
        discard_start_penetration: true,
        walkable_floor_z,
        ..SweepParams::default()
    }
}

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
    /// UE2's `MINFLOORZ`, **measured in XIII's `Engine.dll`** (`0x10483428` = `0.7`).
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

/// Moves an extent box like [`crate::move_slide`], but with a downward floor probe
/// (**measurement-chosen approximation**, not a proven engine branch), a three-sweep step-up on
/// a blocked move and floor following after the move. See the module documentation for the exact
/// port, its approximations and its limits.
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
        let hit = match sweep_aabb(
            world,
            pos,
            target,
            half_extents,
            &walk_sweep_params(None, Some(params.min_floor_z)),
        ) {
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

        // Step-up on a blocked horizontal move, gated on a downward floor probe. **Host
        // approximation (hypothesis):** `0x104831ac` = 37 UU is a measured constant, but no
        // `physWalking` compare/branch uses its result to gate `stepUp`, so this gate is chosen
        // by measurement (it held reach and removed the walkable-ledge stalls). Measured branch:
        // `0x103be058` sends a walkable contact to `0x103be547` and an unwalkable one to
        // `0x103be06c`; `try_step_walk` covers both by attempting on any block and requiring a
        // walkable landing. (item1j measured a contact-normal gate to regress; item1k measured a
        // pure walkable-slide to regress far more.)
        if params.max_step_height > 1e-6
            && probe_floor(world, pos, half_extents, params).is_some()
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

/// A downward floor probe used to gate [`try_step_walk`]: sweep straight down by
/// `max_step_height * [`FLOOR_PROBE_RATIO`]` (the `0x104831ac` = 37 UU constant is **measured**)
/// and return the hit when the surface is walkable (`MINFLOORZ`). **Approximation
/// (hypothesis):** no branch using that 37 UU result to gate `stepUp` was located in
/// `APawn::physWalking`; this gate is **chosen by measurement** (it holds reach and removes the
/// walkable-ledge stalls), not a proven engine gate. `None` when step-up is disabled or no floor
/// is under the pawn.
fn probe_floor(
    world: &CollisionWorld,
    center: Vec3,
    half_extents: Vec3,
    params: &WalkParams,
) -> Option<SweepHit> {
    if params.max_step_height <= 1e-9 {
        return None;
    }
    let down = add(
        center,
        [0.0, -(params.max_step_height * FLOOR_PROBE_RATIO), 0.0],
    );
    sweep_aabb(world, center, down, half_extents, &SweepParams::default())
        .filter(|h| h.normal[1] >= params.min_floor_z)
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
    // Host approximation (item1k): rising and moving forward must ignore a walkable floor the
    // box is already touching, otherwise on a slightly inclined floor the grazing `t = 0`
    // contact blocks the up-sweep and the whole step fails. The down-sweep keeps the default
    // behaviour so it still finds the landing floor. This is not a proven engine rule.
    let step_params = walk_sweep_params(Some(params.min_floor_z), Some(params.min_floor_z));
    let up = [0.0, params.max_step_height, 0.0];
    let up_target = add(pos, up);
    // UE2 `stepUp` sweeps up by `MAXSTEPHEIGHT`; a blocked sweep lifts by the swept fraction
    // (hit time) and carries on, rather than failing outright.
    let rise = match sweep_aabb(world, pos, up_target, half_extents, &step_params) {
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
    if let Some(h) = sweep_aabb(world, raised, fwd_target, half_extents, &step_params) {
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
    // Host approximation (item1k): sweep down by the raise plus `max_step_height`, so the
    // landing can be up to `max_step_height` below the raised point and the extra `rise` reaches
    // a floor the box was floating above (a grazing walkable slope raises the box only by
    // `rise`). The nearest hit is the landing, so a longer sweep never skips a higher step top.
    // The engine's own down distance is not proven to include `rise`.
    let down = [
        0.0,
        -(rise + params.max_step_height + params.skin + 1e-4),
        0.0,
    ];
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
