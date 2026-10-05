//! Deterministic first-person movement simulation, in Unreal units.
//!
//! This is the single movement core used by both the interactive `--play` window and the
//! headless `--play-script` driver. It is a **movement prototype**: there is no script VM,
//! no weapons and no AI.
//!
//! State is kept in Unreal units (X forward, Y right, Z up) and Unreal units per second.
//! `xiii-collision` works in Bevy metres (Y up), so each fixed step converts the box centre
//! and the step delta through the single coordinate policy in `xiii_decode::common`
//! ([`to_bevy_position`]) and converts the result back with
//! [`xiii_world::physics::bevy_to_unreal_position`]. No scale constant is duplicated here.
//!
//! Grounded walking uses `xiii_collision::walk_move` (UE2-style step-up at
//! [`MAXSTEPHEIGHT_UU`] and floor following), which is the same primitive `--collision-test`
//! and `--reach-test` use. Falling applies gravity and `AirControl`, and lands on a
//! walkable floor (`MINFLOORZ`).

use xiii_collision::{
    CollisionWorld, MoveParams, SweepHit, Vec3, WalkParams, move_slide, walk_move,
};
use xiii_decode::common::{UNREAL_UNITS_PER_METER, to_bevy_position};
use xiii_world::physics::{
    bevy_to_unreal_direction, bevy_to_unreal_position, unreal_extent_to_bevy,
};

use crate::collision::{MAXSTEPHEIGHT_UU, MINFLOORZ, SKIN_UU};

/// Downward probe distance used to decide whether an airborne box has landed, in metres. Small
/// relative to the player's 75 UU half height and to `MAXSTEPHEIGHT` (35 UU = 0.389 m).
const LAND_PROBE_M: f32 = 0.02;

/// Maximum number of slide iterations per move call (shared by both primitives).
const MAX_ITERATIONS: u32 = 4;

/// Resolved player parameters.
///
/// Every field is either decoded from `XIII.XIIIPlayerPawn`'s inherited class defaults or
/// carries an explicit, reported source. `accel_rate == None` means "no decoded rate": the
/// simulation then accelerates instantly (documented in the startup report).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PlayerParams {
    /// Collision cylinder radius (Unreal units, horizontal).
    pub radius_uu: f32,
    /// Collision cylinder half height (Unreal units, vertical).
    pub height_uu: f32,
    /// Standing eye height above the box centre (Unreal units).
    pub base_eye_height_uu: f32,
    /// Ground speed (Unreal units/s).
    pub ground_speed: f32,
    /// Jump launch velocity (Unreal units/s, positive up).
    pub jump_z: f32,
    /// Horizontal ground acceleration (Unreal units/s^2); `None` => instant.
    pub accel_rate: Option<f32>,
    /// Fraction of `accel_rate` available while airborne; `None` => 0.
    pub air_control: Option<f32>,
    /// `GroundSpeed` multiplier while walking (Shift); `None` => 0.5 (documented).
    pub walking_pct: Option<f32>,
    /// Downward speed clamp while falling (Unreal units/s); `None` => unclamped.
    pub max_fall_speed: Option<f32>,
    /// Gravity (Unreal units/s^2, negative = down).
    pub gravity_z: f32,
}

impl PlayerParams {
    /// Bevy-space half extents `(radius, half height, radius)` in metres for the extent box.
    pub fn half_extents_bevy(&self) -> Vec3 {
        unreal_extent_to_bevy([self.radius_uu, self.radius_uu, self.height_uu])
    }
}

/// One fixed step of player input. `forward`/`right` are axis values in `[-1, 1]`.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Input {
    /// Forward/back axis (+forward).
    pub forward: f32,
    /// Strafe axis (+right).
    pub right: f32,
    /// A jump was requested this tick (edge-triggered by the caller).
    pub jump: bool,
    /// Walk modifier (Shift): scale `GroundSpeed` by `walking_pct`.
    pub walk: bool,
    /// A use/interact action was requested this tick (edge-triggered; `E`). Not consumed by
    /// [`PlayerSim::step`]; the host performs the use against the VM.
    pub use_action: bool,
    /// A fire action was requested this tick (edge-triggered; left mouse / script `fire`). Not
    /// consumed by [`PlayerSim::step`]; the host routes it to the player's weapon (item14).
    pub fire: bool,
}

/// The simulated player.
#[derive(Debug, Clone)]
pub struct PlayerSim {
    /// Box centre in Unreal units.
    pub location: [f32; 3],
    /// Velocity in Unreal units/s.
    pub velocity: [f32; 3],
    /// Yaw in radians (Unreal convention: +X forward, +Y right).
    pub yaw: f32,
    /// Pitch in radians (positive up).
    pub pitch: f32,
    /// True when resting on (or stepping along) a walkable floor.
    pub grounded: bool,
    /// Unreal-space normal of the floor under the box, last time it was grounded.
    pub floor_normal: [f32; 3],
    /// Human-readable source of the last blocking contact, if any.
    pub last_source: Option<String>,
}

impl PlayerSim {
    /// Creates a player at a box-centre location with the given yaw.
    pub fn new(location: [f32; 3], yaw: f32) -> Self {
        Self {
            location,
            velocity: [0.0; 3],
            yaw,
            pitch: 0.0,
            grounded: true,
            floor_normal: [0.0, 0.0, 1.0],
            last_source: None,
        }
    }

    /// Camera eye location in Unreal units (`base_eye_height` above the box centre).
    pub fn eye_location(&self, params: &PlayerParams) -> [f32; 3] {
        [
            self.location[0],
            self.location[1],
            self.location[2] + params.base_eye_height_uu,
        ]
    }

    /// `"walking"` or `"falling"`, for the overlay and trace.
    pub fn state(&self) -> &'static str {
        if self.grounded { "walking" } else { "falling" }
    }

    /// Advances one fixed step of `dt` seconds against `world`.
    ///
    /// `sources` maps a collision source id to its object path; it is used only to name the
    /// last blocking contact and is not required for the movement itself.
    pub fn step(
        &mut self,
        dt: f32,
        world: &CollisionWorld,
        params: &PlayerParams,
        input: Input,
        sources: &[String],
    ) {
        debug_assert!(dt > 0.0);
        let s = UNREAL_UNITS_PER_METER;

        // --- desired horizontal velocity in Unreal axes ---------------------------------
        let (sy, cy) = self.yaw.sin_cos();
        let fwd = [cy, sy, 0.0];
        let right = [-sy, cy, 0.0];
        let mut wish = [
            fwd[0] * input.forward + right[0] * input.right,
            fwd[1] * input.forward + right[1] * input.right,
            0.0,
        ];
        let wl = (wish[0] * wish[0] + wish[1] * wish[1]).sqrt();
        if wl < 1e-6 {
            wish = [0.0; 3];
        } else {
            wish[0] /= wl;
            wish[1] /= wl;
        }
        let speed = if input.walk {
            params.ground_speed * params.walking_pct.unwrap_or(0.5)
        } else {
            params.ground_speed
        };
        let desired = [wish[0] * speed, wish[1] * speed, 0.0];

        // --- horizontal acceleration ----------------------------------------------------
        let accel = if self.grounded {
            params.accel_rate
        } else {
            params
                .accel_rate
                .map(|a| a * params.air_control.unwrap_or(0.0))
        };
        let dv = [desired[0] - self.velocity[0], desired[1] - self.velocity[1]];
        let dvl = (dv[0] * dv[0] + dv[1] * dv[1]).sqrt();
        match accel {
            None => {
                self.velocity[0] = desired[0];
                self.velocity[1] = desired[1];
            }
            Some(a) => {
                let max_dv = a * dt;
                if dvl <= max_dv || dvl < 1e-9 {
                    self.velocity[0] = desired[0];
                    self.velocity[1] = desired[1];
                } else {
                    let f = max_dv / dvl;
                    self.velocity[0] += dv[0] * f;
                    self.velocity[1] += dv[1] * f;
                }
            }
        }

        // --- jump -----------------------------------------------------------------------
        if input.jump && self.grounded {
            self.velocity[2] = params.jump_z;
            self.grounded = false;
        }

        // --- gravity while airborne -----------------------------------------------------
        // The vertical step uses the average of the pre/post-gravity velocity (trapezoidal
        // rule), which is exact for constant acceleration: the discrete apex then matches the
        // analytic JumpZ^2/(2g) within one tick instead of undershooting it by ~4% at 60 Hz.
        let vertical_delta_uu = if !self.grounded {
            let before = self.velocity[2];
            self.velocity[2] -= params.gravity_z.abs() * dt;
            if let Some(mf) = params.max_fall_speed
                && self.velocity[2] < -mf
            {
                self.velocity[2] = -mf;
            }
            0.5 * (before + self.velocity[2]) * dt
        } else {
            self.velocity[2] = 0.0;
            0.0
        };

        // --- move -----------------------------------------------------------------------
        let delta_uu = [
            self.velocity[0] * dt,
            self.velocity[1] * dt,
            vertical_delta_uu,
        ];
        let center = to_bevy_position(self.location);
        let half = params.half_extents_bevy();
        let skin = SKIN_UU / s;

        if self.grounded {
            let delta_h = to_bevy_position([delta_uu[0], delta_uu[1], 0.0]);
            let walk = WalkParams {
                skin,
                max_iterations: MAX_ITERATIONS,
                max_step_height: MAXSTEPHEIGHT_UU / s,
                min_floor_z: MINFLOORZ,
            };
            let result = walk_move(world, center, delta_h, half, &walk);
            self.location = bevy_to_unreal_position(result.position);
            self.record_contact(&result.contacts.last().map(|c| c.source), sources);
            if result.falling || !result.on_floor {
                self.grounded = false;
            } else if let Some(hit) = floor_probe(world, result.position, half) {
                self.floor_normal = bevy_to_unreal_direction(hit.normal);
            }
        } else {
            let slide = MoveParams {
                skin,
                max_iterations: MAX_ITERATIONS,
                max_step_height: 0.0,
            };
            let result = move_slide(world, center, to_bevy_position(delta_uu), half, &slide);
            self.location = bevy_to_unreal_position(result.position);
            self.record_contact(&result.contacts.last().map(|c| c.source), sources);
            // Land when a walkable floor is within a small probe below the box.
            let down = [
                result.position[0],
                result.position[1] - LAND_PROBE_M,
                result.position[2],
            ];
            if let Some(hit) = world
                .sweep(result.position, down, half)
                .filter(|h| h.normal[1] >= MINFLOORZ)
            {
                self.grounded = true;
                self.velocity[2] = 0.0;
                self.floor_normal = bevy_to_unreal_direction(hit.normal);
            }
        }
        if self.grounded {
            self.velocity[2] = 0.0;
        }
    }

    fn record_contact(&mut self, source: &Option<u32>, sources: &[String]) {
        self.last_source = source.and_then(|id| sources.get(id as usize).cloned());
    }
}

/// Nearest walkable floor within `MAXSTEPHEIGHT` below `center` (Bevy space).
fn floor_probe(world: &CollisionWorld, center: Vec3, half: Vec3) -> Option<SweepHit> {
    let down = [
        center[0],
        center[1] - MAXSTEPHEIGHT_UU / UNREAL_UNITS_PER_METER,
        center[2],
    ];
    world
        .sweep(center, down, half)
        .filter(|h| h.normal[1] >= MINFLOORZ)
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_collision::Triangle;

    /// Params with instant horizontal acceleration, for the pure speed-mapping test.
    fn instant_params() -> PlayerParams {
        PlayerParams {
            radius_uu: 34.0,
            height_uu: 75.0,
            base_eye_height_uu: 60.0,
            ground_speed: 472.0,
            jump_z: 420.0,
            accel_rate: None,
            air_control: None,
            walking_pct: Some(0.5),
            max_fall_speed: Some(1200.0),
            gravity_z: -950.0,
        }
    }

    /// Params using the decoded `AccelRate`/`AirControl`.
    fn decoded_params() -> PlayerParams {
        PlayerParams {
            accel_rate: Some(2048.0),
            air_control: Some(0.35),
            ..instant_params()
        }
    }

    /// A large flat floor at Bevy Y = 0 (metres), 2 triangles.
    fn flat_floor() -> CollisionWorld {
        let t: Vec<(Triangle, u32)> = vec![
            (
                [[-50.0, 0.0, -50.0], [50.0, 0.0, -50.0], [50.0, 0.0, 50.0]],
                0,
            ),
            (
                [[-50.0, 0.0, -50.0], [50.0, 0.0, 50.0], [-50.0, 0.0, 50.0]],
                0,
            ),
        ];
        CollisionWorld::new(t)
    }

    /// An upper floor for Bevy z > 0 at Y = 0 and a lower floor for z <= 0 at Y = -3 m,
    /// i.e. a 270 UU ledge across the player's forward direction (yaw 0 = Unreal +X = Bevy -Z).
    fn ledge() -> CollisionWorld {
        let t: Vec<(Triangle, u32)> = vec![
            ([[-50.0, 0.0, 0.0], [50.0, 0.0, 0.0], [50.0, 0.0, 50.0]], 0),
            (
                [[-50.0, 0.0, 0.0], [50.0, 0.0, 50.0], [-50.0, 0.0, 50.0]],
                0,
            ),
            (
                [[-50.0, -3.0, -50.0], [50.0, -3.0, -50.0], [50.0, -3.0, 0.0]],
                1,
            ),
            (
                [[-50.0, -3.0, -50.0], [50.0, -3.0, 0.0], [-50.0, -3.0, 0.0]],
                1,
            ),
        ];
        CollisionWorld::new(t)
    }

    /// Box centre resting on a Bevy floor at Y = 0, at Bevy `(x, z)`.
    fn start_on_floor(params: &PlayerParams, bevy_x: f32, bevy_z: f32) -> PlayerSim {
        let height = params.height_uu / UNREAL_UNITS_PER_METER;
        let mut sim = PlayerSim::new(bevy_to_unreal_position([bevy_x, height, bevy_z]), 0.0);
        sim.grounded = true;
        sim
    }

    #[test]
    fn flat_floor_forward_distance_matches_ground_speed() {
        // Instant acceleration (AccelRate absent): after one second of full forward input the
        // horizontal distance must equal GroundSpeed * 1 s in Unreal units.
        let params = instant_params();
        let world = flat_floor();
        let mut sim = start_on_floor(&params, 0.0, 0.0);
        let start = sim.location;
        let dt = 1.0 / 60.0;
        for _ in 0..60 {
            sim.step(
                dt,
                &world,
                &params,
                Input {
                    forward: 1.0,
                    ..Default::default()
                },
                &[],
            );
        }
        let dx = sim.location[0] - start[0];
        assert!(
            (dx - params.ground_speed).abs() < 1.0,
            "distance {dx} UU, expected ~{}",
            params.ground_speed
        );
        assert!(sim.grounded, "must stay on the floor");
        assert!(
            (sim.location[2] - start[2]).abs() < 0.5,
            "must not change height: {}",
            sim.location[2] - start[2]
        );
    }

    #[test]
    fn decoded_accel_rate_reaches_ground_speed_and_matches_ramp_distance() {
        // With the decoded AccelRate 2048, the speed reaches GroundSpeed after
        // GroundSpeed/AccelRate = 0.2305 s and the distance over 1 s is the analytic ramp:
        // v*T - v^2/(2a) = 472 - 472^2/(2*2048) = 417.6 UU.
        let params = decoded_params();
        let world = flat_floor();
        let mut sim = start_on_floor(&params, 0.0, 0.0);
        let start = sim.location;
        let dt = 1.0 / 60.0;
        for _ in 0..60 {
            sim.step(
                dt,
                &world,
                &params,
                Input {
                    forward: 1.0,
                    ..Default::default()
                },
                &[],
            );
        }
        let speed = (sim.velocity[0].powi(2) + sim.velocity[1].powi(2)).sqrt();
        assert!(
            (speed - params.ground_speed).abs() < 1.0,
            "final speed {speed} UU/s, expected {}",
            params.ground_speed
        );
        // Continuous ramp distance v*T - v^2/(2a) = 417.6 UU; the 60 Hz semi-implicit
        // integrator measures 421.5 UU. Assert it lies between the continuous value and full
        // speed (which instant acceleration would give), i.e. acceleration really ramped.
        let dx = sim.location[0] - start[0];
        assert!(
            (417.0..426.0).contains(&dx),
            "distance {dx} UU after 1 s, expected the 417.6 (continuous) .. 421.5 (60 Hz) ramp"
        );
    }

    #[test]
    fn jump_reaches_apex_height() {
        // v^2 / (2g) = 420^2 / (2*950) = 92.84 UU. Discrete integration is within a step.
        let params = instant_params();
        let world = flat_floor();
        let mut sim = start_on_floor(&params, 0.0, 0.0);
        let start_z = sim.location[2];
        let dt = 1.0 / 60.0;
        let mut apex: f32 = 0.0;
        for i in 0..240 {
            // Jump is edge-triggered: request it only on the first tick, then fall.
            let jump = i == 0;
            sim.step(
                dt,
                &world,
                &params,
                Input {
                    jump,
                    ..Default::default()
                },
                &[],
            );
            apex = apex.max(sim.location[2] - start_z);
        }
        // Continuous apex v^2/(2g) = 92.84 UU. The trapezoidal vertical integration reaches
        // it within one 60 Hz tick.
        let expected = params.jump_z * params.jump_z / (2.0 * params.gravity_z.abs());
        assert!(
            (apex - expected).abs() < 1.0,
            "apex {apex} UU, expected {expected} UU"
        );
        assert!(sim.grounded, "must land again");
        assert!(
            (sim.location[2] - start_z).abs() < 0.5,
            "must land back on the floor"
        );
    }

    #[test]
    fn walking_off_a_ledge_falls_and_lands_on_the_lower_floor() {
        let params = decoded_params();
        let world = ledge();
        // Start on the upper floor 1 m before the edge, facing +X (yaw 0).
        let mut sim = start_on_floor(&params, 0.0, 1.0);
        let dt = 1.0 / 60.0;
        let mut saw_falling = false;
        for _ in 0..300 {
            sim.step(
                dt,
                &world,
                &params,
                Input {
                    forward: 1.0,
                    ..Default::default()
                },
                &[],
            );
            if !sim.grounded {
                saw_falling = true;
            }
        }
        assert!(saw_falling, "must fall off the ledge");
        assert!(sim.grounded, "must land on the lower floor");
        // Lower floor at Bevy Y = -3 m => Unreal Z = -270; box centre = floor + 75.
        let expected_center = -3.0 * UNREAL_UNITS_PER_METER + params.height_uu;
        assert!(
            (sim.location[2] - expected_center).abs() < 3.0,
            "centre Z {} UU, expected {expected_center}",
            sim.location[2]
        );
        assert!(sim.location[0] > 0.0, "must have moved past the edge");
    }

    #[test]
    fn no_input_stays_put_on_a_flat_floor() {
        let params = decoded_params();
        let world = flat_floor();
        let mut sim = start_on_floor(&params, 0.0, 0.0);
        let start = sim.location;
        for _ in 0..120 {
            sim.step(1.0 / 60.0, &world, &params, Input::default(), &[]);
        }
        assert!(sim.grounded);
        for (a, b) in sim.location.iter().zip(start) {
            assert!((a - b).abs() < 0.5, "moved without input: {sim:?}");
        }
    }

    #[test]
    fn walk_modifier_scales_speed() {
        let params = instant_params();
        let world = flat_floor();
        let mut walking = start_on_floor(&params, 0.0, 0.0);
        let mut running = start_on_floor(&params, 0.0, 0.0);
        let dt = 1.0 / 60.0;
        for _ in 0..60 {
            walking.step(
                dt,
                &world,
                &params,
                Input {
                    forward: 1.0,
                    walk: true,
                    ..Default::default()
                },
                &[],
            );
            running.step(
                dt,
                &world,
                &params,
                Input {
                    forward: 1.0,
                    ..Default::default()
                },
                &[],
            );
        }
        let w = (walking.location[0].powi(2) + walking.location[1].powi(2)).sqrt();
        let r = (running.location[0].powi(2) + running.location[1].powi(2)).sqrt();
        assert!(
            (w / r - 0.5).abs() < 1e-3,
            "walk speed {w} vs run {r} must be walking_pct 0.5"
        );
    }

    #[test]
    fn huge_fall_velocity_never_tunnels() {
        // Degenerate/hostile input: huge downward velocity must still be stopped by the floor.
        let params = decoded_params();
        let world = flat_floor();
        let mut sim = start_on_floor(&params, 0.0, 0.0);
        sim.grounded = false;
        sim.velocity[2] = -100_000.0;
        for _ in 0..60 {
            sim.step(1.0 / 60.0, &world, &params, Input::default(), &[]);
        }
        assert!(
            sim.location[2] > -params.height_uu - 5.0,
            "tunnelled through the floor: {}",
            sim.location[2]
        );
    }
}
