//! Adapter from the decoded map movement volumes ([`xiii_world::movement_volumes`]) to the
//! movement simulation's [`MotionQuery`].
//!
//! The map's `WaterVolume`/`LadderVolume` brushes are decoded once at setup (read-only) and this
//! adapter answers the per-tick box queries. A failed import is never silent: the play startup
//! report prints every diagnostic and the overlay/state simply has no volumes.

use xiii_world::movement_volumes::MovementVolumes;

use crate::play::sim::{LadderSample, MotionQuery, WaterSample};

/// Map movement volumes adapted to the simulation's environment query.
#[derive(Debug, Clone, Default)]
pub struct VolumeMotion {
    /// Decoded volumes (empty when the map has none or the import failed).
    pub volumes: MovementVolumes,
}

impl VolumeMotion {
    /// Wraps decoded volumes.
    pub fn new(volumes: MovementVolumes) -> Self {
        Self { volumes }
    }

    /// True when the map contributed any volume.
    pub fn is_empty(&self) -> bool {
        self.volumes.volumes.is_empty()
    }
}

impl MotionQuery for VolumeMotion {
    fn water_at(&self, center_uu: [f32; 3], half_uu: [f32; 3]) -> Option<WaterSample> {
        self.volumes
            .water_box_at(center_uu, half_uu)
            .map(|v| WaterSample {
                gravity_z: v.gravity[2],
                fluid_friction: v.fluid_friction,
                // The simulation uses `max(pawn.Buoyancy, volume.Buoyancy)`.
                buoyancy: v.buoyancy,
                terminal_velocity: v.terminal_velocity,
                zone_velocity: v.zone_velocity,
            })
    }

    fn ladder_at(&self, center_uu: [f32; 3], half_uu: [f32; 3]) -> Option<LadderSample> {
        self.volumes
            .ladder_box_at(center_uu, half_uu)
            .map(|v| LadderSample {
                climb_dir: v.climb_dir,
            })
    }
}
