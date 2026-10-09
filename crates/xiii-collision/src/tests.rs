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
        ..SweepParams::default()
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
        ..SweepParams::default()
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
fn static_mesh_ray_rejects_a_backface_from_behind_the_collision_face() {
    // Converted Plage01 desk top: Bevy's up normal is +Y. The key ray starts below the
    // triangle and travels upward, so this is the backface direction for this winding.
    let tri = [[-1.0, 0.0, -1.0], [-1.0, 0.0, 1.0], [1.0, 0.0, 1.0]];
    let w = world(vec![(tri, 7)]);
    let mut one_sided = vec![false; 8];
    one_sided[7] = true;
    assert!(
        w.ray_with_one_sided_sources([-0.5, -0.1, 0.5], [-0.5, 0.1, 0.5], &one_sided)
            .is_none(),
        "a ray starting behind the one-sided mesh face leaves without a hit"
    );
    assert!(
        w.ray_with_one_sided_sources([-0.5, 0.1, 0.5], [-0.5, -0.1, 0.5], &one_sided)
            .is_some(),
        "the matching front-facing direction still hits"
    );
    assert!(
        w.ray([-0.5, -0.1, 0.5], [-0.5, 0.1, 0.5]).is_some(),
        "unclassified sources retain the legacy two-sided behavior"
    );
}

#[test]
fn ray_from_inside_a_closed_two_sided_soup_still_reports_the_exit_surface() {
    let cube = [
        [[-1.0, -1.0, -1.0], [1.0, -1.0, -1.0], [1.0, 1.0, -1.0]],
        [[-1.0, -1.0, -1.0], [1.0, 1.0, -1.0], [-1.0, 1.0, -1.0]],
        [[-1.0, -1.0, 1.0], [1.0, 1.0, 1.0], [1.0, -1.0, 1.0]],
        [[-1.0, -1.0, 1.0], [-1.0, 1.0, 1.0], [1.0, 1.0, 1.0]],
        [[-1.0, -1.0, -1.0], [-1.0, 1.0, -1.0], [-1.0, 1.0, 1.0]],
        [[-1.0, -1.0, -1.0], [-1.0, 1.0, 1.0], [-1.0, -1.0, 1.0]],
        [[1.0, -1.0, -1.0], [1.0, -1.0, 1.0], [1.0, 1.0, 1.0]],
        [[1.0, -1.0, -1.0], [1.0, 1.0, 1.0], [1.0, 1.0, -1.0]],
        [[-1.0, -1.0, -1.0], [-1.0, -1.0, 1.0], [1.0, -1.0, 1.0]],
        [[-1.0, -1.0, -1.0], [1.0, -1.0, 1.0], [1.0, -1.0, -1.0]],
        [[-1.0, 1.0, -1.0], [1.0, 1.0, 1.0], [-1.0, 1.0, 1.0]],
        [[-1.0, 1.0, -1.0], [1.0, 1.0, -1.0], [1.0, 1.0, 1.0]],
    ];
    let w = world(cube.into_iter().map(|tri| (tri, 3)));
    let hit = w.ray([0.0, 0.0, 0.0], [2.0, 0.0, 0.0]).expect("exit hit");
    assert!(hit.t > 0.0 && hit.t < 1.0, "exit surface time: {}", hit.t);
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

/// `origin` maps a retained triangle back to its position in the input sequence even when
/// earlier entries were dropped as degenerate (so a caller can look up per-input data, e.g. a
/// floor triangle's surface material).
#[test]
fn origin_maps_retained_triangles_to_input_positions() {
    let entries: Vec<(Triangle, u32)> = vec![
        // A degenerate triangle first, so retained index 0 has input origin 1.
        ([[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [2.0, 0.0, 0.0]], 10),
        ([[0.0, 0.0, 0.0], [1.0, 0.0, 0.0], [0.0, 0.0, 1.0]], 11),
        ([[1.0, 0.0, 0.0], [1.0, 0.0, 1.0], [0.0, 0.0, 1.0]], 12),
        ([[0.0; 3]; 3], 13), // degenerate (point)
    ];
    let w = world(entries);
    assert_eq!(w.triangle_count(), 2);
    // Retained 0 is input 1, retained 1 is input 2; the degenerate inputs 0 and 3 are dropped.
    assert_eq!(w.origin(0), 1);
    assert_eq!(w.origin(1), 2);
    assert_eq!(w.source(0), 11);
    assert_eq!(w.source(1), 12);
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

// ---------------------------------------------------------------------------------------
// walk_move (UE2-style walking)
// ---------------------------------------------------------------------------------------

fn big_floor(y: f32) -> Vec<Triangle> {
    quad(
        [-60.0, y, -60.0],
        [60.0, y, -60.0],
        [60.0, y, 60.0],
        [-60.0, y, 60.0],
    )
}

/// Walks an extent box toward +X by `step` for `steps` calls of `walk_move`, returning the
/// last result and whether `falling` was ever reported.
fn walk_plus_x(
    world: &CollisionWorld,
    start: Vec3,
    half: Vec3,
    step: f32,
    steps: usize,
    params: &WalkParams,
) -> (MoveResult, bool) {
    let mut pos = start;
    let mut fell = false;
    let mut last = walk_move(world, pos, [step, 0.0, 0.0], half, params);
    for _ in 0..steps {
        last = walk_move(world, pos, [step, 0.0, 0.0], half, params);
        pos = last.position;
        fell |= last.falling;
    }
    (last, fell)
}

fn walk_params(max_step_height: f32) -> WalkParams {
    WalkParams {
        skin: 0.001,
        max_iterations: 4,
        max_step_height,
        min_floor_z: 0.7,
    }
}

#[test]
fn walk_over_low_plank_edge_passes() {
    // Lower floor at y=0, a 1.8 cm raised deck from x=0, and its 1.8 cm vertical face.
    let mut tris: Vec<(Triangle, u32)> = big_floor(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(
        quad(
            [0.0, 0.018, -1.0],
            [5.0, 0.018, -1.0],
            [5.0, 0.018, 1.0],
            [0.0, 0.018, 1.0],
        )
        .into_iter()
        .map(|t| (t, 2)),
    );
    tris.extend(
        quad(
            [0.0, 0.0, -1.0],
            [0.0, 0.018, -1.0],
            [0.0, 0.018, 1.0],
            [0.0, 0.0, 1.0],
        )
        .into_iter()
        .map(|t| (t, 2)),
    );
    let w = world(tris);
    let half = [0.3, 0.5, 0.3];
    let (last, _) = walk_plus_x(&w, [-1.0, 0.5, 0.0], half, 0.05, 40, &walk_params(0.25));
    assert!(
        last.position[0] > 0.5,
        "crossed the plank: {:?}",
        last.position
    );
    assert!(
        (last.position[1] - (0.018 + 0.5)).abs() < 0.01,
        "stood on the deck: {:?}",
        last.position
    );
}

#[test]
fn walk_up_a_14_degree_ramp_passes() {
    // Lower floor for x<0 and a 14-degree surface rising from (0, 0).
    let slope = 14.0f32.to_radians().tan();
    let mut tris: Vec<(Triangle, u32)> = quad(
        [-5.0, 0.0, -1.0],
        [0.0, 0.0, -1.0],
        [0.0, 0.0, 1.0],
        [-5.0, 0.0, 1.0],
    )
    .into_iter()
    .map(|t| (t, 1))
    .collect();
    tris.extend(
        quad(
            [0.0, 0.0, -1.0],
            [20.0, 20.0 * slope, -1.0],
            [20.0, 20.0 * slope, 1.0],
            [0.0, 0.0, 1.0],
        )
        .into_iter()
        .map(|t| (t, 2)),
    );
    let w = world(tris);
    let half = [0.3, 0.5, 0.3];
    let (last, fell) = walk_plus_x(&w, [-1.0, 0.5, 0.0], half, 0.05, 300, &walk_params(0.25));
    assert!(
        last.position[0] > 1.0,
        "climbed the ramp: {:?}",
        last.position
    );
    assert!(
        last.position[1] > 0.6,
        "gained height on the ramp: {:?}",
        last.position
    );
    assert!(!fell, "never fell off the ramp");
}

#[test]
fn walk_step_below_max_height_passes_above_is_blocked() {
    let half = [0.3, 0.5, 0.3];
    let build = |h: f32| {
        let mut tris: Vec<(Triangle, u32)> = floor_y(0.0).into_iter().map(|t| (t, 1)).collect();
        tris.extend(box_tris([0.0, 0.0, -1.0], [1.0, h, 1.0], 2));
        world(tris)
    };
    // Just below the 0.25 m limit: step up onto it.
    let below = build(0.2);
    let (last, _) = walk_plus_x(&below, [-0.5, 0.5, 0.0], half, 0.05, 30, &walk_params(0.25));
    assert!(last.position[0] > 0.0, "climbed: {:?}", last.position);
    assert!(
        (last.position[1] - (0.2 + 0.5)).abs() < 0.02,
        "on top: {:?}",
        last.position
    );
    // Just above the limit: blocked at the face, still on the lower floor.
    let above = build(0.3);
    let (last, _) = walk_plus_x(&above, [-0.5, 0.5, 0.0], half, 0.05, 30, &walk_params(0.25));
    assert!(last.position[0] < 0.0, "did not climb: {:?}", last.position);
    assert!(
        (last.position[1] - 0.5).abs() < 1e-3,
        "stayed on the floor: {:?}",
        last.position
    );
}

#[test]
fn walking_off_a_ledge_reports_falling() {
    // Floor only for x <= 0. The box walks +X off the edge; there is nothing below.
    let tris: Vec<(Triangle, u32)> = quad(
        [-5.0, 0.0, -1.0],
        [0.0, 0.0, -1.0],
        [0.0, 0.0, 1.0],
        [-5.0, 0.0, 1.0],
    )
    .into_iter()
    .map(|t| (t, 1))
    .collect();
    let w = world(tris);
    let half = [0.3, 0.5, 0.3];
    let (last, fell) = walk_plus_x(&w, [-0.5, 0.5, 0.0], half, 0.05, 60, &walk_params(0.25));
    assert!(fell, "walking off the ledge must report falling");
    assert!(last.position[0] > 0.3, "{:?}", last.position);
    assert!(
        last.falling,
        "the last result is airborne: {:?}",
        last.position
    );
}

#[test]
fn coplanar_corridor_floor_with_tiny_seams_never_sticks() {
    // A floor of 0.5 m tiles, each at a random height within 2 mm. The box must keep moving
    // and never fall through a seam.
    let mut rng = Lcg(0x0bad_c0de_1234_5678);
    let mut tris: Vec<(Triangle, u32)> = Vec::new();
    let mut x = -2.0f32;
    while x < 12.0 {
        let y0 = rng.range(0.0, 0.002);
        let y1 = rng.range(0.0, 0.002);
        tris.extend(
            quad(
                [x, y0, -1.0],
                [x + 0.5, y1, -1.0],
                [x + 0.5, y1, 1.0],
                [x, y0, 1.0],
            )
            .into_iter()
            .map(|t| (t, 1)),
        );
        x += 0.5;
    }
    let w = world(tris);
    let half = [0.3, 0.5, 0.3];
    let start = [0.0, 0.5, 0.0];
    let mut pos = start;
    let mut fell = false;
    for _ in 0..200 {
        let r = walk_move(&w, pos, [0.05, 0.0, 0.0], half, &walk_params(0.25));
        assert!(!r.falling, "fell through a seam at {pos:?}");
        assert!(
            r.position[0] >= pos[0] - 1e-4,
            "went backwards at {pos:?} -> {:?}",
            r.position
        );
        pos = r.position;
        fell |= r.falling;
    }
    assert!(!fell);
    assert!(pos[0] > 9.0, "walked along the floor: {pos:?}");
}

#[test]
fn walk_does_not_tunnel_through_a_wall_after_1000_steps() {
    let mut tris: Vec<(Triangle, u32)> = big_floor(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(
        quad(
            [0.0, -50.0, -50.0],
            [0.0, 50.0, -50.0],
            [0.0, 50.0, 50.0],
            [0.0, -50.0, 50.0],
        )
        .into_iter()
        .map(|t| (t, 2)),
    );
    let w = world(tris);
    let half = [0.3, 0.5, 0.3];
    let params = walk_params(0.25);
    let mut pos = [-0.5, 0.5, 0.0];
    for _ in 0..1000 {
        let r = walk_move(&w, pos, [0.01, 0.0, 0.05], half, &params);
        pos = r.position;
        assert!(
            pos[0] <= -0.29,
            "box crossed to the far side of the wall: x={}",
            pos[0]
        );
    }
    assert!(pos[2] > 10.0, "slid along the wall, z={}", pos[2]);
}

// ---------------------------------------------------------------------------------------
// walk_move against the failure shapes reported for Plage01 (player-scale box, 90 UU/m).
// ---------------------------------------------------------------------------------------

/// Unreal units per metre at the importer's current scale (see `xiii-decode::common`).
const UU_PER_M: f32 = 90.0;
/// Player extent half-size (radius, half height, radius) in metres (R 34, H 75 UU).
const PLAYER_HALF: Vec3 = [34.0 / UU_PER_M, 75.0 / UU_PER_M, 34.0 / UU_PER_M];
/// Reach walk step in metres (2.5 UU).
const PLAYER_STEP: f32 = 2.5 / UU_PER_M;
/// UE2 `MAXSTEPHEIGHT` in metres (35 UU).
const PLAYER_MAX_STEP: f32 = 35.0 / UU_PER_M;
/// The reported 1.8 UU plank/ledge rise in metres.
const LEDGE_H: f32 = 1.8 / UU_PER_M;

fn player_params() -> WalkParams {
    WalkParams {
        skin: 0.05 / UU_PER_M,
        max_iterations: 4,
        max_step_height: PLAYER_MAX_STEP,
        min_floor_z: 0.7,
    }
}

/// A big floor at y=0 plus a raised deck at `h` whose leading edge is a shallow (14 degree)
/// bevel with a walkable normal, starting at x=`x0`.
fn bevel_ledge(x0: f32, h: f32) -> Vec<(Triangle, u32)> {
    let bevel_w = h / 14.0f32.to_radians().tan();
    let mut tris: Vec<(Triangle, u32)> = big_floor(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(
        quad(
            [x0, 0.0, -1.0],
            [x0 + bevel_w, h, -1.0],
            [x0 + bevel_w, h, 1.0],
            [x0, 0.0, 1.0],
        )
        .into_iter()
        .map(|t| (t, 2)),
    );
    tris.extend(
        quad(
            [x0 + bevel_w, h, -1.0],
            [x0 + 4.0, h, -1.0],
            [x0 + 4.0, h, 1.0],
            [x0 + bevel_w, h, 1.0],
        )
        .into_iter()
        .map(|t| (t, 2)),
    );
    tris
}

#[test]
fn walk_steps_onto_a_ledge_behind_a_walkable_bevel() {
    // The blocking contact is the walkable bevel (normal up ~0.97), so the old walkability
    // gate never attempted a step and the box stalled at the bevel.
    let w = world(bevel_ledge(0.0, LEDGE_H));
    let (last, fell) = walk_plus_x(
        &w,
        [-1.5, PLAYER_HALF[1], 0.0],
        PLAYER_HALF,
        PLAYER_STEP,
        120,
        &player_params(),
    );
    assert!(
        last.position[0] > 0.4,
        "did not cross the bevel: {:?}",
        last.position
    );
    assert!(
        (last.position[1] - (LEDGE_H + PLAYER_HALF[1])).abs() < 1e-3,
        "not standing on the deck: {:?}",
        last.position
    );
    assert!(!fell, "never fell crossing the bevel");
}

#[test]
fn large_box_crosses_a_series_of_walkable_plank_seams() {
    // Several 1.8 UU planks, each reached by a walkable bevel: the 150 UU box must step over
    // every seam, not stall at the first.
    let mut tris: Vec<(Triangle, u32)> = big_floor(0.0).into_iter().map(|t| (t, 1)).collect();
    let plank_w = 0.6f32;
    let n = 4u32;
    for i in 0..n {
        let x0 = i as f32 * plank_w;
        let y0 = i as f32 * LEDGE_H;
        let y1 = (i + 1) as f32 * LEDGE_H;
        let bw = LEDGE_H / 14.0f32.to_radians().tan();
        tris.extend(
            quad(
                [x0, y0, -1.0],
                [x0 + bw, y1, -1.0],
                [x0 + bw, y1, 1.0],
                [x0, y0, 1.0],
            )
            .into_iter()
            .map(|t| (t, 2)),
        );
        tris.extend(
            quad(
                [x0 + bw, y1, -1.0],
                [x0 + plank_w, y1, -1.0],
                [x0 + plank_w, y1, 1.0],
                [x0 + bw, y1, 1.0],
            )
            .into_iter()
            .map(|t| (t, 2)),
        );
    }
    let w = world(tris);
    // Walk just far enough to end on the top plank, tracking the highest standing height.
    let params = player_params();
    let mut pos = [-1.0, PLAYER_HALF[1], 0.0];
    let mut max_y = pos[1];
    let mut fell = false;
    for _ in 0..110 {
        let r = walk_move(&w, pos, [PLAYER_STEP, 0.0, 0.0], PLAYER_HALF, &params);
        pos = r.position;
        max_y = max_y.max(pos[1]);
        fell |= r.falling;
    }
    let top = n as f32 * LEDGE_H;
    assert!(pos[0] > 1.8, "did not cross the plank seams: {:?}", pos);
    assert!(
        (max_y - (top + PLAYER_HALF[1])).abs() < 2e-3,
        "did not reach the top plank: max_y={max_y} expected {}",
        top + PLAYER_HALF[1]
    );
    assert!(!fell, "never fell crossing the seams");
}

#[test]
fn walk_steps_onto_a_ledge_under_an_overhang_that_blocks_the_full_up_sweep() {
    // A down-facing overhang leaves only 0.35 m of headroom, less than MAXSTEPHEIGHT (0.389 m),
    // so the full up-sweep is blocked. The required rise is only the 1.8 UU ledge, so the step
    // must still succeed by not sweeping the full height (UE2 uses the sweep hit time).
    let mut tris = bevel_ledge(0.0, LEDGE_H);
    let ceiling = 2.0 * PLAYER_HALF[1] + 0.35;
    tris.extend(
        quad(
            [-2.0, ceiling, -1.0],
            [0.0, ceiling, -1.0],
            [0.0, ceiling, 1.0],
            [-2.0, ceiling, 1.0],
        )
        .into_iter()
        .map(|t| (t, 3)),
    );
    let w = world(tris);
    let (last, fell) = walk_plus_x(
        &w,
        [-1.5, PLAYER_HALF[1], 0.0],
        PLAYER_HALF,
        PLAYER_STEP,
        120,
        &player_params(),
    );
    assert!(
        last.position[0] > 0.4,
        "did not step through under the overhang: {:?}",
        last.position
    );
    assert!(
        (last.position[1] - (LEDGE_H + PLAYER_HALF[1])).abs() < 1e-3,
        "not on the deck: {:?}",
        last.position
    );
    assert!(!fell, "never fell stepping under the overhang");
}

// ---------------------------------------------------------------------------------------
// Evidence constants measured from Engine.dll (item1j). These pin the values the primitive is
// calibrated against so a later edit that changes them fails loudly instead of silently drifting.
// ---------------------------------------------------------------------------------------

/// The engine collision constants read from `Engine.dll` `.rdata`. Each is `(symbol, vma, value)`.
///
/// Sources (all **from source**, `llvm-objdump`, read-only; raw in `local/re/`):
/// - `MINFLOORZ` = 0.7: `APawn::physWalking` (`0x103be05b`) and `APawn::stepUp` (`0x103baa96`)
///   compare a floor normal's Z against `0x10483428`.
/// - `MAXSTEPHEIGHT` = 35.0 UU: `APawn::stepUp` multiplies the requested `(X,Y,Z)` by
///   `0x104829c4` (`35.0`), the fixed up-step magnitude.
/// - `physWalking` step loop bound = 8: `cmpl $0x8, <iter>` at `0x103bde14`.
/// - `stepUp` normal threshold = 0.08: `0x1048341c` is compared against the check result's Z at
///   `0x103baa96`; `0x10483420` = 1.9 and `0x10483424` = 2.4 are the floor-snap tolerances.
/// - `ATerrainInfo::LineCheck` / `UModel::LineCheck` clamp the ray to the heightmap and test
///   `0.5`/`0.1` barycentric thresholds (`0x1046f584` = 0.5, `0x10478ca4` = 0.1) — line checks
///   with extent, not a swept box.
const ENGINE_MIN_FLOOR_Z: f32 = 0.7;
const ENGINE_MAX_STEP_HEIGHT_UU: f32 = 35.0;
const ENGINE_PHYSWALKING_ITERATIONS: u32 = 8;
const ENGINE_FLOOR_SNAP_TOL_UU: f32 = 2.4;

#[test]
fn evidence_constants_match_the_crate_defaults_and_reach() {
    // `WalkParams::default()` is the engine's `MINFLOORZ`; the other engine values are supplied by
    // the `xiii-world::reach` caller (kept in Unreal units there).
    assert_eq!(WalkParams::default().min_floor_z, ENGINE_MIN_FLOOR_Z);
    assert_eq!(ENGINE_MAX_STEP_HEIGHT_UU, 35.0);
    assert_eq!(ENGINE_PHYSWALKING_ITERATIONS, 8);
    assert_eq!(ENGINE_FLOOR_SNAP_TOL_UU, 2.4);
}

/// The engine's `stepUp` is gated on the **floor** normal, not the horizontal blocking contact.
/// Our `walk_move` attempts the step on any horizontal block and requires a walkable *landing*
/// in `try_step_walk`. This test pins that design: a vertical obstacle face (unwalkable, `n_y=0`)
/// with a walkable top must still be stepped onto, which the engine gate applied to the contact
/// normal would not do.
#[test]
fn vertical_face_is_stepped_onto_by_the_ungated_heuristic() {
    let h = 0.2;
    let half = [0.3, 0.5, 0.3];
    let mut tris: Vec<(Triangle, u32)> = floor_y(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(box_tris([0.0, 0.0, -1.0], [1.0, h, 1.0], 2));
    let w = world(tris);
    let (last, fell) = walk_plus_x(&w, [-0.5, 0.5, 0.0], half, 0.05, 30, &walk_params(0.25));
    assert!(
        last.position[0] > 0.0,
        "vertical face with a walkable top was not stepped onto: {:?}",
        last.position
    );
    assert!(
        (last.position[1] - (h + 0.5)).abs() < 0.02,
        "not standing on the step top: {:?}",
        last.position
    );
    assert!(!fell);
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

// ---- moving (dynamic) collision objects ---------------------------------------------------

/// Identity rotation rows.
const ID: [Vec3; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

// ---- ULevel::FindSpot port (item1k) --------------------------------------------------------

fn spot_params() -> FindSpotParams {
    FindSpotParams {
        min_floor_z: 0.7,
        drop: 3.0,
        raise_step: 0.0,
        max_raise: 0.0,
    }
}

#[test]
fn find_spot_free_center_settles_on_floor() {
    let w = world(floor_y(0.0).into_iter().map(|t| (t, 1)));
    let spot = find_spot(&w, [0.0, 1.0, 0.0], [0.4, 0.4, 0.4], &spot_params())
        .expect("free centre must place");
    assert!(spot.free_corners.is_none(), "no search when already free");
    assert!(
        (spot.position[1] - 0.4).abs() < 1e-3,
        "settled onto the floor: {:?}",
        spot.position
    );
    assert!((spot.floor - 0.0).abs() < 1e-3);
    assert!(spot.offset[0].abs() < 1e-4 && spot.offset[2].abs() < 1e-4);
}

#[test]
fn find_spot_zero_extent_is_rejected() {
    let w = world(floor_y(0.0).into_iter().map(|t| (t, 1)));
    assert_eq!(
        find_spot(&w, [0.0, 1.0, 0.0], [0.0, 0.4, 0.4], &spot_params()),
        Err(FindSpotError::ZeroExtent)
    );
}

#[test]
fn find_spot_without_floor_is_no_floor() {
    let w = world(std::iter::empty());
    assert_eq!(
        find_spot(&w, [0.0, 1.0, 0.0], [0.4, 0.4, 0.4], &spot_params()),
        Err(FindSpotError::NoFloor)
    );
}

/// Builds a 0.4 m overhang slab at `y` spanning the search area: every position at that height
/// overlaps it, so the corner search finds nothing.
fn slab(y: f32) -> Vec<(Triangle, u32)> {
    quad(
        [-5.0, y, -5.0],
        [5.0, y, -5.0],
        [5.0, y, 5.0],
        [-5.0, y, 5.0],
    )
    .into_iter()
    .map(|t| (t, 9))
    .collect()
}

#[test]
fn find_spot_embedded_raises_when_enabled() {
    let mut entries = slab(1.0);
    entries.extend(floor_y(0.0).into_iter().map(|t| (t, 1)));
    let w = world(entries);
    let params = FindSpotParams {
        raise_step: 0.1,
        max_raise: 2.0,
        ..spot_params()
    };
    let spot = find_spot(&w, [0.0, 1.0, 0.0], [0.4, 0.4, 0.4], &params)
        .expect("the vertical raise fallback must free the embedded box");
    assert!(
        spot.position[1] > 1.0,
        "raised above the slab and settled: {:?}",
        spot.position
    );
}

#[test]
fn find_spot_embedded_without_raise_is_no_free_spot() {
    let mut entries = slab(1.0);
    entries.extend(floor_y(0.0).into_iter().map(|t| (t, 1)));
    let w = world(entries);
    // The slab blocks the centre and all four corners; raising is disabled.
    assert_eq!(
        find_spot(&w, [0.0, 1.0, 0.0], [0.4, 0.4, 0.4], &spot_params()),
        Err(FindSpotError::NoFreeSpot)
    );
}

#[test]
fn find_spot_single_free_corner_extrapolates() {
    // Three small obstacles block the centre and the (−X,−Z), (+X,−Z) and (−X,+Z) corners;
    // only (+X,+Z) is free, so FindSpot extrapolates to twice that offset.
    let mut entries: Vec<(Triangle, u32)> = floor_y(0.0).into_iter().map(|t| (t, 1)).collect();
    for (min, max) in [
        ([-0.35, -1.0, -0.35], [-0.22, 1.0, -0.22]),
        ([0.22, -1.0, -0.35], [0.35, 1.0, -0.22]),
        ([-0.35, -1.0, 0.22], [-0.22, 1.0, 0.35]),
    ] {
        entries.extend(box_tris(min, max, 3));
    }
    let w = world(entries);
    let spot = find_spot(&w, [0.0, 1.0, 0.0], [0.4, 0.4, 0.4], &spot_params())
        .expect("one free corner must place");
    assert_eq!(spot.free_corners, Some(1), "{spot:?}");
    assert!(
        (spot.offset[0] - 0.4).abs() < 1e-3 && (spot.offset[2] - 0.4).abs() < 1e-3,
        "extrapolated to the doubled offset: {:?}",
        spot.offset
    );
}

#[test]
fn moving_wall_blocks_then_unblocks_a_sweep() {
    // No static geometry: the only obstacle is the moving wall at x = 0.
    let mut w = CollisionWorld::new(std::iter::empty());
    let tris: Vec<Triangle> = wall_x(0.0);
    let idx = w.add_moving(MovingObject::from_world_triangles(tris, 7, [0.0; 3], ID));
    assert_eq!(w.moving_count(), 1);
    assert_eq!(w.moving(idx).unwrap().source(), 7);

    let start = [-2.0, 0.0, 0.0];
    let end = [2.0, 0.0, 0.0];
    let half = [0.1, 0.1, 0.1];
    let hit = w
        .sweep(start, end, half)
        .expect("the closed moving wall must block the sweep");
    assert_eq!(hit.source, 7, "the hit must name the moving object");
    assert!((hit.t - 0.475).abs() < 0.02, "t={}", hit.t);
    assert!(w.ray(start, end).is_some(), "ray must see the moving wall");

    // Slide the wall far away: the sweep is now clear and the moving object still exists.
    assert!(w.set_moving_transform(idx, [100.0, 0.0, 0.0], ID));
    assert!(
        w.sweep(start, end, half).is_none(),
        "an unblocked sweep must pass after the wall moves away"
    );
    assert!(w.ray(start, end).is_none());
    assert!(!w.overlaps_aabb([0.0, 0.0, 0.0], [0.1; 3]));
    assert!(w.overlaps_aabb([100.0, 0.0, 0.0], [0.1; 3]));
}

#[test]
fn disabled_moving_object_is_ignored_by_every_query_until_re_enabled() {
    // item40e: a destroyed/non-colliding mover (a broken grille) stays installed but must not
    // block sweeps, rays or overlaps; re-enabling restores it at its current pose.
    let mut w = CollisionWorld::new(std::iter::empty());
    let idx = w.add_moving(MovingObject::from_world_triangles(
        wall_x(0.0),
        7,
        [0.0; 3],
        ID,
    ));
    let (start, end, half) = ([-2.0, 0.0, 0.0], [2.0, 0.0, 0.0], [0.1, 0.1, 0.1]);
    assert!(w.sweep(start, end, half).is_some());
    assert!(w.set_moving_enabled(idx, false));
    assert!(!w.moving(idx).unwrap().is_enabled());
    assert!(
        w.sweep(start, end, half).is_none(),
        "disabled mover blocked a sweep"
    );
    assert!(w.ray(start, end).is_none(), "disabled mover blocked a ray");
    assert!(
        !w.overlaps_aabb([0.0; 3], [0.1; 3]),
        "disabled mover overlapped"
    );
    assert!(w.overlap_aabb([0.0; 3], [0.1; 3]).is_empty());
    assert!(w.set_moving_enabled(idx, true));
    assert_eq!(w.sweep(start, end, half).map(|h| h.source), Some(7));
    assert!(
        !w.set_moving_enabled(idx + 1, false),
        "out-of-range index must report false"
    );
}

#[test]
fn box_resting_on_a_coplanar_floor_crosses_a_triangle_edge_flush_with_its_front() {
    // item40e (Amos01 duct mouth): a box resting exactly on y = 0 whose front face lies exactly
    // on the leading edge of the next coplanar floor triangle must move onto it. Before, the
    // zero-depth contact on the static y axis counted as overlap, so the edge returned a
    // horizontal hit at t = 0 and the box stalled (step-up had no headroom under a duct roof).
    let tris: Vec<(Triangle, u32)> = vec![
        ([[0.5, 0.0, -1.0], [0.4, 0.0, 1.0], [0.5, 0.0, 1.0]], 1),
        ([[-3.0, 0.0, -1.0], [0.5, 0.0, -1.0], [-3.0, 0.0, 1.0]], 1),
    ];
    let w = world(tris);
    let half = [0.5, 0.5, 0.5];
    let start = [1.0, 0.5, 0.0]; // front face x = 0.5, bottom y = 0
    assert!(
        w.sweep(start, [-2.0, 0.5, 0.0], half).is_none(),
        "a coplanar floor edge flush with the box front must not block"
    );
    // A flush wall alongside the motion behaves the same way (side face exactly on z = 0.5).
    let wall: Vec<(Triangle, u32)> = vec![
        ([[0.5, 0.0, 1.0], [0.5, 1.0, 1.0], [-3.0, 1.0, 1.0]], 2),
        ([[0.5, 0.0, 1.0], [-3.0, 1.0, 1.0], [-3.0, 0.0, 1.0]], 2),
    ];
    let w = world(wall);
    assert!(
        w.sweep([1.0, 0.5, 0.5], [-2.0, 0.5, 0.5], half).is_none(),
        "a wall flush with the box side must not block motion along it"
    );
}

#[test]
fn a_real_overlap_on_the_static_axis_still_blocks() {
    // Guard for the touching rule: a 1 cm slab top above the box bottom is a genuine obstacle
    // (overlap far above the 1e-5 m contact tolerance) and must stop the sweep at its face.
    let slab = box_tris([-1.0, -0.2, -1.0], [0.0, 0.01, 1.0], 3);
    let w = world(slab);
    let hit = w
        .sweep([1.0, 0.5, 0.0], [-2.0, 0.5, 0.0], [0.5, 0.5, 0.5])
        .expect("the raised slab must block");
    assert!((hit.t - 0.5 / 3.0).abs() < 1e-3, "t={}", hit.t);
    assert!(hit.normal[0] > 0.9, "normal {:?}", hit.normal);
    // A flat sheet 0.005 mm above the box bottom (below the tolerance) is a touching contact.
    let sheet: Vec<(Triangle, u32)> = quad(
        [-1.0, 0.000_005, -1.0],
        [0.0, 0.000_005, -1.0],
        [0.0, 0.000_005, 1.0],
        [-1.0, 0.000_005, 1.0],
    )
    .into_iter()
    .map(|t| (t, 3))
    .collect();
    assert!(
        world(sheet)
            .sweep([1.0, 0.5, 0.0], [-2.0, 0.5, 0.0], [0.5, 0.5, 0.5])
            .is_none()
    );
}

#[test]
fn a_door_rotating_open_clears_a_doorway() {
    // Static wall in the x = 0 plane, y in [0,2], with a doorway gap for z in [-1, 1]; the
    // door leaf is a dynamic box hinged at (0, 0, -1) that fills the gap when closed.
    let mut entries: Vec<(Triangle, u32)> = Vec::new();
    entries.extend(
        quad(
            [0.0, 0.0, -5.0],
            [0.0, 2.0, -5.0],
            [0.0, 2.0, -1.0],
            [0.0, 0.0, -1.0],
        )
        .into_iter()
        .map(|t| (t, 1)),
    );
    entries.extend(
        quad(
            [0.0, 0.0, 1.0],
            [0.0, 2.0, 1.0],
            [0.0, 2.0, 5.0],
            [0.0, 0.0, 5.0],
        )
        .into_iter()
        .map(|t| (t, 1)),
    );
    let mut w = CollisionWorld::new(entries);
    let hinge = [0.0, 0.0, -1.0];
    // Door leaf closed in world space: a box x in [-0.1,0.1], y in [0,2], z in [-1,1].
    let leaf_world: Vec<Triangle> = box_tris([-0.1, 0.0, -1.0], [0.1, 2.0, 1.0], 9)
        .into_iter()
        .map(|(t, _)| t)
        .collect();
    let idx = w.add_moving(MovingObject::from_world_triangles(leaf_world, 9, hinge, ID));

    let start = [-3.0, 1.0, 0.0];
    let end = [3.0, 1.0, 0.0];
    let half = [0.05, 0.05, 0.3];
    let hit = w
        .sweep(start, end, half)
        .expect("the closed door must block the doorway");
    assert_eq!(
        hit.source, 9,
        "the door leaf hit must name the moving object"
    );

    // Rotate 90 degrees about the vertical (Y) axis: rows of R = [[0,0,1],[0,1,0],[-1,0,0]].
    let r90: [Vec3; 3] = [[0.0, 0.0, 1.0], [0.0, 1.0, 0.0], [-1.0, 0.0, 0.0]];
    w.set_moving_transform(idx, hinge, r90);
    assert!(
        w.sweep(start, end, half).is_none(),
        "an open door must clear the doorway"
    );
    // The open leaf now lies along +x at z ~ -1, off the doorway line (z = 0).
    assert!(w.overlaps_aabb([1.0, 1.0, -1.0], [0.3; 3]));
    assert!(!w.overlaps_aabb([0.0, 1.0, 0.0], [0.05; 3]));
}

#[test]
fn walking_into_a_moving_wall_is_blocked_and_clears_when_it_moves() {
    // `walk_move` must consult the dynamic set through its internal `sweep_aabb`.
    let mut w = CollisionWorld::new(floor_y(0.0).into_iter().map(|t| (t, 1)));
    let idx = w.add_moving(MovingObject::from_world_triangles(
        wall_x(0.5),
        4,
        [0.0; 3],
        ID,
    ));
    let params = WalkParams {
        max_step_height: 0.4,
        ..Default::default()
    };
    let start = [0.0, 0.5, 0.0];
    let blocked = walk_move(&w, start, [2.0, 0.0, 0.0], [0.1; 3], &params);
    assert!(blocked.blocked, "the wall must block the walker");
    assert!(
        blocked.position[0] < 0.5,
        "the walker stopped before the wall: {:?}",
        blocked.position
    );
    w.set_moving_transform(idx, [50.0, 0.0, 0.0], ID);
    let clear = walk_move(&w, start, [2.0, 0.0, 0.0], [0.1; 3], &params);
    assert!(
        clear.position[0] > 1.0,
        "the walker must pass after the wall moves: {:?}",
        clear.position
    );
}

#[test]
fn sweep_discard_start_penetration_lets_a_wedged_mover_escape() {
    // The engine's `ULevel::MoveActor` discards world hits inside the 2-UU back-off behind
    // the start (Engine.dll 0x1038a89a-0x1038ac05, item27k): for this crate's continuous sweep
    // that reduces to discarding start-penetrating hits. A box overlapping a wall and moving
    // into it must NOT be reported as blocked when the discard is on (walk movement), while
    // the default still blocks (the plain tests above).
    let mut tris: Vec<(Triangle, u32)> = wall_x(0.0).into_iter().map(|t| (t, 4)).collect();
    // A second wall the box reaches while clear of the first.
    tris.extend(wall_x(1.0).into_iter().map(|t| (t, 5)));
    let w = world(tris);
    let start = [-0.3 + 1e-4, 0.0, 0.0];
    let params = SweepParams {
        discard_start_penetration: true,
        walkable_floor_z: Some(0.7),
        ..SweepParams::default()
    };
    // The flush first wall is discarded...
    assert!(
        sweep_aabb(
            &w,
            start,
            [start[0] + 0.5, 0.0, 0.0],
            [0.3, 0.5, 0.3],
            &params
        )
        .is_none(),
        "a start-penetrating contact must not block with the discard on"
    );
    // ...but geometry reached from a clear box still blocks, with its own hit.
    let hit = sweep_aabb(
        &w,
        start,
        [start[0] + 2.0, 0.0, 0.0],
        [0.3, 0.5, 0.3],
        &params,
    )
    .expect("the second wall must still block");
    assert!(!hit.start_penetrating, "{hit:?}");
    assert!(hit.source == 5 && hit.normal[0] < -0.9, "{hit:?}");
}

#[test]
fn walk_repeated_steps_along_a_flush_wall_do_not_deadlock() {
    // Regression (item27k B-2): a pawn overlapping a wall and steering diagonally into it used
    // to make zero progress on every tick (the slide's float residual re-classified the same
    // start-penetrating contact as blocking forever). The engine's back-off discard lets each
    // step proceed; the pawn must advance along the wall.
    let mut tris: Vec<(Triangle, u32)> = big_floor(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(wall_x(0.0).into_iter().map(|t| (t, 4)));
    let w = world(tris);
    let half = [0.3, 0.5, 0.3];
    let params = walk_params(0.25);
    // Flush against the wall (penetrating by 1e-4 m), steering into it and along it.
    let mut pos = [-0.3 + 1e-4, 0.5, 0.0];
    let start_x = pos[0];
    for i in 0..20 {
        let r = walk_move(&w, pos, [0.05, 0.0, 0.05], half, &params);
        assert!(
            r.position[2] > pos[2] + 1e-4,
            "tick {i} made no progress along the wall: {:?}",
            r.position
        );
        pos = r.position;
    }
    assert!(
        pos[2] > 0.5,
        "must advance along the wall: x={} z={}",
        pos[0],
        pos[2]
    );
    let _ = start_x;
}

#[test]
fn jones_safe_wedge_synthetic_repro_walks_free() {
    // Synthetic reconstruction of the item27k B-2 deadlock (Banque01's safe desk): a wedge
    // whose front face is slanted (normal ~(0, 0.994, 0.112) in Unreal axes, the angle the
    // real blocker reports) with a pawn box (34,34,75 UU) overlapping the face band by ~8 UU
    // and steering into it. Before the back-off fix `walk_move` made zero progress forever on
    // the real soup (measured: 966 identical rejected steps, local/re/item27k/); the pawn must
    // now advance every tick. All coordinates are synthetic.
    const S: f32 = 90.0;
    let to_bevy = |u: [f32; 3]| [u[1] / S, u[2] / S, -u[0] / S];
    // Wedge in Unreal axes: slanted front face from (y=0, z=-30) rising to (y=-22.4, z=200),
    // flat back at y=60, x in [-100,100]; its bottom (z=-30) sits below the pawn's box bottom
    // (z=0) like the real safe's bottom sits below the desk-standing pawn. The pawn stands in
    // front (+y), its box overlapping the slanted face band by ~8 UU at its bottom tapering to
    // 0 (the real overlap shape), and steers with delta (-2.2, -1.17, 0) UU (into the face,
    // along -x).
    let (x0, x1, yb, z0, z1) = (-100.0f32, 100.0f32, 60.0f32, -30.0f32, 200.0f32);
    let face_y = |z: f32| -z * (0.112f32 / 0.994);
    let ya = face_y(z1); // -22.4
    let c = [
        [x0, face_y(z0), z0], // 0 front-bottom-left
        [x1, face_y(z0), z0], // 1 front-bottom-right
        [x1, ya, z1],         // 2 front-top-right
        [x0, ya, z1],         // 3 front-top-left
        [x0, yb, z0],         // 4 back-bottom-left
        [x1, yb, z0],         // 5 back-bottom-right
        [x1, yb, z1],         // 6 back-top-right
        [x0, yb, z1],         // 7 back-top-left
    ];
    let f = [
        [0, 1, 2],
        [0, 2, 3], // slanted front face
        [4, 6, 5],
        [4, 7, 6], // back face
        [0, 4, 5],
        [0, 5, 1], // bottom
        [3, 2, 6],
        [3, 6, 7], // top
        [0, 3, 7],
        [0, 7, 4], // left wall
        [1, 5, 6],
        [1, 6, 2], // right wall
    ];
    let raw: Vec<[[f32; 3]; 3]> = f.map(|t| t.map(|i| c[i])).to_vec();
    let world = CollisionWorld::new(raw.into_iter().map(|t| (t.map(to_bevy), 0u32)));
    let half = [34.0 / S, 75.0 / S, 34.0 / S];
    let params = WalkParams {
        skin: 0.001,
        max_iterations: 4,
        max_step_height: 35.0 / S,
        min_floor_z: 0.7,
    };
    // Spawn with the box overlapping the face band by ~8 UU (the real pawn overlapped by 7.9).
    let start_uu = [0.0f32, 26.0, 75.0];
    // Unreal x is -Bevy z after the importer's axis mapping.
    let ux = |p: [f32; 3]| -p[2] * S;
    let mut pos = to_bevy(start_uu);
    let start = ux(pos);
    for i in 0..12 {
        let r = walk_move(&world, pos, to_bevy([-2.2, -1.17, 0.0]), half, &params);
        assert!(
            ux(r.position) < ux(pos) - 1e-3,
            "tick {i} made no progress: {:?}",
            r.position
        );
        pos = r.position;
    }
    // 12 ticks x 2.2 UU = 26.4 UU of advance along -x (was exactly 0 before the fix); the
    // pawn advances until the wedge's left wall blocks the -x push and it slides along it.
    assert!(start - ux(pos) > 10.0, "net advance {}", start - ux(pos));
}

#[test]
fn remove_static_source_drops_only_that_source_and_rebuilds() {
    let mut tris: Vec<(Triangle, u32)> = floor_y(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(wall_x(0.0).into_iter().map(|t| (t, 4)));
    tris.extend(wall_x(1.0).into_iter().map(|t| (t, 5)));
    let mut w = world(tris);
    let half = [0.3, 0.5, 0.3];
    // Box riding above the floor (bottom at y=0.1) sweeping the corridor along +x.
    let sweep = |w: &CollisionWorld| w.sweep([-2.0, 0.6, 0.0], [3.0, 0.6, 0.0], half);
    // Before: both walls block.
    assert!(sweep(&w).is_some());
    let removed = w.remove_static_source(4);
    assert_eq!(removed, 2, "wall_x is a 2-triangle quad");
    // The remaining wall still blocks; the removed one no longer does.
    let hit = sweep(&w).expect("the surviving wall must block");
    assert_eq!(hit.source, 5, "the surviving wall must be the blocker");
    let removed5 = w.remove_static_source(5);
    assert_eq!(removed5, 2);
    assert_eq!(w.remove_static_source(4), 0, "removing twice is a no-op");
    assert!(
        sweep(&w).is_none(),
        "the corridor must be clear with both walls removed"
    );
}

#[test]
fn dbg_corner_deadlock() {
    // Floor + two walls forming an interior corner; the pawn box overlaps both walls slightly
    // and steers diagonally into the corner.
    let mut tris: Vec<(Triangle, u32)> = big_floor(0.0).into_iter().map(|t| (t, 1)).collect();
    tris.extend(wall_x(0.0).into_iter().map(|t| (t, 4)));
    // A wall in the z = const plane (x,y span): blocking the +z direction... use a z-wall via quad.
    let wall_z = |z: f32| -> Vec<Triangle> {
        quad(
            [-5.0, -5.0, z],
            [5.0, -5.0, z],
            [5.0, 5.0, z],
            [-5.0, 5.0, z],
        )
    };
    tris.extend(wall_z(0.0).into_iter().map(|t| (t, 5)));
    let w = world(tris);
    let half = [0.3, 0.5, 0.3];
    let params = walk_params(0.25);
    // Overlap wall_x by 1e-4 and wall_z by 1e-4: spawn at (-0.3+1e-4, 0.5, 0.3-1e-4).
    let mut pos = [-0.3 + 1e-4, 0.5, 0.3 - 1e-4];
    for i in 0..8 {
        let r = walk_move(&w, pos, [-0.05, 0.0, -0.05], half, &params);
        println!(
            "tick {i}: pos=({:.6},{:.6},{:.6}) blocked={} contacts={}",
            r.position[0],
            r.position[1],
            r.position[2],
            r.blocked,
            r.contacts.len()
        );
        pos = r.position;
    }
}
