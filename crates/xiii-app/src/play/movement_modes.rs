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
    fn water_at(&self, center_uu: [f32; 3], _half_uu: [f32; 3]) -> Option<WaterSample> {
        // APawn::SetZone (Engine.dll 0x1038ca50) chooses PhysicsVolume using Encompasses at
        // Location, not IsOverlapping at the pawn's extent. Feet can touch painful water while
        // the centre is still above it; that Touch is handled separately by the VM brush query.
        self.volumes.water_at(center_uu).map(|v| WaterSample {
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
                look_dir: v.look_dir,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_world::movement_volumes::{MovementVolume, VolumeKind};

    #[test]
    fn foot_touch_does_not_switch_to_swimming_before_the_center_enters_water() {
        let water = MovementVolume {
            name: "Water".into(),
            class: "Test.WaterVolume".into(),
            kind: VolumeKind::Water,
            gravity: [0.0, 0.0, -950.0],
            zone_velocity: [0.0; 3],
            fluid_friction: 2.4,
            buoyancy: 0.0,
            terminal_velocity: 2500.0,
            ground_friction: 8.0,
            climb_dir: [0.0, 0.0, 1.0],
            look_dir: [1.0, 0.0, 0.0],
            no_physical_ladder: false,
            min_uu: [-1000.0, -1000.0, -1000.0],
            max_uu: [1000.0, 1000.0, 0.0],
            bounds_are_point: false,
            primitive: None,
        };
        let motion = VolumeMotion::new(MovementVolumes {
            volumes: vec![water],
            ..Default::default()
        });
        let half = [34.0, 34.0, 75.0];
        assert!(
            motion
                .volumes
                .water_box_at([0.0, 0.0, 50.0], half)
                .is_some(),
            "feet touch the volume for VM Touch/pain"
        );
        assert!(
            motion.water_at([0.0, 0.0, 50.0], half).is_none(),
            "retail PhysicsVolume is still dry at the centre"
        );
        assert!(motion.water_at([0.0, 0.0, -1.0], half).is_some());
        assert!(motion.water_at([1100.0, 0.0, -1.0], half).is_none());
    }
}
