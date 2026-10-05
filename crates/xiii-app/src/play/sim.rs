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
    /// Collision cylinder half height while standing (Unreal units, vertical).
    pub height_uu: f32,
    /// `CrouchRadius` (Unreal units, horizontal) while crouched.
    pub crouch_radius_uu: f32,
    /// `CrouchHeight` (Unreal units, treated as a half height like `CollisionHeight`).
    pub crouch_height_uu: f32,
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
    /// `CrouchingPct`: ground-speed multiplier while crouched (e.g. 0.3).
    pub crouching_pct: f32,
    /// `WaterSpeed` (Unreal units/s) while in a water volume.
    pub water_speed: f32,
    /// `LadderSpeed` (Unreal units/s) along a ladder.
    pub ladder_speed: f32,
    /// `Pawn.Buoyancy` (Unreal units/s^2 upward while in water).
    pub buoyancy: f32,
    /// `Pawn.UnderWaterTime` (seconds before drowning); carried for the report.
    pub under_water_time: f32,
    /// `Pawn.bCanCrouch`.
    pub b_can_crouch: bool,
    /// `Pawn.bCanClimbLadders`.
    pub b_can_climb_ladders: bool,
    /// Downward speed clamp while falling (Unreal units/s); `None` => unclamped.
    pub max_fall_speed: Option<f32>,
    /// Gravity (Unreal units/s^2, negative = down).
    pub gravity_z: f32,
}

impl PlayerParams {
    /// Bevy-space half extents `(radius, half height, radius)` in metres for the standing box.
    pub fn half_extents_bevy(&self) -> Vec3 {
        unreal_extent_to_bevy([self.radius_uu, self.radius_uu, self.height_uu])
    }

    /// Bevy-space half extents of the box while `crouched`.
    pub fn half_extents_bevy_for(&self, crouched: bool) -> Vec3 {
        let (r, h) = if crouched {
            (self.crouch_radius_uu, self.crouch_height_uu)
        } else {
            (self.radius_uu, self.height_uu)
        };
        unreal_extent_to_bevy([r, r, h])
    }

    /// Unreal-axis half extents `(radius, radius, half height)` of the box while `crouched`.
    pub fn half_extents_uu(&self, crouched: bool) -> [f32; 3] {
        if crouched {
            [
                self.crouch_radius_uu,
                self.crouch_radius_uu,
                self.crouch_height_uu,
            ]
        } else {
            [self.radius_uu, self.radius_uu, self.height_uu]
        }
    }
}

/// UE2 `EPhysics` values the host writes into `Pawn.Physics` (upstream order: `PHYS_None`=0,
/// `PHYS_Walking`=1, `PHYS_Falling`=2, `PHYS_Swimming`=3, ... `PHYS_Ladder`=10).
pub const PHYS_WALKING: u8 = 1;
/// `PHYS_Falling`.
pub const PHYS_FALLING: u8 = 2;
/// `PHYS_Swimming`.
pub const PHYS_SWIMMING: u8 = 3;
/// `PHYS_Ladder`.
pub const PHYS_LADDER: u8 = 10;

/// Movement properties sampled from the physics/water volume the player is inside.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct WaterSample {
    /// `Gravity.Z` of the volume (Unreal units/s^2, negative down).
    pub gravity_z: f32,
    /// `FluidFriction` of the volume.
    pub fluid_friction: f32,
    /// `Buoyancy` of the volume.
    pub buoyancy: f32,
    /// `TerminalVelocity` of the volume.
    pub terminal_velocity: f32,
    /// `ZoneVelocity` of the volume (Unreal units/s, added to the movement).
    pub zone_velocity: [f32; 3],
}

/// Movement properties sampled from a ladder volume the player overlaps.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LadderSample {
    /// `LadderVolume.ClimbDir` (Unreal axes, unit-ish).
    pub climb_dir: [f32; 3],
}

/// Read-only environment query for the movement modes. The host adapts the decoded map volumes
/// to this trait; synthetic tests implement it directly. The box (`center_uu` + `half_uu` in
/// Unreal axes) is tested against the volume, because a thin water slab is only touched by the
/// player's box, not its centre point.
pub trait MotionQuery {
    /// Water volume overlapping the player box, if any.
    fn water_at(&self, center_uu: [f32; 3], half_uu: [f32; 3]) -> Option<WaterSample>;
    /// Ladder volume overlapping the player box, if any.
    fn ladder_at(&self, center_uu: [f32; 3], half_uu: [f32; 3]) -> Option<LadderSample>;
}

/// The empty environment: no water, no ladders (used by the plain [`PlayerSim::step`]).
#[derive(Debug, Clone, Copy, Default)]
pub struct NoMotion;

impl MotionQuery for NoMotion {
    fn water_at(&self, _center_uu: [f32; 3], _half_uu: [f32; 3]) -> Option<WaterSample> {
        None
    }
    fn ladder_at(&self, _center_uu: [f32; 3], _half_uu: [f32; 3]) -> Option<LadderSample> {
        None
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
    /// Crouch held (XIII `C=Duck`; see the report). Edge state: held, not toggled.
    pub crouch: bool,
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
    /// `bIsCrouched`: the box is currently the smaller crouch box.
    pub crouched: bool,
    /// On a ladder volume (`PHYS_Ladder`).
    pub on_ladder: bool,
    /// Inside a water volume (`PHYS_Swimming`).
    pub in_water: bool,
    /// Current UE2 `EPhysics` value written to the VM pawn.
    pub physics: u8,
    /// True when this step transitioned from airborne to grounded.
    pub landed: bool,
    /// Downward velocity (Unreal units/s, negative) at the moment of landing, before it was
    /// zeroed. The host writes this into the VM before calling the pawn's `Landed`.
    pub land_velocity_z: f32,
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
            crouched: false,
            on_ladder: false,
            in_water: false,
            physics: PHYS_WALKING,
            landed: false,
            land_velocity_z: 0.0,
        }
    }

    /// The box half extents in Bevy metres for the current crouch state.
    pub fn half_extents_bevy(&self, params: &PlayerParams) -> Vec3 {
        params.half_extents_bevy_for(self.crouched)
    }

    /// The box half extents in Unreal axes for the current crouch state.
    pub fn half_extents_uu(&self, params: &PlayerParams) -> [f32; 3] {
        params.half_extents_uu(self.crouched)
    }

    /// Camera eye location in Unreal units (`base_eye_height` above the box centre).
    pub fn eye_location(&self, params: &PlayerParams) -> [f32; 3] {
        [
            self.location[0],
            self.location[1],
            self.location[2] + params.base_eye_height_uu,
        ]
    }

    /// Movement-mode state name (`"walking"`, `"crouched"`, `"falling"`, `"swimming"`,
    /// `"ladder"`), for the overlay and trace.
    pub fn state(&self) -> &'static str {
        if self.on_ladder {
            "ladder"
        } else if self.in_water {
            "swimming"
        } else if !self.grounded {
            "falling"
        } else if self.crouched {
            "crouched"
        } else {
            "walking"
        }
    }

    /// Advances one fixed step of `dt` seconds against `world` with no volumes (walking,
    /// crouching and falling only).
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
        self.step_with_modes(dt, world, params, input, sources, &NoMotion);
    }

    /// Advances one fixed step including ladder/water volume queries from `modes`.
    ///
    /// Mode order (documented hypothesis against UE2's tick): crouch box update, ladder while
    /// touching a ladder volume, swimming while inside a water volume, else walking/falling.
    /// Every mode writes [`PlayerSim::physics`] for the host to publish to `Pawn.Physics`, and a
    /// landing records [`PlayerSim::landed`]/[`PlayerSim::land_velocity_z`] once for the host's
    /// `Landed` call.
    pub fn step_with_modes(
        &mut self,
        dt: f32,
        world: &CollisionWorld,
        params: &PlayerParams,
        input: Input,
        sources: &[String],
        modes: &dyn MotionQuery,
    ) {
        debug_assert!(dt > 0.0);
        self.landed = false;
        self.land_velocity_z = 0.0;

        // --- mode queries against the current box ---------------------------------------
        let half_uu = self.half_extents_uu(params);
        let water = modes.water_at(self.location, half_uu);
        let ladder = modes.ladder_at(self.location, half_uu);

        // --- crouch / uncrouch (grounded on foot only) -----------------------------------
        self.update_crouch(world, params, input, water.is_some(), ladder.is_some());

        // --- ladder ----------------------------------------------------------------------
        if self.on_ladder && params.b_can_climb_ladders {
            match ladder {
                Some(l) => {
                    self.climb_ladder(dt, world, params, input, sources, modes, l);
                    return;
                }
                None => {
                    // Reached the top/bottom of the volume: leave the ladder state.
                    self.on_ladder = false;
                    self.velocity = [0.0; 3];
                    self.grounded = false;
                    self.physics = PHYS_FALLING;
                }
            }
        } else if let Some(l) = ladder
            && params.b_can_climb_ladders
            && water.is_none()
        {
            self.on_ladder = true;
            self.crouched = false;
            self.in_water = false;
            self.climb_ladder(dt, world, params, input, sources, modes, l);
            return;
        }

        // --- water -----------------------------------------------------------------------
        self.in_water = water.is_some();
        if let Some(w) = water {
            self.swim(dt, world, params, input, sources, w);
            return;
        }

        // --- walking / falling -----------------------------------------------------------
        self.walk_or_fall(dt, world, params, input, sources);
    }

    /// Updates the crouch box in place, keeping the box bottom fixed. Crouching is immediate
    /// (the head moves down); standing requires `CrouchHeight` of headroom.
    fn update_crouch(
        &mut self,
        world: &CollisionWorld,
        params: &PlayerParams,
        input: Input,
        in_water: bool,
        on_ladder: bool,
    ) {
        if !params.b_can_crouch || on_ladder || in_water {
            return;
        }
        let delta = (params.height_uu - params.crouch_height_uu).max(0.0);
        if input.crouch && !self.crouched && self.grounded {
            self.location[2] -= delta;
            self.crouched = true;
        } else if !input.crouch && self.crouched {
            // Only the headroom slab above the crouch box grows when standing (the feet stay on
            // the floor), so test that slab: it avoids a false block by the floor the box rests
            // on. The slab is `2*delta` tall, from the crouch top to the standing top.
            let slab_center = [
                self.location[0],
                self.location[1],
                self.location[2] + params.crouch_height_uu + delta,
            ];
            let slab_half =
                unreal_extent_to_bevy([params.crouch_radius_uu, params.crouch_radius_uu, delta]);
            if world
                .overlap_aabb(to_bevy_position(slab_center), slab_half)
                .is_empty()
            {
                self.location[2] += delta;
                self.crouched = false;
            }
        }
    }

    /// Climbs along `ladder.climb_dir` at `LadderSpeed` (forward = up, back = down), then exits
    /// the ladder state when the box leaves every ladder volume.
    #[allow(clippy::too_many_arguments)]
    fn climb_ladder(
        &mut self,
        dt: f32,
        world: &CollisionWorld,
        params: &PlayerParams,
        input: Input,
        sources: &[String],
        modes: &dyn MotionQuery,
        ladder: LadderSample,
    ) {
        self.physics = PHYS_LADDER;
        self.grounded = false;
        self.crouched = false;
        let mut dir = ladder.climb_dir;
        let dl = (dir[0] * dir[0] + dir[1] * dir[1] + dir[2] * dir[2]).sqrt();
        if dl < 1e-6 {
            dir = [0.0, 0.0, 1.0];
        } else {
            dir = [dir[0] / dl, dir[1] / dl, dir[2] / dl];
        }
        // Jump leaves the ladder (UE2 lets the player push off).
        if input.jump {
            self.on_ladder = false;
            self.velocity = [0.0, 0.0, params.jump_z];
            self.physics = PHYS_FALLING;
            return;
        }
        let v = input.forward * params.ladder_speed;
        self.velocity = [dir[0] * v, dir[1] * v, dir[2] * v];
        let delta = [dir[0] * v * dt, dir[1] * v * dt, dir[2] * v * dt];
        self.move_no_gravity(world, params, delta, sources);
        let half_uu = self.half_extents_uu(params);
        if modes.ladder_at(self.location, half_uu).is_none() {
            self.on_ladder = false;
            self.velocity = [0.0; 3];
            self.physics = PHYS_FALLING;
        }
    }

    /// Swims in a water volume: `WaterSpeed` horizontally, `Buoyancy`/volume gravity vertically,
    /// fluid-friction damping, and a `Jump` stroke upward.
    fn swim(
        &mut self,
        dt: f32,
        world: &CollisionWorld,
        params: &PlayerParams,
        input: Input,
        sources: &[String],
        w: WaterSample,
    ) {
        self.physics = PHYS_SWIMMING;
        self.grounded = false;
        self.crouched = false;
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
        // Horizontal: accelerate toward `WaterSpeed` with the decoded AccelRate when present.
        let desired = [wish[0] * params.water_speed, wish[1] * params.water_speed];
        match params.accel_rate {
            None => {
                self.velocity[0] = desired[0];
                self.velocity[1] = desired[1];
            }
            Some(a) => {
                let dv = [desired[0] - self.velocity[0], desired[1] - self.velocity[1]];
                let dvl = (dv[0] * dv[0] + dv[1] * dv[1]).sqrt();
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
        // Vertical: volume gravity opposes the pawn's buoyancy; `Jump` is a swim stroke.
        let gravity = w.gravity_z.abs();
        let buoyancy = params.buoyancy.max(w.buoyancy);
        if input.jump {
            self.velocity[2] = params.jump_z;
        } else {
            self.velocity[2] += (buoyancy - gravity) * dt;
        }
        let damp = (1.0 - w.fluid_friction * dt).clamp(0.0, 1.0);
        for k in 0..3 {
            self.velocity[k] *= damp;
        }
        let terminal = if w.terminal_velocity > 0.0 {
            w.terminal_velocity
        } else {
            params.max_fall_speed.unwrap_or(f32::INFINITY)
        };
        self.velocity[2] = self.velocity[2].clamp(-terminal, terminal);
        // `ZoneVelocity` is a constant drift the volume imparts on everything inside it.
        let delta = [
            (self.velocity[0] + w.zone_velocity[0]) * dt,
            (self.velocity[1] + w.zone_velocity[1]) * dt,
            (self.velocity[2] + w.zone_velocity[2]) * dt,
        ];
        self.move_no_gravity(world, params, delta, sources);
    }

    /// Moves the box without gravity and lands it on a walkable floor (used by ladder/swim).
    fn move_no_gravity(
        &mut self,
        world: &CollisionWorld,
        params: &PlayerParams,
        delta_uu: [f32; 3],
        sources: &[String],
    ) {
        let center = to_bevy_position(self.location);
        let half = self.half_extents_bevy(params);
        let slide = MoveParams {
            skin: SKIN_UU / UNREAL_UNITS_PER_METER,
            max_iterations: MAX_ITERATIONS,
            max_step_height: 0.0,
        };
        let result = move_slide(world, center, to_bevy_position(delta_uu), half, &slide);
        self.location = bevy_to_unreal_position(result.position);
        self.record_contact(&result.contacts.last().map(|c| c.source), sources);
        let down = [
            result.position[0],
            result.position[1] - LAND_PROBE_M,
            result.position[2],
        ];
        if let Some(hit) = world
            .sweep(result.position, down, half)
            .filter(|h| h.normal[1] >= MINFLOORZ)
            && !self.on_ladder
        {
            self.grounded = true;
            self.floor_normal = bevy_to_unreal_direction(hit.normal);
        }
    }

    /// The original walking/falling path, using the current (possibly crouched) box.
    fn walk_or_fall(
        &mut self,
        dt: f32,
        world: &CollisionWorld,
        params: &PlayerParams,
        input: Input,
        sources: &[String],
    ) {
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
        let speed = if self.crouched {
            params.ground_speed * params.crouching_pct
        } else if input.walk {
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
        let half = self.half_extents_bevy(params);
        let skin = SKIN_UU / s;

        if self.grounded {
            self.physics = PHYS_WALKING;
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
                self.physics = PHYS_FALLING;
            } else if let Some(hit) = floor_probe(world, result.position, half) {
                self.floor_normal = bevy_to_unreal_direction(hit.normal);
            }
        } else {
            self.physics = PHYS_FALLING;
            let slide = MoveParams {
                skin,
                max_iterations: MAX_ITERATIONS,
                max_step_height: 0.0,
            };
            let impact_z = self.velocity[2];
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
                self.physics = PHYS_WALKING;
                self.landed = true;
                self.land_velocity_z = impact_z;
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

    // ---- movement modes -----------------------------------------------------------------

    /// The flat floor plus a ceiling plane at Bevy Y = `ceiling_y` (metres), 2 triangles.
    fn floor_and_ceiling(ceiling_y: f32) -> CollisionWorld {
        let mut t: Vec<(Triangle, u32)> = vec![
            (
                [[-50.0, 0.0, -50.0], [50.0, 0.0, -50.0], [50.0, 0.0, 50.0]],
                0,
            ),
            (
                [[-50.0, 0.0, -50.0], [50.0, 0.0, 50.0], [-50.0, 0.0, 50.0]],
                0,
            ),
        ];
        t.push((
            [
                [-50.0, ceiling_y, -50.0],
                [50.0, ceiling_y, 50.0],
                [50.0, ceiling_y, -50.0],
            ],
            1,
        ));
        t.push((
            [
                [-50.0, ceiling_y, -50.0],
                [-50.0, ceiling_y, 50.0],
                [50.0, ceiling_y, 50.0],
            ],
            1,
        ));
        CollisionWorld::new(t)
    }

    /// A player already crouched, feet on a Bevy Y = 0 floor.
    fn start_crouched(params: &PlayerParams, bevy_x: f32, bevy_z: f32) -> PlayerSim {
        let h = params.crouch_height_uu / UNREAL_UNITS_PER_METER;
        let mut sim = PlayerSim::new(bevy_to_unreal_position([bevy_x, h, bevy_z]), 0.0);
        sim.grounded = true;
        sim.crouched = true;
        sim
    }

    fn aabb_overlap(min: [f32; 3], max: [f32; 3], c: [f32; 3], h: [f32; 3]) -> bool {
        (0..3).all(|k| c[k] + h[k] >= min[k] && c[k] - h[k] <= max[k])
    }

    /// Synthetic motion environment: optional axis-aligned water/ladder volumes (Unreal units).
    #[derive(Default)]
    struct TestEnv {
        water: Option<([f32; 3], [f32; 3])>,
        ladder: Option<([f32; 3], [f32; 3])>,
    }

    impl MotionQuery for TestEnv {
        fn water_at(&self, c: [f32; 3], h: [f32; 3]) -> Option<WaterSample> {
            self.water
                .filter(|(mn, mx)| aabb_overlap(*mn, *mx, c, h))
                .map(|_| WaterSample {
                    gravity_z: -950.0,
                    fluid_friction: 0.5,
                    buoyancy: 0.0,
                    terminal_velocity: 2500.0,
                    zone_velocity: [0.0; 3],
                })
        }
        fn ladder_at(&self, c: [f32; 3], h: [f32; 3]) -> Option<LadderSample> {
            self.ladder
                .filter(|(mn, mx)| aabb_overlap(*mn, *mx, c, h))
                .map(|_| LadderSample {
                    climb_dir: [0.0, 0.0, 1.0],
                })
        }
    }

    #[test]
    fn crouch_shrinks_the_box_and_slows_movement() {
        let params = instant_params();
        let world = flat_floor();
        let mut crouched = start_on_floor(&params, 0.0, 0.0);
        let mut running = start_on_floor(&params, 0.0, 0.0);
        let dt = 1.0 / 60.0;
        // Settle the crouch (the first step moves the box centre down).
        crouched.step(
            dt,
            &world,
            &params,
            Input {
                crouch: true,
                ..Default::default()
            },
            &[],
        );
        assert!(crouched.crouched, "crouch input must set bIsCrouched");
        assert_eq!(crouched.state(), "crouched");
        assert!(
            params.crouch_height_uu < params.height_uu,
            "the crouch box must be shorter than the standing box"
        );
        for _ in 0..119 {
            crouched.step(
                dt,
                &world,
                &params,
                Input {
                    forward: 1.0,
                    crouch: true,
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
        // Crouch height is below the standing half height and feet stay on the floor.
        assert!(
            (crouched.location[2] - params.crouch_height_uu).abs() < 0.5,
            "crouched centre {} must be CrouchHeight {}",
            crouched.location[2],
            params.crouch_height_uu
        );
        let speed = (crouched.velocity[0].powi(2) + crouched.velocity[1].powi(2)).sqrt();
        assert!(
            (speed - params.ground_speed * params.crouching_pct).abs() < 1.0,
            "crouched speed {speed} != GroundSpeed*CrouchingPct {}",
            params.ground_speed * params.crouching_pct
        );
        let cd = (crouched.location[0].powi(2) + crouched.location[1].powi(2)).sqrt();
        let rd = (running.location[0].powi(2) + running.location[1].powi(2)).sqrt();
        assert!(
            (cd / rd - params.crouching_pct).abs() < 0.02,
            "crouch distance ratio {} != CrouchingPct {}",
            cd / rd,
            params.crouching_pct
        );
    }

    #[test]
    fn standing_is_blocked_by_a_low_ceiling_and_kept_crouched() {
        let params = instant_params();
        // A ceiling at 1.2 m: the 1.5 m standing box does not fit, the crouch box (0.96+0.53
        // top ~1.07 m) does.
        let world = floor_and_ceiling(1.2);
        let mut sim = start_crouched(&params, 0.0, 0.0);
        for _ in 0..30 {
            sim.step(1.0 / 60.0, &world, &params, Input::default(), &[]);
        }
        assert!(
            sim.crouched,
            "player must stay crouched under a 1.2 m ceiling (centre {})",
            sim.location[2]
        );
        // With the ceiling high enough, the same input stands up.
        let high = floor_and_ceiling(3.0);
        let mut sim = start_crouched(&params, 0.0, 0.0);
        for _ in 0..30 {
            sim.step(1.0 / 60.0, &high, &params, Input::default(), &[]);
        }
        assert!(!sim.crouched, "player must stand with a 3 m ceiling");
        assert!(
            (sim.location[2] - params.height_uu).abs() < 0.5,
            "standing centre {} must be CollisionHeight {}",
            sim.location[2],
            params.height_uu
        );
    }

    #[test]
    fn ladder_enter_climb_and_exit() {
        let params = instant_params();
        let world = flat_floor();
        let env = TestEnv {
            ladder: Some(([-200.0, -200.0, 0.0], [200.0, 200.0, 500.0])),
            ..Default::default()
        };
        let mut sim = start_on_floor(&params, 0.0, 0.0);
        let start_z = sim.location[2];
        let dt = 1.0 / 60.0;
        let mut entered = false;
        let mut saw_ladder_physics = false;
        let mut reached_top = false;
        let mut max_z = start_z;
        for _ in 0..240 {
            sim.step_with_modes(
                dt,
                &world,
                &params,
                Input {
                    forward: 1.0,
                    ..Default::default()
                },
                &[],
                &env,
            );
            max_z = max_z.max(sim.location[2]);
            if sim.on_ladder {
                entered = true;
                saw_ladder_physics |= sim.physics == PHYS_LADDER;
            } else if entered {
                // Left the volume after climbing: the top was reached.
                reached_top = true;
            }
        }
        assert!(entered, "player must enter the ladder volume");
        assert!(
            saw_ladder_physics,
            "PHYS_Ladder must be published while on the ladder"
        );
        assert!(
            max_z > start_z + 150.0,
            "ladder climb raised only {} UU",
            max_z - start_z
        );
        // Once past the top, the box leaves the volume and the ladder state clears.
        assert!(reached_top, "player never left the ladder volume");
    }

    #[test]
    fn swimming_uses_water_speed_and_stays_above_the_floor() {
        let params = instant_params();
        let world = flat_floor();
        let env = TestEnv {
            water: Some(([-500.0, -500.0, 0.0], [500.0, 500.0, 200.0])),
            ..Default::default()
        };
        let mut sim = start_on_floor(&params, 0.0, 0.0);
        let start = sim.location;
        let dt = 1.0 / 60.0;
        for _ in 0..60 {
            sim.step_with_modes(
                dt,
                &world,
                &params,
                Input {
                    forward: 1.0,
                    ..Default::default()
                },
                &[],
                &env,
            );
        }
        assert!(sim.in_water, "player must be in the water volume");
        assert_eq!(sim.physics, PHYS_SWIMMING);
        assert_eq!(sim.state(), "swimming");
        let dx = sim.location[0] - start[0];
        assert!(
            (dx - params.water_speed).abs() < 5.0,
            "swim distance {dx} UU, expected ~WaterSpeed {}",
            params.water_speed
        );
        assert!(
            sim.location[2] > 0.0,
            "swimmer sank through the floor to {}",
            sim.location[2]
        );
    }

    #[test]
    fn landing_records_the_impact_velocity() {
        let params = instant_params();
        let world = flat_floor();
        let height = 2000.0 / UNREAL_UNITS_PER_METER;
        let mut sim = PlayerSim::new(bevy_to_unreal_position([0.0, height, 0.0]), 0.0);
        sim.grounded = false;
        let mut landed_velocity = None;
        for _ in 0..300 {
            sim.step(1.0 / 60.0, &world, &params, Input::default(), &[]);
            if sim.landed {
                landed_velocity = Some(sim.land_velocity_z);
                break;
            }
        }
        let vz = landed_velocity.expect("player never landed");
        assert!(
            vz < 0.0 && vz >= -params.max_fall_speed.unwrap_or(f32::INFINITY) - 1.0,
            "landing velocity {vz} is not the downward impact speed"
        );
        assert!(sim.grounded, "must be grounded after landing");
        assert!(
            (sim.location[2] - params.height_uu).abs() < 2.0,
            "landed centre {} != CollisionHeight {}",
            sim.location[2],
            params.height_uu
        );
    }
}
