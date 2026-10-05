//! Player footstep synthesis for `--play` (the notify-free path).
//!
//! **How the original triggers footsteps (measured).** `XIIIPlayerPawn.PlayFootStep` is a
//! `simulated event` invoked by the `PlayFootStep` animation notify carried by the player mesh's
//! `Walk`/`Run`/`Strafe` sequences (e.g. `xiiipersos.XIIIM` -> `xiiipersosG.MigA` `Run` has
//! notifies at 0.263 and 0.737 of the clip). The event reads `self.LastCollidedMaterial` and calls
//! `PlaySound(M.XIIIFootStepSound, VSize(Velocity), SoundStepCategory, bSilent)`. The engine sets
//! `LastCollidedMaterial` from the collision trace under the pawn; no decoded script writes it.
//! `XIIIPawn.Landed` calls `PlayLandSound()` on landing (guarded by `LastLandSoundTime`).
//!
//! In this prototype the player pawn has **no third-person animation**, so there is no notify to
//! fire. This module reconstructs the same behaviour from the host simulation:
//!
//! * every fixed step it resolves the surface under the player's feet with the same downward
//!   sweep the engine's `Trace` uses (`Location - (CollisionHeight+CollisionRadius)*Z`), looks up
//!   the floor material's `XIIIFootStepSound`, and
//! * advances a time accumulator matching the decoded `Run`/`Walk` clip cadence (one footstep per
//!   half-cycle) while grounded and moving, plus one on landing.
//!
//! The sound played is the material's `XIIIFootStepSound` wrapper (`XIIIsound.Footsteps__…__hXIIIFoot…`),
//! exactly what the PC (`bReplaceHXScripts == false`) path passes to `PlaySound`; the wrapper's HX
//! program then selects the concrete wave (the engine's own random/sequence choice is not
//! decoded, so the resolver's deterministic candidate is used and labelled).

use xiii_collision::CollisionWorld;
use xiii_decode::common::UNREAL_UNITS_PER_METER;
use xiii_decode::common::to_bevy_position;
use xiii_world::WorldScene;

use crate::collision::{MAXSTEPHEIGHT_UU, MINFLOORZ};
use crate::play::sim::PlayerParams;
use crate::play::sim::PlayerSim;

/// Decoded `Run` clip duration in seconds (`xiiipersosG.MigA.Run`, 21 frames at 30 fps). Two
/// notifies (`PlayFootStep`) per clip, so the per-foot interval is half of this.
pub const RUN_CYCLE_SECONDS: f32 = 21.0 / 30.0;
/// Decoded `Walk` clip duration in seconds (`MigA.Walk`, 31 frames at 30 fps).
pub const WALK_CYCLE_SECONDS: f32 = 31.0 / 30.0;
/// Minimum horizontal speed (UU/s) for the walk/run cadence to advance. Below this the pawn is
/// effectively stopped (XIII's own `bMoving` gate in `PlayFootStep`).
pub const MIN_STEP_SPEED_UU: f32 = 20.0;

/// One collision triangle's surface data for the footstep lookup: the root material's dotted
/// path (evidence) and its player `XIIIFootStepSound` wrapper.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SurfaceEntry {
    /// Dotted object path of the surface material (`XIIIPlage.BATwood05`, `Plage00.TerrainInfo1`),
    /// else `None` (BSP surfaces with no root path, missing slots).
    pub material: Option<String>,
    /// Player `XIIIFootStepSound` wrapper `Sound` path (`XIIIsound.Footsteps__XIIIFSBoi.…`).
    pub sound: Option<String>,
}

/// The floor-surface footstep lookup for a built [`CollisionWorld`]. Parallel to the world's
/// input order (the scene's `collision_box` entries): entry `i` is the surface the `i`-th box
/// triangle belongs to. [`FootstepDriver`] indexes it by `world.origin(triangle)`.
#[derive(Debug, Clone, Default, bevy::prelude::Resource)]
pub struct SurfaceSounds {
    entries: Vec<SurfaceEntry>,
}

impl SurfaceSounds {
    /// Builds the table from an imported scene's box-query collision pool.
    pub fn from_scene(scene: &WorldScene) -> Self {
        let entries = scene
            .collision_box
            .iter()
            .map(|&i| {
                let m = scene.collision_material(i);
                SurfaceEntry {
                    material: m.and_then(|m| m.material_path.clone()),
                    sound: m.and_then(|m| m.footstep_sound.clone()),
                }
            })
            .collect();
        Self { entries }
    }

    /// Builds the table from explicit per-triangle entries (tests).
    #[cfg(test)]
    pub fn from_list(sounds: Vec<Option<String>>) -> Self {
        Self {
            entries: sounds
                .into_iter()
                .map(|sound| SurfaceEntry {
                    material: None,
                    sound,
                })
                .collect(),
        }
    }

    /// Entry of the box triangle `triangle` of `world`, or `None` out of range.
    pub fn entry(&self, world: &CollisionWorld, triangle: u32) -> Option<&SurfaceEntry> {
        self.entries.get(world.origin(triangle) as usize)
    }

    /// Wrapper `Sound` path of the box triangle `triangle` of `world`, or `None`.
    #[cfg(test)]
    pub fn sound(&self, world: &CollisionWorld, triangle: u32) -> Option<&str> {
        self.entry(world, triangle).and_then(|e| e.sound.as_deref())
    }

    /// Number of table entries (diagnostics/tests).
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.entries.len()
    }
}

/// One emitted footstep: the surface material's `XIIIFootStepSound` wrapper path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Footstep {
    /// Wrapper `Sound` object path resolved from the floor material under the player.
    pub sound: String,
    /// Dotted path of the floor material the wrapper was read from (evidence).
    pub material: Option<String>,
}

/// Per-player footstep cadence state.
#[derive(Debug, Clone)]
pub struct FootstepDriver {
    /// Seconds accumulated since the last footstep.
    accum: f32,
    /// Whether the player was grounded on the previous step (to detect the landing edge when the
    /// caller does not pass `sim.landed`).
    was_grounded: bool,
    /// Last horizontal location (Unreal UU), to measure actual displacement. A pawn pressing
    /// into a wall keeps a non-zero commanded velocity in this simulation but does not move, so
    /// velocity alone is not a reliable "walking" test (the engine's own `bMoving` follows the
    /// physics result).
    last_xy: Option<[f32; 2]>,
}

impl Default for FootstepDriver {
    fn default() -> Self {
        Self::new()
    }
}

impl FootstepDriver {
    /// A fresh driver (no accumulated phase).
    pub fn new() -> Self {
        Self {
            accum: 0.0,
            was_grounded: true,
            last_xy: None,
        }
    }

    /// Advances one fixed step. Returns the footstep to play this tick, if any.
    ///
    /// The surface under the player is found by the same downward sweep
    /// [`crate::play::sim`] uses to keep the box on the floor. A landing always emits (the
    /// `Landed` -> `PlayLandSound` path) when a surface sound is available; walking/running emits
    /// on the decoded clip cadence. `input_walk` selects the `Walk` clip timing (Shift).
    pub fn advance(
        &mut self,
        dt: f32,
        sim: &PlayerSim,
        params: &PlayerParams,
        world: &CollisionWorld,
        surfaces: &SurfaceSounds,
        input_walk: bool,
    ) -> Option<Footstep> {
        let landed = sim.landed || (sim.grounded && !self.was_grounded);
        self.was_grounded = sim.grounded;

        // Actual horizontal displacement since the last step (not the commanded velocity).
        let xy = [sim.location[0], sim.location[1]];
        let moved_uu = self
            .last_xy
            .map(|p| ((xy[0] - p[0]).powi(2) + (xy[1] - p[1]).powi(2)).sqrt())
            .unwrap_or(0.0);
        self.last_xy = Some(xy);

        if !sim.grounded {
            self.accum = 0.0;
            return None;
        }
        let speed = (sim.velocity[0] * sim.velocity[0] + sim.velocity[1] * sim.velocity[1]).sqrt();
        // Both are required: a commanded speed with no displacement is a pawn pressed against a
        // wall; a displacement without speed is a teleport/push (not a walking step).
        let moving = speed >= MIN_STEP_SPEED_UU && moved_uu > 1e-4;

        // The interval matches the clip that would be playing: the crouch/walk clip when the
        // Shift walk modifier or crouch is active (the host does not animate, so this is the
        // documented approximation of XIIIPawn.AnimateWalking/AnimateRunning).
        let cycle = if sim.crouched || input_walk {
            WALK_CYCLE_SECONDS
        } else {
            RUN_CYCLE_SECONDS
        };
        let interval = (cycle * 0.5).max(0.05);

        if landed {
            // `PlayLandSound` on touch-down (the engine's `LastLandSoundTime` gate is 0.333 s;
            // `Landed` is already edge-triggered by the caller, so no extra gate is needed).
            self.accum = 0.0;
            return self.surface_footstep(sim, params, world, surfaces);
        }

        if !moving {
            self.accum = 0.0;
            return None;
        }

        self.accum += dt;
        if self.accum < interval {
            return None;
        }
        self.accum -= interval;
        self.surface_footstep(sim, params, world, surfaces)
    }

    /// The footstep under the player (`None` when no walkable floor is found or the surface has
    /// no `XIIIFootStepSound`).
    ///
    /// The engine's `PlayFootStep` uses `Trace` (a **line**, not a box): it traces from
    /// `Location - (CollisionHeight+CollisionRadius)*Z` up to `Location` and takes the material
    /// of the hit. A line query is used here for the same reason — a box sweep reports the wall
    /// the player box is flush against at `t = 0` and masks the floor. A vertical ray from the
    /// box centre down to well below the feet finds the floor (or a ceiling the box is inside),
    /// and only a walkable normal is accepted.
    fn surface_footstep(
        &self,
        sim: &PlayerSim,
        params: &PlayerParams,
        world: &CollisionWorld,
        surfaces: &SurfaceSounds,
    ) -> Option<Footstep> {
        let center = to_bevy_position(sim.location);
        let reach = params.height_uu + params.radius_uu + MAXSTEPHEIGHT_UU;
        let down = [
            center[0],
            center[1] - reach / UNREAL_UNITS_PER_METER,
            center[2],
        ];
        let hit = world
            .ray(center, down)
            .filter(|h| h.normal[1] >= MINFLOORZ)?;
        let entry = surfaces.entry(world, hit.triangle)?;
        Some(Footstep {
            sound: entry.sound.clone()?,
            material: entry.material.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_collision::Triangle;

    /// A large flat floor at Bevy Y = 0 with source id 7.
    fn flat_floor() -> CollisionWorld {
        let t: Vec<(Triangle, u32)> = vec![
            (
                [[-50.0, 0.0, -50.0], [50.0, 0.0, -50.0], [50.0, 0.0, 50.0]],
                7,
            ),
            (
                [[-50.0, 0.0, -50.0], [50.0, 0.0, 50.0], [-50.0, 0.0, 50.0]],
                7,
            ),
        ];
        CollisionWorld::new(t)
    }

    fn surfaces(sound: Option<&str>) -> SurfaceSounds {
        SurfaceSounds::from_list(vec![sound.map(str::to_owned); 2])
    }

    #[test]
    fn table_len_is_tracked() {
        assert_eq!(surfaces(None).len(), 2);
    }

    fn params() -> PlayerParams {
        PlayerParams {
            radius_uu: 34.0,
            height_uu: 75.0,
            crouch_radius_uu: 34.0,
            crouch_height_uu: 48.0,
            base_eye_height_uu: 60.0,
            ground_speed: 472.0,
            jump_z: 420.0,
            accel_rate: None,
            air_control: None,
            walking_pct: Some(0.5),
            crouching_pct: 0.3,
            water_speed: 300.0,
            ladder_speed: 200.0,
            buoyancy: 99.0,
            under_water_time: 19.0,
            b_can_crouch: true,
            b_can_climb_ladders: true,
            max_fall_speed: Some(1200.0),
            gravity_z: -950.0,
        }
    }

    /// The table maps the world's hit triangle back to its input order.
    #[test]
    fn surface_table_maps_by_world_origin() {
        let world = flat_floor();
        let s = surfaces(Some("XIII.Ft.Met"));
        assert_eq!(s.len(), 2);
        // The floor is hit on one of the two triangles; both carry the same sound.
        let hit = world
            .ray([0.0, 5.0, 0.0], [0.0, -5.0, 0.0])
            .expect("floor hit");
        assert_eq!(s.sound(&world, hit.triangle), Some("XIII.Ft.Met"));
    }

    /// Standing still: no footsteps (the `bMoving` gate).
    #[test]
    fn still_player_emits_no_footsteps() {
        let world = flat_floor();
        let surfaces = surfaces(Some("XIII.Ft.Met"));
        let mut sim = PlayerSim::new([0.0, 75.0, 0.0], 0.0);
        sim.grounded = true;
        let mut driver = FootstepDriver::new();
        for _ in 0..120 {
            assert!(
                driver
                    .advance(1.0 / 60.0, &sim, &params(), &world, &surfaces, false)
                    .is_none()
            );
        }
    }

    /// Test environment: the flat floor, the surface table and player params.
    struct Env {
        world: CollisionWorld,
        surfaces: SurfaceSounds,
        params: PlayerParams,
    }

    impl Env {
        fn new(sound: Option<&str>) -> Self {
            Self {
                world: flat_floor(),
                surfaces: surfaces(sound),
                params: params(),
            }
        }

        /// Runs the driver for `ticks` steps, moving the pawn by `vx*dt` each tick (the real
        /// `PlayerSim` updates `location` from the physics step; the driver requires actual
        /// displacement, so the test must move it too). Returns the footstep count.
        fn run(
            &self,
            driver: &mut FootstepDriver,
            sim: &mut PlayerSim,
            vx: f32,
            walk: bool,
            ticks: usize,
        ) -> usize {
            let dt = 1.0 / 60.0;
            let mut n = 0;
            for _ in 0..ticks {
                sim.location[0] += vx * dt;
                if driver
                    .advance(dt, sim, &self.params, &self.world, &self.surfaces, walk)
                    .is_some()
                {
                    n += 1;
                }
            }
            n
        }
    }

    /// Running for the decoded `Run` clip duration emits ~two footsteps (the two notifies), with
    /// the wrapper path from the floor material.
    #[test]
    fn running_emits_two_footsteps_per_decoded_run_cycle() {
        let world = flat_floor();
        let surfaces = surfaces(Some("XIII.Ft.Met"));
        let mut sim = PlayerSim::new([0.0, 75.0, 0.0], 0.0);
        sim.grounded = true;
        sim.velocity = [params().ground_speed, 0.0, 0.0];
        let mut driver = FootstepDriver::new();
        let dt = 1.0 / 60.0;
        let mut steps = 0;
        for _ in 0..(RUN_CYCLE_SECONDS / dt).round() as usize {
            sim.location[0] += sim.velocity[0] * dt;
            if let Some(f) = driver.advance(dt, &sim, &params(), &world, &surfaces, false) {
                assert_eq!(f.sound, "XIII.Ft.Met");
                steps += 1;
            }
        }
        // Two notifies per cycle; the accumulator may land one tick either side.
        assert!(
            (1..=3).contains(&steps),
            "expected ~2 footsteps for one Run cycle, got {steps}"
        );
    }

    /// Walking (Shift) uses the longer `Walk` clip interval, so it emits fewer steps than running
    /// over the same wall time.
    #[test]
    fn walking_cadence_is_slower_than_running() {
        let env = Env::new(Some("XIII.Ft.Met"));
        let mut walk = PlayerSim::new([0.0, 75.0, 0.0], 0.0);
        walk.grounded = true;
        walk.velocity = [236.0, 0.0, 0.0];
        let mut run = walk.clone();
        run.velocity = [472.0, 0.0, 0.0];
        let mut dw = FootstepDriver::new();
        let mut dr = FootstepDriver::new();
        let w = env.run(&mut dw, &mut walk, 236.0, true, 90);
        let r = env.run(&mut dr, &mut run, 472.0, false, 90);
        assert!(
            w < r,
            "walking ({w}) must emit fewer steps than running ({r})"
        );
    }

    /// A pawn pressed against a wall has a commanded velocity but no displacement: it must not
    /// emit footsteps (this is the failure mode the box `sweep` produced before the line trace).
    #[test]
    fn blocked_pawn_does_not_emit_footsteps() {
        let world = flat_floor();
        let surfaces = surfaces(Some("XIII.Ft.Met"));
        let mut sim = PlayerSim::new([0.0, 75.0, 0.0], 0.0);
        sim.grounded = true;
        sim.velocity = [472.0, 0.0, 0.0]; // commanded, but location never changes
        let mut driver = FootstepDriver::new();
        let mut any = false;
        for _ in 0..120 {
            any |= driver
                .advance(1.0 / 60.0, &sim, &params(), &world, &surfaces, false)
                .is_some();
        }
        assert!(!any, "a blocked pawn must not emit footsteps");
    }

    /// A landing always emits exactly one footstep even with no horizontal speed.
    #[test]
    fn landing_emits_one_footstep() {
        let env = Env::new(Some("XIII.Ft.Sab"));
        let mut sim = PlayerSim::new([0.0, 75.0, 0.0], 0.0);
        sim.grounded = false;
        sim.velocity = [0.0, 0.0, -300.0];
        let mut driver = FootstepDriver::new();
        // First tick airborne: nothing.
        assert!(
            driver
                .advance(
                    1.0 / 60.0,
                    &sim,
                    &env.params,
                    &env.world,
                    &env.surfaces,
                    false
                )
                .is_none()
        );
        // Land.
        sim.grounded = true;
        sim.landed = true;
        let f = driver
            .advance(
                1.0 / 60.0,
                &sim,
                &env.params,
                &env.world,
                &env.surfaces,
                false,
            )
            .expect("landing must emit a footstep");
        assert_eq!(f.sound, "XIII.Ft.Sab");
        // The next grounded tick is not a landing.
        sim.landed = false;
        assert!(
            driver
                .advance(
                    1.0 / 60.0,
                    &sim,
                    &env.params,
                    &env.world,
                    &env.surfaces,
                    false
                )
                .is_none()
        );
    }

    /// No floor material -> no footstep (never a fabricated sound), even while moving.
    #[test]
    fn surface_without_footstep_sound_is_silent() {
        let env = Env::new(None);
        let mut sim = PlayerSim::new([0.0, 75.0, 0.0], 0.0);
        sim.grounded = true;
        sim.velocity = [472.0, 0.0, 0.0];
        let mut driver = FootstepDriver::new();
        let n = env.run(&mut driver, &mut sim, 472.0, false, 120);
        assert_eq!(n, 0, "a surface with no footstep sound must stay silent");
    }
}
