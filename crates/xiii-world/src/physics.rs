//! Unreal-space physics adapter over the imported collision soup.
//!
//! [`WorldPhysicsAdapter`] implements `xiii_script::physics::WorldPhysics`, whose coordinates
//! are Unreal units and axes (X forward, Y right, **Z up**). `xiii-collision` works in Bevy
//! metres (Y up), so every crossing converts with the single coordinate policy in
//! `xiii_decode::common` — the scale constant and axis mapping are never duplicated here.
//!
//! The adapter is deliberately not wired into the VM (`xiii-tool`) yet; it is unit-tested
//! with a synthetic soup and, opt-in, against the imported Plage01 map.

use xiii_collision::{CollisionWorld, Triangle, Vec3 as BevyVec3};
use xiii_decode::common::{UNREAL_UNITS_PER_METER, to_bevy_position, to_bevy_scale};
use xiii_script::physics::{MoveOutcome, WorldHit, WorldPhysics};

/// Converts a Bevy-space position (metres) back into Unreal space: the inverse of
/// `xiii_decode::common::to_bevy_position`, using the same scale constant.
pub fn bevy_to_unreal_position(b: BevyVec3) -> [f32; 3] {
    let s = UNREAL_UNITS_PER_METER;
    [-b[2] * s, b[0] * s, b[1] * s]
}

/// Converts a Bevy-space direction (unit or not) back into Unreal axes: the inverse of
/// `xiii_decode::common::to_bevy_direction`.
pub fn bevy_to_unreal_direction(b: BevyVec3) -> [f32; 3] {
    [-b[2], b[0], b[1]]
}

/// Converts an Unreal half-extent into the Bevy-space half-extent in metres: the axis
/// permutation of `to_bevy_scale` divided by the same scale constant.
pub fn unreal_extent_to_bevy(e: [f32; 3]) -> BevyVec3 {
    let s = 1.0 / UNREAL_UNITS_PER_METER;
    let p = to_bevy_scale(e);
    [p[0] * s, p[1] * s, p[2] * s]
}

/// A [`WorldPhysics`] provider backed by the imported collision triangles.
///
/// The provider is stateless across queries. It holds two soups: extent (box) queries use the
/// mesh's `UseSimpleBoxCollision` selection, zero-extent (line/ray) queries its
/// `UseSimpleLineCollision` selection (see [`crate::WorldScene`]).
pub struct WorldPhysicsAdapter {
    box_world: CollisionWorld,
    line_world: CollisionWorld,
}

impl WorldPhysicsAdapter {
    /// Builds the adapter from the extent-query and zero-extent-query `(triangle, source id)`
    /// pairs in Bevy space (metres) produced by the importer.
    pub fn from_entries(
        box_entries: impl IntoIterator<Item = (Triangle, u32)>,
        line_entries: impl IntoIterator<Item = (Triangle, u32)>,
    ) -> Self {
        Self {
            box_world: CollisionWorld::new(box_entries),
            line_world: CollisionWorld::new(line_entries),
        }
    }

    /// Builds the adapter from an imported world's query-specific collision soups.
    pub fn from_scene(scene: &crate::WorldScene) -> Self {
        Self::from_entries(scene.box_collision(), scene.line_collision())
    }

    /// The extent-query (box) collision world (for diagnostics and tests).
    pub fn box_world(&self) -> &CollisionWorld {
        &self.box_world
    }

    /// The zero-extent-query (line) collision world (for diagnostics and tests).
    pub fn line_world(&self) -> &CollisionWorld {
        &self.line_world
    }

    /// Shared implementation of [`WorldPhysics::trace`] and [`WorldPhysics::move_box`]: a
    /// swept (or zero-extent ray) query in Unreal space, returned as a [`WorldHit`].
    fn swept_hit(&self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        let s = to_bevy_position(start);
        let e = to_bevy_position(end);
        let half = unreal_extent_to_bevy(extent);
        let zero = half.iter().all(|x| x.abs() < 1e-9);
        let hit = if zero {
            self.line_world.ray(s, e)
        } else {
            self.box_world.sweep(s, e, half)
        }?;
        let t = hit.t;
        let point = [
            s[0] + (e[0] - s[0]) * t,
            s[1] + (e[1] - s[1]) * t,
            s[2] + (e[2] - s[2]) * t,
        ];
        Some(WorldHit {
            location: bevy_to_unreal_position(point),
            normal: bevy_to_unreal_direction(hit.normal),
            time: t,
        })
    }
}

impl WorldPhysics for WorldPhysicsAdapter {
    fn trace(&mut self, start: [f32; 3], end: [f32; 3], extent: [f32; 3]) -> Option<WorldHit> {
        self.swept_hit(start, end, extent)
    }

    fn move_box(&mut self, start: [f32; 3], delta: [f32; 3], extent: [f32; 3]) -> MoveOutcome {
        let end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        match self.swept_hit(start, end, extent) {
            Some(hit) => MoveOutcome {
                end: [
                    start[0] + delta[0] * hit.time,
                    start[1] + delta[1] * hit.time,
                    start[2] + delta[2] * hit.time,
                ],
                hit: Some(hit),
            },
            None => MoveOutcome { end, hit: None },
        }
    }

    fn point_free(&mut self, location: [f32; 3], extent: [f32; 3]) -> bool {
        let center = to_bevy_position(location);
        let half = unreal_extent_to_bevy(extent);
        let zero = half.iter().all(|x| x.abs() < 1e-9);
        let overlaps = if zero {
            self.line_world.overlaps_aabb(center, half)
        } else {
            self.box_world.overlaps_aabb(center, half)
        };
        !overlaps
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{PackageCache, import_map};
    use xiii_decode::common::to_bevy_direction;

    fn close(a: [f32; 3], b: [f32; 3], eps: f32) -> bool {
        a.iter().zip(b).all(|(x, y)| (x - y).abs() < eps)
    }

    #[test]
    fn position_and_direction_round_trip() {
        let s = UNREAL_UNITS_PER_METER;
        let u = [123.0, -45.5, 678.25];
        let b = to_bevy_position(u);
        assert!(close(bevy_to_unreal_position(b), u, 1e-3));
        // Bevy (x,y,z) = (u.y, u.z, -u.x)/s.
        assert!(close(b, [u[1] / s, u[2] / s, -u[0] / s], 1e-6));
        let dir = [0.0, 1.0, 0.0];
        assert!(close(
            bevy_to_unreal_direction(to_bevy_direction(dir)),
            dir,
            1e-6
        ));
        // Extent permutation: (x,y,z)/s -> (y,z,x)/s, magnitudes preserved.
        let e = [2.0, 3.0, 5.0];
        assert!(close(
            unreal_extent_to_bevy(e),
            [e[1] / s, e[2] / s, e[0] / s],
            1e-6
        ));
    }

    /// A 20x20 m floor at Bevy Y = 0 plus a 0.5 m tall wall at Bevy X = -5, in Bevy space.
    fn tiny_soup() -> Vec<(Triangle, u32)> {
        let floor = vec![
            (
                [[-10.0, 0.0, -10.0], [10.0, 0.0, -10.0], [10.0, 0.0, 10.0]],
                0,
            ),
            (
                [[-10.0, 0.0, -10.0], [10.0, 0.0, 10.0], [-10.0, 0.0, 10.0]],
                0,
            ),
        ];
        // Wall at x = -5, spanning z in [-1,1], y in [0,0.5].
        let wall = vec![
            ([[-5.0, 0.0, -1.0], [-5.0, 0.5, -1.0], [-5.0, 0.5, 1.0]], 1),
            ([[-5.0, 0.0, -1.0], [-5.0, 0.5, 1.0], [-5.0, 0.0, 1.0]], 1),
        ];
        floor.into_iter().chain(wall).collect()
    }

    /// Unreal coordinates of a Bevy point (via the adapter's public inverse).
    fn u(p: [f32; 3]) -> [f32; 3] {
        bevy_to_unreal_position(p)
    }

    #[test]
    fn trace_down_hits_floor_at_expected_unreal_height() {
        let mut p = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup());
        // Bevy floor y=0 => Unreal z=0. Start Bevy y=4 m and go to y=-1 m.
        let start = u([0.0, 4.0, 0.0]);
        let end = u([0.0, -1.0, 0.0]);
        let hit = p.trace(start, end, [0.0; 3]).expect("downward trace hits");
        assert!(hit.location[2].abs() < 1e-2, "z={}", hit.location[2]);
        assert!(hit.normal[2] > 0.9, "normal={:?}", hit.normal);
        // 4 m down over a 5 m segment: t = 0.8.
        assert!((hit.time - 0.8).abs() < 1e-3, "{hit:?}");
    }

    #[test]
    fn trace_with_extent_stops_at_wall() {
        let mut p = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup());
        // Move a 0.1 m half-extent box from Bevy x=-4 to x=-6 into the wall at x=-5.
        let start = u([-4.0, 0.25, 0.0]);
        let end = u([-6.0, 0.25, 0.0]);
        let hit = p
            .trace(
                start,
                end,
                [0.1 * xiii_decode::common::UNREAL_UNITS_PER_METER; 3],
            )
            .expect("moving into the wall with an extent must hit");
        assert!(hit.time < 1.0, "{hit:?}");
        // Contact when the box's leading face reaches the wall: center Bevy x = -4.9 => t=0.45.
        assert!((hit.time - 0.45).abs() < 0.02, "t={}", hit.time);
    }

    #[test]
    fn move_box_stops_at_first_block_and_ticks_leave_free_space() {
        let mut p = WorldPhysicsAdapter::from_entries(tiny_soup(), tiny_soup());
        let start = u([-4.0, 0.25, 0.0]);
        let delta = u([-2.0, 0.0, 0.0]);
        let out = p.move_box(start, delta, [10.0, 10.0, 10.0]);
        let hit = out.hit.expect("move_box into the wall must report a hit");
        assert!(hit.time < 1.0);
        let full_end = [
            start[0] + delta[0],
            start[1] + delta[1],
            start[2] + delta[2],
        ];
        assert!(!close(out.end, full_end, 1e-6), "cut short");
        // A move on the open half of the floor is unobstructed.
        let free_start = u([0.0, 4.0, 0.0]);
        let free_delta = u([1.0, 0.0, 0.0]);
        let free = p.move_box(free_start, free_delta, [0.0; 3]);
        assert!(free.hit.is_none(), "{free:?}");
        assert_eq!(
            free.end,
            [
                free_start[0] + free_delta[0],
                free_start[1] + free_delta[1],
                free_start[2] + free_delta[2]
            ]
        );
        // Point-free: in the air above the floor yes, inside the wall no.
        assert!(p.point_free(u([0.0, 4.0, 0.0]), [10.0, 10.0, 10.0]));
        assert!(!p.point_free(u([-5.0, 0.25, 0.0]), [10.0, 10.0, 10.0]));
    }

    #[test]
    fn extent_uses_box_soup_and_zero_extent_uses_line_soup() {
        // Two walls at Bevy x = -5: the box soup has a tall one (y in [0,2]), the line soup a
        // short one (y in [0,0.5]). A query at y = 1.0 distinguishes them.
        let box_soup = vec![
            (
                [[-5.0, 0.0, -1.0], [-5.0, 2.0, -1.0], [-5.0, 2.0, 1.0]],
                0u32,
            ),
            (
                [[-5.0, 0.0, -1.0], [-5.0, 2.0, 1.0], [-5.0, 0.0, 1.0]],
                0u32,
            ),
        ];
        let line_soup = vec![
            (
                [[-5.0, 0.0, -1.0], [-5.0, 0.5, -1.0], [-5.0, 0.5, 1.0]],
                1u32,
            ),
            (
                [[-5.0, 0.0, -1.0], [-5.0, 0.5, 1.0], [-5.0, 0.0, 1.0]],
                1u32,
            ),
        ];
        let mut p = WorldPhysicsAdapter::from_entries(box_soup, line_soup);
        let start = u([-4.0, 1.0, 0.0]);
        let end = u([-6.0, 1.0, 0.0]);
        // Zero extent: the short line wall does not reach y = 1.0, and there is no floor.
        assert!(
            p.trace(start, end, [0.0; 3]).is_none(),
            "a zero-extent query must not see the box soup's tall wall"
        );
        // Extent trace: the tall box wall blocks.
        let hit = p
            .trace(start, end, [0.1 * UNREAL_UNITS_PER_METER; 3])
            .expect("an extent query must see the box soup's tall wall");
        assert!(hit.time < 1.0, "{hit:?}");
        // The line world is the short one; the box world the tall one (diagnostics).
        assert_eq!(p.line_world().triangle_count(), 2);
        assert_eq!(p.box_world().triangle_count(), 2);
    }

    #[test]
    fn opt_in_plage01_player_start_trace_hits_floor() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        // A relative value is resolved against the workspace root (the test CWD is the crate
        // directory), so `XIII_GOG_DIR=XIII_Game cargo test` works from anywhere.
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        let scene = import_map(&mut cache, "Plage01").expect("import Plage01");
        let (ps_bevy, _rot) = scene.player_start.expect("Plage01 has a PlayerStart");
        let mut p = WorldPhysicsAdapter::from_scene(&scene);
        // The PlayerStart is at Unreal Z 1196.0; trace down from 100 UU above the floor.
        let ps = bevy_to_unreal_position(ps_bevy);
        let hit = p
            .trace(ps, [ps[0], ps[1], ps[2] - 200.0], [0.0; 3])
            .expect("a downward trace from the PlayerStart must hit the floor");
        println!(
            "Plage01 PlayerStart {:?} -> floor trace hit at {:?} (normal {:?}, time {:.3})",
            ps, hit.location, hit.normal, hit.time
        );
        // Measured (item1-collision/item1c): floor 12 UU below => Unreal Z = 1184.0.
        assert!(
            (hit.location[2] - 1184.0).abs() < 2.0,
            "floor Z {} (expected ~1184)",
            hit.location[2]
        );
        assert!(hit.normal[2] > 0.9, "floor normal {:?}", hit.normal);
    }
}
