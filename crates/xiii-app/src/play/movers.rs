//! Host-side dynamic collision for UE2 `Mover`s (`PHYS_MovingBrush`).
//!
//! The script VM owns the mover's `Location`/`Rotation` (advanced by
//! [`xiii_script::Vm::advance_interpolation`]); this module turns each mover actor into a
//! [`MovingObject`] in the [`CollisionWorld`] so the player sim and the use-ray collide with the
//! moving brush. The mover's collision triangles are **removed** from the static soup by
//! [`partition`] so they exist in exactly one place.
//!
//! Base pose: [`partition`] uses the mover's `BasePos`/`BaseRot` (the map-serialized actor
//! transform the static triangles were placed with). Each tick [`MoverCollision::update`] writes
//! the VM's current `Location`/`Rotation`, converted with the single coordinate policy and the
//! same `rotator` basis the VM uses for `vector >> rotator`.

use std::collections::HashMap;

use xiii_collision::{CollisionWorld, MovingObject, Triangle};
use xiii_decode::common::to_bevy_position;
use xiii_script::vm::MoverState;
use xiii_world::WorldScene;

// One authoritative rotator -> Bevy-matrix conversion, shared with the VM physics adapter.
pub use xiii_world::physics::rotation_rows;

/// One mover's collision triangles and base pose.
pub struct MoverPart {
    /// Actor display name.
    pub name: String,
    /// Collision source id shared by its triangles.
    pub source: u32,
    /// Base `Location` in Bevy space (metres).
    pub base_center: [f32; 3],
    /// Base rotation rows in Bevy space.
    pub base_rot: [[f32; 3]; 3],
    /// World-space triangles at the base pose.
    pub triangles: Vec<Triangle>,
}

/// Splits the map's box collision soup into static entries and mover parts. A triangle belongs to
/// a mover when its source path's actor part (before `" -> "`) names a live mover actor.
pub fn partition(
    scene: &WorldScene,
    movers: &[MoverState],
) -> (Vec<(Triangle, u32)>, Vec<MoverPart>) {
    let mut parts: Vec<MoverPart> = Vec::new();
    // Index mover name -> part slot, built lazily so only movers with collision triangles exist.
    let mut slot_for: HashMap<String, usize> = HashMap::new();
    let mut static_entries: Vec<(Triangle, u32)> = Vec::new();
    for &i in &scene.collision_box {
        let (tri, source) = scene.collision_triangles[i as usize];
        let actor = scene
            .collision_sources
            .get(source as usize)
            .and_then(|p| p.split_once(" -> "))
            .map_or("", |(a, _)| a);
        if !actor.is_empty()
            && let Some(m) = movers.iter().find(|m| m.name.eq_ignore_ascii_case(actor))
        {
            let slot = *slot_for
                .entry(m.name.to_ascii_lowercase())
                .or_insert_with(|| {
                    parts.push(MoverPart {
                        name: m.name.clone(),
                        source,
                        base_center: to_bevy_position(m.base_pos),
                        base_rot: rotation_rows(m.base_rot),
                        triangles: Vec::new(),
                    });
                    parts.len() - 1
                });
            parts[slot].triangles.push(tri);
            continue;
        }
        static_entries.push((tri, source));
    }
    (static_entries, parts)
}

/// Mover parts installed in a [`CollisionWorld`], with the actor name -> moving-object index
/// mapping needed to update them from the VM each tick.
pub struct MoverCollision {
    actors: Vec<ActorSlot>,
    by_name: HashMap<String, usize>,
}

struct ActorSlot {
    name: String,
    index: usize,
}

impl MoverCollision {
    /// Builds a collision world from `scene`'s box soup with every mover's triangles moved into
    /// the dynamic set, and returns the world plus the updater. Movers with no collision
    /// triangles (or no live actor) are skipped.
    pub fn build(scene: &WorldScene, movers: &[MoverState]) -> (CollisionWorld, MoverCollision) {
        let (static_entries, parts) = partition(scene, movers);
        let mut world = CollisionWorld::new(static_entries);
        let mut actors = Vec::new();
        let mut by_name = HashMap::new();
        for p in parts {
            let object = MovingObject::from_world_triangles(
                p.triangles,
                p.source,
                p.base_center,
                p.base_rot,
            );
            let index = world.add_moving(object);
            by_name.insert(p.name.to_ascii_lowercase(), actors.len());
            actors.push(ActorSlot {
                index,
                name: p.name,
            });
        }
        (world, MoverCollision { actors, by_name })
    }

    /// Number of mover objects installed.
    pub fn count(&self) -> usize {
        self.actors.len()
    }

    /// Names of the installed movers (in installation order).
    pub fn names(&self) -> Vec<&str> {
        self.actors.iter().map(|a| a.name.as_str()).collect()
    }

    /// Writes every live mover's current VM pose into the collision world. Returns the number of
    /// objects updated.
    ///
    /// item40e: an installed mover that is no longer live (destroyed, e.g. a broken
    /// `BreakableMover`) or that no longer blocks the player (`bCollideActors && bBlockPlayers`
    /// cleared by `SetCollision`) is disabled; it is re-enabled when the flags are set again. In
    /// UE2 such an actor is out of the collision hash, so the player walks through it.
    pub fn update(&self, world: &mut CollisionWorld, movers: &[MoverState]) -> usize {
        let mut live: Vec<Option<&MoverState>> = vec![None; self.actors.len()];
        for m in movers {
            if let Some(&slot) = self.by_name.get(&m.name.to_ascii_lowercase()) {
                live[slot] = Some(m);
            }
        }
        let mut updated = 0;
        for (slot, state) in self.actors.iter().zip(live) {
            let enabled = state.is_some_and(|m| m.blocks_players);
            world.set_moving_enabled(slot.index, enabled);
            let Some(m) = state.filter(|_| enabled) else {
                continue;
            };
            let center = to_bevy_position(m.location);
            let rows = rotation_rows(m.rotation);
            if world.set_moving_transform(slot.index, center, rows) {
                updated += 1;
            }
        }
        updated
    }
}

/// The mover the host-owned player pawn stands on, and the part of its motion the player
/// inherits (UE2 `Pawn.Base`: a mover carries the actors attached to it).
///
/// The player's motion is host-owned ([`PlayerSim`]); the VM moves the movers. Per tick:
/// [`Rider::observe`] after the player's own step (which mover is the floor, recorded at the
/// mover's pre-VM-step pose), then [`Rider::carry`] after the VM step (the mover's displacement
/// is added to the player). Decoded for the engine's own attached actors in `Engine.dll`
/// `ULevel::MoveActor` 0x1038a770 (loop 0x1038ae0d..0x1038b084): an attached actor is moved by
/// the base's displacement. Applying that to the host-owned player pawn is this module's
/// adaptation (the pawn's own `MoveActor`/blocking test is not run).
#[derive(Debug, Default)]
pub struct Rider {
    base: Option<RideBase>,
}

#[derive(Debug)]
struct RideBase {
    name: String,
    /// The mover's `Location` the last time the player's pose was reconciled with it.
    location: [f32; 3],
}

/// Result of [`Rider::observe`].
#[derive(Debug, PartialEq, Eq)]
pub enum BaseChange {
    /// The base mover (or lack of one) is the same as before.
    Unchanged,
    /// The player now stands on this mover, or on none (`None`).
    Changed(Option<String>),
}

impl Rider {
    /// Name of the mover the player currently stands on.
    #[cfg(test)]
    pub fn base(&self) -> Option<&str> {
        self.base.as_ref().map(|b| b.name.as_str())
    }

    /// Updates the base from the player's last walking/landing step. `floor_source` is the
    /// collision source of the walkable floor under the box; the actor part of its path
    /// (`"LiftCage0 -> pkg.mesh"`) names the mover. `state_of` returns a live mover's pose.
    pub fn observe(
        &mut self,
        grounded: bool,
        floor_source: Option<u32>,
        sources: &[String],
        state_of: impl Fn(&str) -> Option<MoverState>,
    ) -> BaseChange {
        let now = floor_source
            .filter(|_| grounded)
            .and_then(|id| sources.get(id as usize))
            .and_then(|path| path.split_once(" -> ").map(|(actor, _)| actor))
            .and_then(state_of);
        match (now, self.base.as_ref()) {
            (Some(s), Some(b)) if b.name.eq_ignore_ascii_case(&s.name) => BaseChange::Unchanged,
            (Some(s), _) => {
                self.base = Some(RideBase {
                    name: s.name,
                    location: s.location,
                });
                BaseChange::Changed(self.base.as_ref().map(|b| b.name.clone()))
            }
            (None, Some(_)) => {
                self.base = None;
                BaseChange::Changed(None)
            }
            (None, None) => BaseChange::Unchanged,
        }
    }

    /// Adds the base mover's displacement since the last reconcile to `location` (Unreal units)
    /// and returns it. A base that vanished (destroyed mover) is dropped.
    pub fn carry(
        &mut self,
        location: &mut [f32; 3],
        state_of: impl Fn(&str) -> Option<MoverState>,
    ) -> [f32; 3] {
        let Some(base) = self.base.as_mut() else {
            return [0.0; 3];
        };
        let Some(state) = state_of(&base.name) else {
            self.base = None;
            return [0.0; 3];
        };
        let delta = [
            state.location[0] - base.location[0],
            state.location[1] - base.location[1],
            state.location[2] - base.location[2],
        ];
        base.location = state.location;
        for k in 0..3 {
            location[k] += delta[k];
        }
        delta
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grille_state(blocks_players: bool) -> MoverState {
        MoverState {
            name: "BreakAbleMover16".to_owned(),
            location: [0.0; 3],
            rotation: [0; 3],
            base_pos: [0.0; 3],
            base_rot: [0; 3],
            key_num: 0,
            phys_alpha: 0.0,
            phys_rate: 0.0,
            interpolating: false,
            blocks_players,
        }
    }

    #[test]
    fn destroyed_or_non_blocking_mover_stops_colliding_and_comes_back() {
        // item40e: a grille broken by the chair (destroyed: absent from the live list) must stop
        // blocking the player; SetCollision(false) does the same; restoring the flags restores it.
        let wall: Vec<Triangle> = vec![
            [[0.0, -1.0, -1.0], [0.0, 1.0, -1.0], [0.0, 1.0, 1.0]],
            [[0.0, -1.0, -1.0], [0.0, 1.0, 1.0], [0.0, -1.0, 1.0]],
        ];
        let mut world = CollisionWorld::new(std::iter::empty());
        let index = world.add_moving(MovingObject::from_world_triangles(
            wall,
            3,
            [0.0; 3],
            rotation_rows([0, 0, 0]),
        ));
        let movers = MoverCollision {
            actors: vec![ActorSlot {
                name: "BreakAbleMover16".to_owned(),
                index,
            }],
            by_name: HashMap::from([("breakablemover16".to_owned(), 0)]),
        };
        let (a, b, half) = ([-1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.1; 3]);
        assert_eq!(movers.update(&mut world, &[grille_state(true)]), 1);
        assert!(world.sweep(a, b, half).is_some(), "intact grille blocks");
        assert_eq!(movers.update(&mut world, &[]), 0);
        assert!(
            world.sweep(a, b, half).is_none(),
            "destroyed grille still blocks"
        );
        movers.update(&mut world, &[grille_state(false)]);
        assert!(
            world.sweep(a, b, half).is_none(),
            "non-blocking mover still blocks"
        );
        movers.update(&mut world, &[grille_state(true)]);
        assert!(
            world.sweep(a, b, half).is_some(),
            "re-enabled mover must block again"
        );
    }

    fn cage_at(z: f32) -> MoverState {
        MoverState {
            location: [0.0, 0.0, z],
            name: "LiftCage0".to_owned(),
            ..grille_state(true)
        }
    }

    fn lift_sources() -> Vec<String> {
        vec![
            "Model1 (BSP)".to_owned(),
            "LiftCage0 -> fredprivate.ascenseur".to_owned(),
            "LiftDoor1 -> pkg.door".to_owned(),
        ]
    }

    #[test]
    fn rider_follows_the_cage_displacement_over_many_small_steps() {
        let sources = lift_sources();
        let mut rider = Rider::default();
        let mut cage_z = 0.0f32;
        let mut player = [5.0f32, 6.0, 100.0];
        let state_of =
            |z: f32| move |name: &str| name.eq_ignore_ascii_case("LiftCage0").then(|| cage_at(z));
        // Standing on the cage floor (source 1): the base is recorded at the pre-move pose.
        assert_eq!(
            rider.observe(true, Some(1), &sources, state_of(cage_z)),
            BaseChange::Changed(Some("LiftCage0".to_owned()))
        );
        for _ in 0..600 {
            cage_z += 9.6; // 576 UU/s at 60 Hz
            assert_eq!(
                rider.observe(true, Some(1), &sources, state_of(cage_z - 9.6)),
                BaseChange::Unchanged
            );
            rider.carry(&mut player, state_of(cage_z));
        }
        assert!(
            (player[2] - (100.0 + cage_z)).abs() < 0.05,
            "z {}",
            player[2]
        );
        assert_eq!((player[0], player[1]), (5.0, 6.0));
        assert_eq!(rider.base(), Some("LiftCage0"));
    }

    #[test]
    fn rider_ignores_static_floors_and_airborne_frames() {
        let sources = lift_sources();
        let mut rider = Rider::default();
        let live = |_: &str| Some(cage_at(0.0));
        // A BSP floor ("Model1 (BSP)") has no actor part: never a base.
        assert_eq!(
            rider.observe(true, Some(0), &sources, live),
            BaseChange::Unchanged
        );
        // An unknown or out-of-range source id must not panic or ride.
        assert_eq!(
            rider.observe(true, Some(99), &sources, live),
            BaseChange::Unchanged
        );
        assert_eq!(
            rider.observe(true, None, &sources, live),
            BaseChange::Unchanged
        );
        // A path naming a non-mover actor is not a base.
        let no_mover = |_: &str| None;
        assert_eq!(
            rider.observe(true, Some(1), &sources, no_mover),
            BaseChange::Unchanged
        );
        // Jumping off ends the ride and a moving cage then no longer moves the player.
        assert!(matches!(
            rider.observe(true, Some(1), &sources, live),
            BaseChange::Changed(Some(_))
        ));
        assert_eq!(
            rider.observe(false, Some(1), &sources, live),
            BaseChange::Changed(None)
        );
        let mut p = [0.0, 0.0, 10.0];
        assert_eq!(rider.carry(&mut p, |_| Some(cage_at(500.0))), [0.0; 3]);
        assert_eq!(p, [0.0, 0.0, 10.0]);
    }

    #[test]
    fn rider_switching_cage_for_door_does_not_jump_and_a_destroyed_base_is_dropped() {
        let sources = lift_sources();
        let mut rider = Rider::default();
        let door = |z: f32| MoverState {
            name: "LiftDoor1".to_owned(),
            location: [0.0, 0.0, z],
            ..grille_state(true)
        };
        let world = |cage: f32, door_z: f32| {
            move |name: &str| match name.to_ascii_lowercase().as_str() {
                "liftcage0" => Some(cage_at(cage)),
                "liftdoor1" => Some(door(door_z)),
                _ => None,
            }
        };
        rider.observe(true, Some(1), &sources, world(0.0, 300.0));
        // Step onto the (resting) door sill while the cage is far away: the new base starts at
        // the door's own pose, so no displacement is inherited from the cage.
        assert_eq!(
            rider.observe(true, Some(2), &sources, world(0.0, 300.0)),
            BaseChange::Changed(Some("LiftDoor1".to_owned()))
        );
        let mut p = [0.0, 0.0, 300.0];
        assert_eq!(rider.carry(&mut p, world(500.0, 300.0)), [0.0; 3]);
        assert_eq!(p[2], 300.0);
        // The base mover is destroyed: the ride ends silently, the player is left where it is.
        assert_eq!(rider.carry(&mut p, |_| None), [0.0; 3]);
        assert_eq!(rider.base(), None);
    }

    #[test]
    fn identity_rotation_rows_are_identity() {
        let r = rotation_rows([0, 0, 0]);
        for (i, row) in r.iter().enumerate() {
            for (j, v) in row.iter().enumerate() {
                let expect = if i == j { 1.0 } else { 0.0 };
                assert!((v - expect).abs() < 1e-5, "R[{i}][{j}]={v}");
            }
        }
    }

    #[test]
    fn yaw_rotation_matches_coordinate_policy() {
        // A +90 degree yaw (16384) maps Unreal +X to +Y. In Bevy that is +X -> +Z? Verify by
        // rotating the forward axis: Unreal X (1,0,0) -> Unreal Y (0,1,0) -> Bevy (1,0,0).
        // Actually the Bevy image of Unreal Y is (1,0,0) (to_bevy_direction(0,1,0)), and the
        // Unreal image of X is Y, so R * to_bevy(X) must equal to_bevy(Y).
        let r = rotation_rows([0, 16384, 0]);
        let x_bevy = xiii_decode::common::to_bevy_direction([1.0, 0.0, 0.0]);
        let got = [
            r[0][0] * x_bevy[0] + r[0][1] * x_bevy[1] + r[0][2] * x_bevy[2],
            r[1][0] * x_bevy[0] + r[1][1] * x_bevy[1] + r[1][2] * x_bevy[2],
            r[2][0] * x_bevy[0] + r[2][1] * x_bevy[1] + r[2][2] * x_bevy[2],
        ];
        let want = xiii_decode::common::to_bevy_direction([0.0, 1.0, 0.0]);
        for k in 0..3 {
            assert!((got[k] - want[k]).abs() < 1e-4, "got {got:?} want {want:?}");
        }
    }
}
