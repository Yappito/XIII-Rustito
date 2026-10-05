//! Behaviour tests on synthetic geometry only (no game data).

use super::*;
use crate::sweep::{start_drives_into, sweep_triangle};

/// Two triangles forming a large axis-aligned plane quad.
fn quad(a: Vec3, b: Vec3, c: Vec3, d: Vec3) -> Vec<Triangle> {
    vec![[a, b, c], [a, c, d]]
}

/// Two triangles forming the x = `x` plane (y,z in [-5,5]).
fn wall_x(x: f32) -> Vec<Triangle> {
    quad(
        [x, -5.0, -5.0],
        [x, 5.0, -5.0],
        [x, 5.0, 5.0],
        [x, -5.0, 5.0],
    )
}

/// Two triangles forming the y = `y` plane (x,z in [-5,5]).
fn floor_y(y: f32) -> Vec<Triangle> {
    quad(
        [-5.0, y, -5.0],
        [5.0, y, -5.0],
        [5.0, y, 5.0],
        [-5.0, y, 5.0],
    )
}

/// A closed axis-aligned box as 12 triangles with the given source id.
fn box_tris(min: Vec3, max: Vec3, source: u32) -> Vec<(Triangle, u32)> {
    let p = |x: usize, y: usize, z: usize| {
        [
            if x == 0 { min[0] } else { max[0] },
            if y == 0 { min[1] } else { max[1] },
            if z == 0 { min[2] } else { max[2] },
        ]
    };
    let c = [
        p(0, 0, 0),
        p(1, 0, 0),
        p(1, 1, 0),
        p(0, 1, 0),
        p(0, 0, 1),
        p(1, 0, 1),
        p(1, 1, 1),
        p(0, 1, 1),
    ];
    let faces = [
        [0, 3, 2, 1],
        [4, 5, 6, 7],
        [0, 1, 5, 4],
        [2, 3, 7, 6],
        [1, 2, 6, 5],
        [0, 4, 7, 3],
    ];
    let mut out = Vec::new();
    for f in faces {
        out.push(([c[f[0]], c[f[1]], c[f[2]]], source));
        out.push(([c[f[0]], c[f[2]], c[f[3]]], source));
    }
    out
}

fn world(entries: impl IntoIterator<Item = (Triangle, u32)>) -> CollisionWorld {
    CollisionWorld::new(entries)
}

#[test]
fn sweep_into_wall_stops_at_expected_t() {
    let w = world(wall_x(0.0).into_iter().map(|t| (t, 7)));
    // Box half-x 0.3 travels from x=-1 to x=1; face reaches the wall at travel 0.7/2.
    let hit = w.sweep([-1.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.3, 0.5, 0.3]);
    let hit = hit.expect("must hit the wall");
    assert!((hit.t - 0.35).abs() < 1e-4, "t={}", hit.t);
    assert_eq!(hit.source, 7);
    // Normal points from the wall (x=0) toward the box (x<0): -X.
    assert!(hit.normal[0] < -0.9, "normal={:?}", hit.normal);
    assert!(!hit.start_penetrating);
}

#[test]
fn sweep_misses_wall_it_does_not_reach() {
    let w = world(wall_x(0.0).into_iter().map(|t| (t, 0)));
    // Travel only to x=-0.5: face at -0.2, short of the wall.
    assert!(
        w.sweep([-1.0, 0.0, 0.0], [-0.5, 0.0, 0.0], [0.3, 0.5, 0.3])
            .is_none()
    );
}

#[test]
fn slides_along_wall_keeping_tangential_motion() {
    let w = world(wall_x(0.0).into_iter().map(|t| (t, 1)));
    let params = MoveParams {
        max_iterations: 4,
        ..MoveParams::default()
    };
    // Push diagonally into the wall (+x) and along it (+z).
    let r = move_slide(
        &w,
        [-0.5, 0.0, 0.0],
        [1.5, 0.0, 1.0],
        [0.3, 0.5, 0.3],
        &params,
    );
    assert!(r.blocked);
    // Box face is stopped near the wall plane, not past it.
    assert!(r.position[0] <= -0.29, "x={}", r.position[0]);
    // Sliding preserved most of the +z travel.
    assert!(r.position[2] > 0.8, "z={}", r.position[2]);
}

#[test]
fn corridor_one_percent_wider_passes() {
    let gap = 0.3 * 1.01;
    let mut tris = Vec::new();
    tris.extend(box_tris([0.0, -1.0, gap], [4.0, 1.0, gap + 0.1], 1));
    tris.extend(box_tris([0.0, -1.0, -gap - 0.1], [4.0, 1.0, -gap], 2));
    let w = world(tris);
    let hit = w.sweep([-1.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.3, 0.5, 0.3]);
    assert!(hit.is_none(), "1% wider corridor must pass, got {hit:?}");
}

#[test]
fn corridor_one_percent_narrower_blocks() {
    let gap = 0.3 * 0.99;
    let mut tris = Vec::new();
    tris.extend(box_tris([0.0, -1.0, gap], [4.0, 1.0, gap + 0.1], 1));
    tris.extend(box_tris([0.0, -1.0, -gap - 0.1], [4.0, 1.0, -gap], 2));
    let w = world(tris);
    let hit = w.sweep([-1.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.3, 0.5, 0.3]);
    let hit = hit.expect("1% narrower corridor must block");
    assert!((hit.t - 0.25).abs() < 0.05, "t={}", hit.t);
    assert!(hit.source == 1 || hit.source == 2);
}

#[test]
fn step_up_below_max_height_succeeds() {
    let h = 0.2;
    let mut tris: Vec<(Triangle, u32)> = floor_y(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(box_tris([0.0, 0.0, -1.0], [1.0, h, 1.0], 2));
    let w = world(tris);
    let params = MoveParams {
        max_step_height: 0.25,
        max_iterations: 4,
        ..MoveParams::default()
    };
    let r = move_slide(
        &w,
        [-0.5, 0.5, 0.0],
        [0.9, 0.0, 0.0],
        [0.3, 0.5, 0.3],
        &params,
    );
    // Landed on top of the step (bottom at y = h).
    assert!(
        (r.position[1] - (h + 0.5)).abs() < 0.02,
        "y={} expected {}",
        r.position[1],
        h + 0.5
    );
    assert!(
        r.position[0] > 0.0,
        "advanced onto the step: x={}",
        r.position[0]
    );
}

#[test]
fn step_up_above_max_height_is_blocked() {
    let h = 0.3;
    let mut tris: Vec<(Triangle, u32)> = floor_y(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(box_tris([0.0, 0.0, -1.0], [1.0, h, 1.0], 2));
    let w = world(tris);
    let params = MoveParams {
        max_step_height: 0.25,
        max_iterations: 4,
        ..MoveParams::default()
    };
    let r = move_slide(
        &w,
        [-0.5, 0.5, 0.0],
        [2.0, 0.0, 0.0],
        [0.3, 0.5, 0.3],
        &params,
    );
    assert!(r.blocked);
    assert!(r.position[0] < 0.0, "did not climb: x={}", r.position[0]);
    assert!(
        (r.position[1] - 0.5).abs() < 1e-3,
        "stayed on the floor: y={}",
        r.position[1]
    );
}

#[test]
fn penetrating_wall_moving_into_it_blocks_at_zero() {
    let w = world(wall_x(0.0).into_iter().map(|t| (t, 4)));
    // Box half-x 0.3 penetrates the x=0 wall by 1e-4 m, then moves +x (into the wall).
    let start = [-0.3 + 1e-4, 0.0, 0.0];
    let hit = w
        .sweep(start, [start[0] + 1.0, 0.0, 0.0], [0.3, 0.5, 0.3])
        .expect("motion into a surface already flush/inside must be blocked");
    assert!(hit.start_penetrating, "{hit:?}");
    assert_eq!(hit.t, 0.0);
    assert!(
        hit.normal[0] < -0.9,
        "normal faces the box: {:?}",
        hit.normal
    );
}

#[test]
fn penetrating_wall_moving_away_or_parallel_is_not_blocked() {
    let w = world(wall_x(0.0).into_iter().map(|t| (t, 4)));
    let start = [-0.3 + 1e-4, 0.0, 0.0];
    // Moving away from the wall (-x).
    assert!(
        w.sweep(start, [start[0] - 1.0, 0.0, 0.0], [0.3, 0.5, 0.3])
            .is_none(),
        "moving away must not be blocked"
    );
    // Moving parallel to the wall (+z).
    assert!(
        w.sweep(start, [start[0], 0.0, 2.0], [0.3, 0.5, 0.3])
            .is_none(),
        "moving parallel must not be blocked"
    );
}

#[test]
fn resting_on_floor_moving_down_blocks_at_zero() {
    let w = world(floor_y(0.0).into_iter().map(|t| (t, 1)));
    // Box touches the floor exactly (center y = 0.5, half height 0.5); moving down drives in.
    let hit = w
        .sweep([0.0, 0.5, 0.0], [0.0, -1.0, 0.0], [0.3, 0.5, 0.3])
        .expect("driving down into the floor must be blocked");
    assert!(hit.start_penetrating && hit.t == 0.0, "{hit:?}");
    assert!(
        hit.normal[1] > 0.9,
        "floor normal points up: {:?}",
        hit.normal
    );
}

#[test]
fn shallow_slide_into_wall_never_tunnels_after_many_steps() {
    // A large wall so the box cannot slide off its edge during the test.
    let w = world(
        quad(
            [0.0, -50.0, -50.0],
            [0.0, 50.0, -50.0],
            [0.0, 50.0, 50.0],
            [0.0, -50.0, 50.0],
        )
        .into_iter()
        .map(|t| (t, 3)),
    );
    let params = MoveParams {
        max_iterations: 4,
        ..MoveParams::default()
    };
    // Diagonal push: mostly +z (along the wall) with a small +x component into it.
    let mut pos = [-0.5, 0.0, 0.0];
    let delta = [0.01, 0.0, 0.05];
    for _ in 0..1000 {
        let r = move_slide(&w, pos, delta, [0.3, 0.5, 0.3], &params);
        pos = r.position;
        assert!(
            pos[0] <= -0.29,
            "box crossed to the far side of the wall: x={}",
            pos[0]
        );
    }
    // It slid a long way along the wall.
    assert!(
        pos[2] > 10.0,
        "expected to slide along the wall, z={}",
        pos[2]
    );
}

#[test]
fn resting_on_floor_does_not_block_horizontal_motion() {
    let w = world(floor_y(0.0).into_iter().map(|t| (t, 1)));
    // Box touches the floor exactly (center y = 0.5, half height 0.5).
    let r = move_slide(
        &w,
        [0.0, 0.5, 0.0],
        [1.0, 0.0, 0.0],
        [0.3, 0.5, 0.3],
        &MoveParams::default(),
    );
    assert!(
        !r.blocked,
        "floor blocked horizontal motion: {:?}",
        r.contacts
    );
    assert!((r.position[0] - 1.0).abs() < 1e-4, "x={}", r.position[0]);
}

#[test]
fn start_overlap_is_reported_not_panicking() {
    let w = world(wall_x(0.0).into_iter().map(|t| (t, 3)));
    // Box already spans the wall plane.
    let params = SweepParams {
        skip_start_penetration: false,
    };
    let hit = sweep_aabb(
        &w,
        [0.0, 0.0, 0.0],
        [1.0, 0.0, 0.0],
        [0.3, 0.5, 0.3],
        &params,
    )
    .expect("overlap reported");
    assert!(hit.start_penetrating);
    assert_eq!(hit.t, 0.0);
    // Default parameters skip start penetration for resting movement.
    assert!(
        sweep_aabb(
            &w,
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.3, 0.5, 0.3],
            &SweepParams::default()
        )
        .is_none()
    );
}

#[test]
fn zero_length_sweep_is_handled() {
    let w = world(wall_x(0.0).into_iter().map(|t| (t, 3)));
    assert!(
        w.sweep([-1.0, 0.0, 0.0], [-1.0, 0.0, 0.0], [0.3, 0.5, 0.3])
            .is_none()
    );
    let params = SweepParams {
        skip_start_penetration: false,
    };
    let hit = sweep_aabb(
        &w,
        [0.0, 0.0, 0.0],
        [0.0, 0.0, 0.0],
        [0.3, 0.5, 0.3],
        &params,
    );
    assert!(hit.is_some_and(|h| h.start_penetrating && h.t == 0.0));
}

#[test]
fn ray_matches_wall_distance_and_overlap_finds_it() {
    let w = world(wall_x(0.0).into_iter().map(|t| (t, 5)));
    let hit = w.ray([-2.0, 0.0, 0.0], [2.0, 0.0, 0.0]).expect("ray hit");
    assert!((hit.t - 0.5).abs() < 1e-4, "t={}", hit.t);
    assert!(w.overlaps_aabb([0.0, 0.0, 0.0], [0.1, 0.1, 0.1]));
    assert!(!w.overlaps_aabb([2.0, 0.0, 0.0], [0.1, 0.1, 0.1]));
    assert!(!w.overlap_aabb([0.0, 0.0, 0.0], [0.1, 0.1, 0.1]).is_empty());
}

#[test]
fn degenerate_triangles_are_dropped() {
    let mut entries: Vec<(Triangle, u32)> = wall_x(0.0).into_iter().map(|t| (t, 0)).collect();
    entries.push(([[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]], 9)); // collinear
    entries.push(([[0.0, 0.0, 0.0], [0.0, 0.0, 0.0], [0.0, 0.0, 0.0]], 9)); // point
    entries.push(([[0.0, 0.0, 0.0], [f32::NAN, 0.0, 0.0], [1.0, 1.0, 1.0]], 9)); // non-finite
    let w = world(entries);
    assert_eq!(w.triangle_count(), 2);
    assert_eq!(w.degenerate_count(), 3);
}

/// Simple deterministic LCG (no `rand` crate).
struct Lcg(u64);

impl Lcg {
    fn next_f32(&mut self) -> f32 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 40) as f32) / ((1u64 << 24) as f32)
    }
    fn range(&mut self, a: f32, b: f32) -> f32 {
        a + (b - a) * self.next_f32()
    }
}

/// Brute-force sweep over every triangle (test-only oracle).
fn brute_sweep(
    w: &CollisionWorld,
    start: Vec3,
    end: Vec3,
    half: Vec3,
    skip: bool,
) -> Option<SweepHit> {
    let d = sub(end, start);
    let mut best: Option<SweepHit> = None;
    for i in 0..w.triangle_count() as u32 {
        if let Some(mut h) = sweep_triangle(start, d, half, w.triangle(i), i, w.source(i)) {
            if h.start_penetrating && skip && !start_drives_into(d, start, w.triangle(i)) {
                continue;
            }
            if h.t < 0.0 {
                h.t = 0.0;
                h.start_penetrating = true;
            }
            if best.is_none_or(|b| h.t < b.t) {
                best = Some(h);
            }
        }
    }
    best
}

#[test]
fn bvh_matches_brute_force_on_random_soup() {
    let mut rng = Lcg(0x1234_5678_9abc_def0);
    let mut entries: Vec<(Triangle, u32)> = Vec::new();
    for s in 0..400u32 {
        let o = [
            rng.range(-10.0, 10.0),
            rng.range(-10.0, 10.0),
            rng.range(-10.0, 10.0),
        ];
        let t = [
            o,
            [
                o[0] + rng.range(-2.0, 2.0),
                o[1] + rng.range(-2.0, 2.0),
                o[2] + rng.range(-2.0, 2.0),
            ],
            [
                o[0] + rng.range(-2.0, 2.0),
                o[1] + rng.range(-2.0, 2.0),
                o[2] + rng.range(-2.0, 2.0),
            ],
        ];
        entries.push((t, s));
    }
    let w = world(entries);
    assert!(w.bvh_node_count() > 0);
    for _ in 0..500 {
        let start = [
            rng.range(-12.0, 12.0),
            rng.range(-12.0, 12.0),
            rng.range(-12.0, 12.0),
        ];
        let end = [
            rng.range(-12.0, 12.0),
            rng.range(-12.0, 12.0),
            rng.range(-12.0, 12.0),
        ];
        let half = [
            rng.range(0.05, 1.0),
            rng.range(0.05, 1.0),
            rng.range(0.05, 1.0),
        ];
        let a = w.sweep(start, end, half);
        let b = brute_sweep(&w, start, end, half, true);
        match (a, b) {
            (None, None) => {}
            (Some(x), Some(y)) => {
                assert!((x.t - y.t).abs() < 1e-4, "t {} vs {}", x.t, y.t);
            }
            (x, y) => panic!("bvh {x:?} != brute {y:?}"),
        }
    }
}

#[test]
fn downward_ray_onto_floor_hits() {
    // Regression: a zero-extent ray straight down onto a coplanar floor must hit; the swept
    // SAT can miss it when the edge-axis cross products are near-degenerate for a point.
    let w = world(floor_y(0.0).into_iter().map(|t| (t, 1)));
    let hit = w
        .ray([0.0, 1.0, 0.0], [0.0, -1.0, 0.0])
        .expect("downward ray must hit the floor");
    assert!((hit.t - 0.5).abs() < 1e-4, "t={}", hit.t);
    assert!(hit.normal[1] > 0.9, "normal={:?}", hit.normal);
    let hit = w
        .sweep([0.0, 1.0, 0.0], [0.0, -1.0, 0.0], [0.01; 3])
        .expect("downward sweep must hit the floor");
    assert!(hit.normal[1] > 0.9, "normal={:?}", hit.normal);
}
