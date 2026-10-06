//! Port of `ULevel::FindSpot` (`Engine.dll` `0x1038a080`, item1k).
//!
//! The engine's placement search (`ULevel::FindSpot(FVector Extent, FVector& Location)`) tests
//! the requested location with the actor's extent, and if it is blocked searches four candidate
//! positions at `(±0.5*Extent.X, ±0.5*Extent.Y)` around it, stopping once two free positions are
//! found. If exactly one candidate is free it extrapolates to twice the offset
//! (`Location = 2*Test - Original`). A zero extent fails immediately; no free candidate fails.
//! The engine then runs a final placement trace before returning success.
//!
//! Control flow read from the disassembly (all addresses in `XIII_Game/system/Engine.dll`):
//! - `0x1038a0f0`-`0x1038a123`: first `LineCheck`-style probe at the given location; a return of
//!   `1` is the "free" success at `0x1038a3e8`.
//! - `0x1038a12c`-`0x1038a15a`: `Extent.IsZero()` returns `0` (`0x1038a15c`).
//! - `0x1038a1f0`-`0x1038a2c8`: the corner loop. The outer index starts at `-1` and steps by `2`
//!   (`0x1038a2c5`), the inner likewise (`0x1038a2ba`), so the candidates are the four corners
//!   `(±0.5*Extent.X, ±0.5*Extent.Y)`. Offsets are built at `0x1038a218`-`0x1038a244`; the probe
//!   and `found++` / `Location = Test` update are at `0x1038a284`-`0x1038a2b2`; the loop stops
//!   at `found >= 2` (`0x1038a210`).
//! - `0x1038a2cd`-`0x1038a316`: `found == 0` fails (`0x1038a15c`); `found == 1` applies
//!   `Location = 2*Location - Original` (`0x1038a2f0`-`0x1038a316`).
//! - `0x1038a319`-`0x1038a33e`: a final placement probe; `false` returns `0`.
//!
//! This crate is a static triangle soup with no actor layer, so the engine's final
//! "encroaching actors" trace is represented by a plain overlap test, and the caller's
//! settle-onto-floor step (the reach harness previously dropped 3 m) is kept as an explicit
//! `drop` parameter. Both are labelled in the report; the corner search and the
//! single-candidate extrapolation are the engine's own.
//!
//! **Hypothesis (not proven):** the virtual calls at `0x1038a11d` (`vtable + 0xcc`) and
//! `0x1038a1aa`/`0x1038a338` (`vtable + 0xc4`) are `ULevel::SingleLineCheck`-style placement
//! probes; only their return polarity (`1` = free) is used here.

use crate::{CollisionWorld, SweepParams, Vec3, add, mul, sub, sweep_aabb};

/// Parameters for [`find_spot`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FindSpotParams {
    /// A landing surface is a floor when its unit normal's up component is at least this
    /// (`MINFLOORZ`, measured `0.7` in `Engine.dll` `0x10483428`).
    pub min_floor_z: f32,
    /// Downward distance the chosen spot is settled onto the floor, in metres. The engine's
    /// final placement probe is reproduced by this caller-supplied drop.
    pub drop: f32,
    /// Vertical raise increment (metres) used only when the corner search finds no free spot,
    /// i.e. the box is embedded. Zero disables the fallback and returns
    /// [`FindSpotError::NoFreeSpot`]. The engine's `FindSpot` has no raise; this reproduces the
    /// previous reach placement's vertical search as the caller's final probe (see the report).
    pub raise_step: f32,
    /// Cap on the vertical raise fallback (metres).
    pub max_raise: f32,
}

impl Default for FindSpotParams {
    fn default() -> Self {
        Self {
            min_floor_z: 0.7,
            drop: 3.0,
            raise_step: 0.0,
            max_raise: 0.0,
        }
    }
}

/// Why [`find_spot`] could not place the box.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FindSpotError {
    /// The extent had a non-positive half-size; the engine returns `0` for a zero extent.
    ZeroExtent,
    /// No candidate position was free (the engine's `found == 0`), including the extrapolated
    /// single-candidate position failing the final probe.
    NoFreeSpot,
    /// A free spot was found but nothing walkable was under it within the drop.
    NoFloor,
}

impl std::fmt::Display for FindSpotError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FindSpotError::ZeroExtent => write!(f, "FindSpot extent is zero"),
            FindSpotError::NoFreeSpot => {
                write!(
                    f,
                    "spawn box still overlaps after the FindSpot corner search"
                )
            }
            FindSpotError::NoFloor => write!(f, "no floor was found below the FindSpot location"),
        }
    }
}

/// Result of [`find_spot`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FindSpot {
    /// Final box centre (Bevy metres).
    pub position: Vec3,
    /// Floor height under the box (`position.y - half.y`).
    pub floor: f32,
    /// Horizontal offset the search applied from the requested centre (Bevy metres).
    pub offset: Vec3,
    /// Number of free corner candidates found, or `None` when the requested centre was already
    /// free (the engine returns success without searching).
    pub free_corners: Option<usize>,
}

/// Port of `ULevel::FindSpot`: places an axis-aligned extent box whose centre is requested at
/// `desired_center` by searching the engine's four corner offsets when the requested centre is
/// blocked, then settling it down onto a walkable floor. See the module documentation.
pub fn find_spot(
    world: &CollisionWorld,
    desired_center: Vec3,
    half_extents: Vec3,
    params: &FindSpotParams,
) -> Result<FindSpot, FindSpotError> {
    if half_extents[0] <= 0.0 || half_extents[1] <= 0.0 || half_extents[2] <= 0.0 {
        return Err(FindSpotError::ZeroExtent);
    }

    let (chosen, free_corners) = if world.overlap_aabb(desired_center, half_extents).is_empty() {
        // The engine checks the requested location first and returns success unchanged.
        (desired_center, None)
    } else {
        // The engine's four corners: `(±0.5*Extent.X, ±0.5*Extent.Y, 0)`, Bevy X/Z
        // horizontal. The outer and inner indices step by 2 from -1 (Engine 0x1038a2ba).
        let dx = 0.5 * half_extents[0];
        let dz = 0.5 * half_extents[2];
        let mut found = 0usize;
        let mut last = desired_center;
        'corners: for j in [-1.0f32, 1.0] {
            for i in [-1.0f32, 1.0] {
                let test = [
                    desired_center[0] + j * dx,
                    desired_center[1],
                    desired_center[2] + i * dz,
                ];
                if world.overlap_aabb(test, half_extents).is_empty() {
                    found += 1;
                    last = test;
                    if found >= 2 {
                        break 'corners;
                    }
                }
            }
        }
        let chosen: Vec3 = if found == 0 {
            // No corner is free: the box is embedded. The engine returns failure here; the
            // reach caller's previous placement raised vertically, so reproduce that as an
            // explicit fallback (disabled when `raise_step`/`max_raise` are zero).
            // `free_corners = Some(0)` marks the raised placement (no corner was free).
            if params.raise_step <= 0.0 || params.max_raise <= 0.0 {
                return Err(FindSpotError::NoFreeSpot);
            }
            let mut y = desired_center[1];
            let mut raised = 0.0f32;
            loop {
                let center = [desired_center[0], y, desired_center[2]];
                if world.overlap_aabb(center, half_extents).is_empty() {
                    break center;
                }
                if raised >= params.max_raise {
                    return Err(FindSpotError::NoFreeSpot);
                }
                y += params.raise_step;
                raised += params.raise_step;
            }
        } else {
            let chosen = if found == 1 {
                // `Location = 2*Test - Original` (`0x1038a2d5`).
                [
                    2.0 * last[0] - desired_center[0],
                    2.0 * last[1] - desired_center[1],
                    2.0 * last[2] - desired_center[2],
                ]
            } else {
                last
            };
            // The engine's final probe (`0x1038a319`) refuses a placement that still
            // overlaps.
            if !world.overlap_aabb(chosen, half_extents).is_empty() {
                return Err(FindSpotError::NoFreeSpot);
            }
            chosen
        };
        (chosen, Some(found))
    };

    // Settle straight down onto a walkable floor (the engine's final placement trace, made
    // explicit as the caller-visible drop).
    let down = [chosen[0], chosen[1] - params.drop, chosen[2]];
    let landed = match sweep_aabb(world, chosen, down, half_extents, &SweepParams::default()) {
        Some(h) if h.normal[1] >= params.min_floor_z => add(chosen, mul(sub(down, chosen), h.t)),
        _ => return Err(FindSpotError::NoFloor),
    };
    Ok(FindSpot {
        position: landed,
        floor: landed[1] - half_extents[1],
        offset: [
            landed[0] - desired_center[0],
            0.0,
            landed[2] - desired_center[2],
        ],
        free_corners,
    })
}
