//! Distance attenuation matching the XIII HXAudio roll-off model.
//!
//! XIII's audio subsystem exposes `CRolloffParam` with `SaturationDistance`,
//! `StabilisationDistance` and `StabilisationVolume` (measured from `Engine.Actor` class
//! defaults: `SaturationDistance = 400`, `StabilisationDistance = 1392`, `StabilisationVolume =
//! -40.0` dB). `HXAudio.dll` exports `f_fGetSaturationDistance`, `f_fGetStabilisationDistance`,
//! `f_fGetStabilisationVolume` and the strings `GET_DISTANCE_FROM_VOLUME` /
//! `GET_NEW_STABILISATION_DISTANCE` (**reference**, `XIII_Game/system/HXAudio.dll` import table),
//! confirming a distance <-> volume relationship over those two radii.
//!
//! Bevy's spatial audio applies only ear-panning, not distance attenuation (rodio `Spatial`
//! scales channel volumes by `1/dist^2` with no configurable roll-off), so this module computes
//! the gain the host applies. The exact curve shape is a **hypothesis**: full volume within
//! `saturation`, then a linear-in-dB fall from 0 dB to `stabilisation_volume` at
//! `stabilisation_distance`, clamped beyond. The UE2 `AUDIOVOLUME` / `CRolloffParam` fields are
//! real and measured; the interpolation between them is inferred.

/// One emitter's distance roll-off parameters, in Unreal units (UU).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Attenuation {
    /// Distance (UU) within which the sound plays at full volume.
    pub saturation_distance: f32,
    /// Distance (UU) at which the sound reaches `stabilisation_volume`.
    pub stabilisation_distance: f32,
    /// Volume (dB) the sound stays at beyond `stabilisation_distance`.
    pub stabilisation_volume_db: f32,
}

impl Attenuation {
    /// The `Engine.Actor` defaults measured from `engine.u` class defaults.
    pub const ACTOR_DEFAULTS: Self = Self {
        saturation_distance: 400.0,
        stabilisation_distance: 1392.0,
        stabilisation_volume_db: -40.0,
    };

    /// Builds a roll-off from a decoded `PlaySound`/`PlayRolloffSound` `radius` parameter
    /// (`Param3`). **Hypothesis**: `Param3` is the saturation distance and the stabilisation
    /// distance keeps the class-default ratio (1392/400); a non-positive radius means no
    /// attenuation. The raw parameter remains in the event.
    pub fn from_radius(radius: i32) -> Option<Self> {
        if radius <= 0 {
            return None;
        }
        let saturation = radius as f32;
        Some(Self {
            saturation_distance: saturation,
            stabilisation_distance: saturation * (1392.0 / 400.0),
            stabilisation_volume_db: -40.0,
        })
    }

    /// Builds a roll-off from actor properties, in UU. Returns `None` when both distances are
    /// non-positive (no attenuation). The stabilisation distance is clamped to at least the
    /// saturation distance so the curve is always well-defined.
    pub fn from_actor(saturation: f32, stabilisation: f32, volume_db: f32) -> Option<Self> {
        if saturation <= 0.0 && stabilisation <= 0.0 {
            return None;
        }
        let saturation_distance = saturation.max(0.0);
        let stabilisation_distance = stabilisation.max(saturation_distance);
        Some(Self {
            saturation_distance,
            stabilisation_distance,
            stabilisation_volume_db: volume_db.min(0.0),
        })
    }

    /// Linear amplitude at `distance` Unreal units.
    ///
    /// `d <= saturation`: 1.0. `saturation < d < stabilisation`: linear in dB from 0 to
    /// `stabilisation_volume_db`. `d >= stabilisation`: the stabilisation amplitude. A degenerate
    /// pair (equal distances) gives a step at the saturation distance.
    pub fn gain(&self, distance: f32) -> f32 {
        let full = 1.0f32;
        let low = 10f32.powf(self.stabilisation_volume_db / 20.0);
        if distance <= self.saturation_distance {
            return full;
        }
        if distance >= self.stabilisation_distance {
            return low;
        }
        let span = self.stabilisation_distance - self.saturation_distance;
        if span <= 0.0 {
            return low;
        }
        let t = ((distance - self.saturation_distance) / span).clamp(0.0, 1.0);
        // Linear interpolation in dB, then to amplitude.
        let db = self.stabilisation_volume_db * t;
        10f32.powf(db / 20.0)
    }
}

impl Default for Attenuation {
    fn default() -> Self {
        Self::ACTOR_DEFAULTS
    }
}

#[cfg(test)]
mod attenuation_tests {
    use super::*;

    #[test]
    fn full_volume_within_saturation_and_floor_beyond_stabilisation() {
        let a = Attenuation::ACTOR_DEFAULTS;
        assert_eq!(a.gain(0.0), 1.0);
        assert_eq!(a.gain(400.0), 1.0);
        // Beyond the stabilisation distance the amplitude is the stabilisation volume.
        let floor = 10f32.powf(-40.0 / 20.0);
        assert!((a.gain(1392.0) - floor).abs() < 1e-6);
        assert!((a.gain(1_000_000.0) - floor).abs() < 1e-6);
    }

    #[test]
    fn gain_is_monotonic_non_increasing_and_bounded() {
        let a = Attenuation::ACTOR_DEFAULTS;
        let mut prev = a.gain(0.0);
        let mut d = 0.0f32;
        while d <= 3000.0 {
            let g = a.gain(d);
            assert!(g <= prev + 1e-6, "gain increased at {d}: {g} > {prev}");
            assert!((0.0..=1.0).contains(&g));
            prev = g;
            d += 1.0;
        }
    }

    #[test]
    fn gain_depends_only_on_distance_ratios() {
        // A uniform scale of all distances (and the query distance) changes no gain. This checks
        // the invariant that would break if a formula mixed an absolute offset with the scale.
        let a = Attenuation::ACTOR_DEFAULTS;
        let scaled = Attenuation {
            saturation_distance: a.saturation_distance * 90.0,
            stabilisation_distance: a.stabilisation_distance * 90.0,
            stabilisation_volume_db: a.stabilisation_volume_db,
        };
        for d in [
            0.0f32, 100.0, 399.0, 400.0, 401.0, 800.0, 1391.0, 1392.0, 2000.0,
        ] {
            let g1 = a.gain(d);
            let g2 = scaled.gain(d * 90.0);
            assert!(
                (g1 - g2).abs() < 1e-5,
                "scale dependence at d={d}: {g1} vs {g2}"
            );
        }
    }

    #[test]
    fn midpoint_is_half_db_fall() {
        let a = Attenuation::ACTOR_DEFAULTS;
        let mid = (a.saturation_distance + a.stabilisation_distance) / 2.0;
        let expected = 10f32.powf((-20.0f32) / 20.0);
        assert!((a.gain(mid) - expected).abs() < 1e-4);
    }

    #[test]
    fn from_radius_rejects_non_positive() {
        assert!(Attenuation::from_radius(0).is_none());
        assert!(Attenuation::from_radius(-5).is_none());
        let a = Attenuation::from_radius(400).expect("radius 400");
        assert_eq!(a.saturation_distance, 400.0);
        assert!((a.stabilisation_distance - 1392.0).abs() < 1e-3);
    }

    #[test]
    fn from_actor_rejects_both_non_positive_and_clamps() {
        assert!(Attenuation::from_actor(0.0, 0.0, -40.0).is_none());
        // A stabilisation below the saturation is clamped up (well-defined curve).
        let a = Attenuation::from_actor(500.0, 100.0, -20.0).expect("valid");
        assert_eq!(a.stabilisation_distance, 500.0);
        // A positive volume is clamped to 0 dB (no amplification).
        let b = Attenuation::from_actor(100.0, 200.0, 5.0).expect("valid");
        assert_eq!(b.stabilisation_volume_db, 0.0);
    }
}
