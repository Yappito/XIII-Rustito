//! ReachSpec navigation walk test shared by `xiii-app --reach-test` and `xiii-tool campaign`.
//!
//! Every `ReachSpec` edge of a map (decoded by [`crate::navigation`]) that requires walking and
//! is wide/tall enough for the player class is walked with the UE2-style
//! [`xiii_collision::walk_move`] (walkable contact normal, step-up at `MAXSTEPHEIGHT` 35 UU,
//! floor-follow at `MINFLOORZ` 0.7). The player extent box is placed at the start node with the
//! ported `ULevel::FindSpot` ([`xiii_collision::find_spot`]), then the heading is re-aimed at the
//! end node every step.
//!
//! This module is Bevy-free: it was moved out of `xiii-app/src/reach.rs` so the headless
//! `xiii-tool` campaign sweep can call it. The app keeps the same `--reach-test` output by
//! calling [`analyze`] and its own `print_report`, which are byte-identical to the previous
//! implementation.
//!
//! **Pass rule (stated):** the walk passes when the final **horizontal** distance from the
//! player box centre to the end node centre is at most `player CollisionRadius + end node
//! CollisionRadius` (Unreal units), within a step budget of `3 x edge length` (the decoded
//! `ReachSpec.Distance`, or the horizontal start-to-end distance when that field is zero).
//! `walk_move` behaviour is not tuned here; failures are reported with their contact and
//! clearance numbers, grouped by a labelled heuristic cause.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use xiii_collision::{CollisionWorld, MoveContact, Vec3, WalkParams, walk_move};
use xiii_decode::common::{UNREAL_UNITS_PER_METER, to_bevy_position};
use xiii_install::{Installation, OpenOptions, PackageKind};
use xiii_package::{Limits, PropertyValue};
use xiii_script::{ScriptLimits, ScriptObject, ScriptPackage, ScriptSet, Value, Vm, VmLimits};

use crate::navigation::Navigation;
pub use crate::navigation::reach_flags;
use crate::{ClassDefaults, PackageCache};

/// UE2 `MINFLOORZ`: a surface is walkable (a floor) when its unit normal's up component is
/// at least this. **Measured in XIII's `Engine.dll`** (item1j): `APawn::physWalking`
/// (`0x103be05b`) and `APawn::stepUp` (`0x103baa96`) compare a floor normal's Z against the
/// `.rdata` constant `0x10483428` = `0.7`.
pub const MINFLOORZ: f32 = 0.7;

/// Upstream UE2 `MAXSTEPHEIGHT` in Unreal units. **Measured in XIII's `Engine.dll`** (item1j):
/// `APawn::stepUp` (`0x103baa30`) multiplies the step vector by the `.rdata` constant
/// `0x104829c4` = `35.0`. It is the fixed up-step magnitude, not a per-class default.
pub const MAXSTEPHEIGHT_UU: f32 = 35.0;

/// Movement skin in Unreal units (0.05 UU = 1 mm at 50 units/m).
pub const SKIN_UU: f32 = 0.05;
/// Walk step in Unreal units.
pub const STEP_UU: f32 = 2.5;
/// Spawn raise increment in Unreal units.
const RAISE_INC_UU: f32 = 1.0;

/// Failure group for an edge whose final blocker is a `Mover` (`Porte`/`XIIIMover`/...) at its
/// authoring pose. The engine moves the brush before traversing; the static reach harness cannot,
/// so this is a test-scope artefact, not a port bug. See [`mover_collision_sources`].
pub const MOVER_BLOCKED_CAUSE: &str =
    "mover/door closed (UE2 Mover at authoring pose; static reach harness cannot open it)";

/// One failed edge with the numbers needed to explain it.
#[derive(Debug, Clone)]
pub struct Failure {
    /// Heuristic failure cause (group key).
    pub cause: String,
    /// Start node class and path.
    pub start: String,
    /// End node class and path.
    pub end: String,
    /// Edge `reachFlags`.
    pub flags: u32,
    /// Edge collision radius (Unreal units).
    pub spec_radius: u16,
    /// Edge collision (half) height (Unreal units).
    pub spec_height: u16,
    /// Decoded cached distance (Unreal units).
    pub distance_uu: u16,
    /// Where the walk stopped (Unreal units).
    pub stop_uu: [f32; 3],
    /// Remaining horizontal distance to the end node (Unreal units).
    pub remaining_uu: f32,
    /// Goal radius (player + end node radius, Unreal units).
    pub goal_uu: f32,
    /// Steps used.
    pub steps: usize,
    /// Step budget.
    pub budget: usize,
    /// Whether the final step reported falling (no walkable floor below).
    pub falling: bool,
    /// Whether any step reported a blocking contact.
    pub blocked: bool,
    /// Blocking collision source path (last contact).
    pub blocking_source: Option<String>,
    /// Last contact normal (Bevy space).
    pub contact_normal: Option<[f32; 3]>,
    /// Vertices of the blocking triangle (Bevy space, metres), when a contact was recorded.
    pub contact_triangle: Option<[[f32; 3]; 3]>,
    /// Last contact triangle centroid height above the box bottom (Unreal units).
    pub contact_height_above_bottom_uu: Option<f32>,
    /// Floor distance below the box centre at the stop (Unreal units).
    pub floor_below_center_uu: Option<f32>,
    /// Up component of the nearest downward hit's normal at the stop (the floor probe's
    /// walkability): `>= MINFLOORZ` means walkable.
    pub floor_normal_y: Option<f32>,
    /// Ceiling distance above the box centre at the stop (Unreal units).
    pub ceiling_above_center_uu: Option<f32>,
    /// Extra note (e.g. the spawn-placement error).
    pub note: Option<String>,
}

/// Per-map result of [`analyze`].
#[derive(Debug, Clone)]
pub struct ReachReport {
    /// Map name.
    pub map: String,
    /// Navigation point count.
    pub nav_points: usize,
    /// Navigation points per class.
    pub class_counts: BTreeMap<String, usize>,
    /// Total edges.
    pub edges: usize,
    /// Edge `reachFlags` histogram.
    pub flags_hist: BTreeMap<u32, usize>,
    /// Edges with the `R_WALK` bit.
    pub walking_edges: usize,
    /// Walking edges large enough for the player.
    pub eligible: usize,
    /// Eligible edges that passed.
    pub passes: usize,
    /// Failures (any cause).
    pub failures: Vec<Failure>,
    /// Failure count per cause.
    pub groups: BTreeMap<String, usize>,
    /// Walking edges skipped because the end node was not a known navigation point.
    pub unresolved_end: usize,
    /// Navigation decode diagnostics (non-empty lines).
    pub diagnostics: Vec<String>,
    /// Player class path.
    pub player_class: String,
    /// Player class inheritance chain (most derived first).
    pub player_chain: Vec<String>,
    /// Player collision radius (Unreal units).
    pub player_radius: f32,
    /// Player collision (half) height (Unreal units).
    pub player_height: f32,
    /// Collision triangles in the built world.
    pub collision_tris: usize,
    /// Edges whose start placement failed.
    pub spawn_failures: usize,
    /// Failed edges whose last blocking triangle belongs to a UE2 `Mover` subclass (a door,
    /// lift, table, ...). Movers change collision pose at runtime; the static harness imports
    /// them at their authoring pose, so these edges are not walk-testable here (item1l).
    pub mover_blocked: usize,
    /// Walking edges excluded from `eligible` because they need a movement the harness does not
    /// model (`R_JUMP`/`R_LADDER`/... or a `Ladder` endpoint). See [`is_pure_walk`].
    pub movement_flagged: usize,
    /// Import + navigation decode time (seconds), as the app's header line reports it.
    pub import_secs: f32,
}

fn scale(a: Vec3, s: f32) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn dist_xz(a: Vec3, b: Vec3) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn heading_xz(from: Vec3, to: Vec3) -> Vec3 {
    let d = [to[0] - from[0], 0.0, to[2] - from[2]];
    let l = (d[0] * d[0] + d[2] * d[2]).sqrt();
    if l < 1e-6 {
        [0.0, 0.0, 0.0]
    } else {
        [d[0] / l, 0.0, d[2] / l]
    }
}

/// Nearest upward floor below `center`, as `(distance below, normal.y)`.
fn floor_below(world: &CollisionWorld, center: Vec3) -> Option<(f32, f32)> {
    let h = world.ray(center, [center[0], center[1] - 20.0, center[2]])?;
    Some((h.t * 20.0, h.normal[1]))
}

/// Nearest ceiling above `center`, as `(distance above, normal.y)`.
fn ceiling_above(world: &CollisionWorld, center: Vec3) -> Option<(f32, f32)> {
    let h = world.ray(center, [center[0], center[1] + 20.0, center[2]])?;
    Some((h.t * 20.0, h.normal[1]))
}

/// Result of walking one edge.
#[derive(Debug)]
struct EdgeWalk {
    position: Vec3,
    steps: usize,
    falling: bool,
    blocked: bool,
    contacts: Vec<MoveContact>,
    reached: bool,
}

/// Walks with `walk_move` from `start` toward `end`, re-aiming horizontally every step, until
/// the horizontal distance to `end` is at most `goal_radius` or the budget runs out.
#[allow(clippy::too_many_arguments)]
fn walk_edge(
    world: &CollisionWorld,
    start: Vec3,
    end: Vec3,
    half: Vec3,
    params: &WalkParams,
    step: f32,
    goal_radius: f32,
    budget: usize,
) -> EdgeWalk {
    let mut pos = start;
    let mut heading = heading_xz(start, end);
    let mut contacts = Vec::new();
    let mut falling = false;
    let mut blocked = false;
    let mut stuck = 0usize;
    let mut steps = 0usize;
    let mut reached = dist_xz(pos, end) <= goal_radius;
    while steps < budget && !reached {
        steps += 1;
        let h = heading_xz(pos, end);
        if h != [0.0, 0.0, 0.0] {
            heading = h;
        }
        let delta = scale(heading, step);
        let r = walk_move(world, pos, delta, half, params);
        falling = r.falling;
        blocked |= r.blocked;
        contacts.extend(r.contacts.iter().copied());
        let before = pos;
        pos = r.position;
        if dist_xz(pos, end) <= goal_radius {
            reached = true;
            break;
        }
        let moved = ((pos[0] - before[0]).powi(2) + (pos[2] - before[2]).powi(2)).sqrt();
        if moved < 1e-4 {
            stuck += 1;
        } else {
            stuck = 0;
        }
        if stuck >= 5 {
            break;
        }
    }
    EdgeWalk {
        position: pos,
        steps,
        falling,
        blocked,
        contacts,
        reached,
    }
}

/// Movement bits other than `R_WALK`: an edge that sets any of these requires flying, swimming,
/// jumping, a door, special movement or a ladder. The walk harness models none of them, so such
/// an edge is outside the walk test's scope (UE2 `EReachSpecFlags`, `UnPath.h`; item1l).
pub const NON_WALK_MOVEMENT: u32 = reach_flags::FLY
    | reach_flags::SWIM
    | reach_flags::JUMP
    | reach_flags::DOOR
    | reach_flags::SPECIAL
    | reach_flags::LADDER;

/// True when an edge is a plain walk: `R_WALK` set and no other movement bit. `R_FORCED`,
/// `R_PROSCRIBED` and `R_PLAYERONLY` are not movement requirements and do not disqualify it.
pub fn is_pure_walk(flags: u32) -> bool {
    flags & reach_flags::WALK != 0 && flags & NON_WALK_MOVEMENT == 0
}

/// True when a navigation-point class is a ladder (`Engine.Ladder` and subclasses). XIII does not
/// set `R_LADDER` on its ladder edges (measured: the `USA01` `Engine.Ladder` edge has
/// `reachFlags` 0x590001), so the class is the reliable signal.
pub fn node_is_ladder(class: &str) -> bool {
    class
        .rsplit('.')
        .next()
        .unwrap_or(class)
        .eq_ignore_ascii_case("ladder")
}

/// Heuristic cause for a failed stop, from the last contact normal and the clearance.
fn classify(world: &CollisionWorld, walk: &EdgeWalk, box_height: f32) -> String {
    if walk.falling {
        return "falling (no walkable floor under the stop)".to_owned();
    }
    let Some(c) = walk.contacts.last() else {
        return "no progress without contact".to_owned();
    };
    let ny = c.normal[1];
    let ceiling = ceiling_above(world, walk.position).map(|(d, _)| d);
    if ny <= -0.3 {
        return "ceiling/overhang contact".to_owned();
    }
    if ny >= MINFLOORZ {
        return "walkable ledge (near-horizontal rise not stepped)".to_owned();
    }
    if ceiling.is_some_and(|d| d < box_height) {
        return "overhang (insufficient headroom for step-up)".to_owned();
    }
    if ny.abs() < 0.3 {
        let diag = c.normal[0].abs() > 0.3 && c.normal[2].abs() > 0.3;
        return if diag {
            "diagonal wall (AABB corner vs cylinder hypothesis)".to_owned()
        } else {
            "vertical wall".to_owned()
        };
    }
    "steep surface".to_owned()
}

/// Cause for a walk that did not reach: the mover/harness artefact when the final contact is on
/// a moved brush, otherwise the geometric heuristic. Returns `(cause, on_mover)`.
fn failure_cause(
    world: &CollisionWorld,
    walk: &EdgeWalk,
    box_height: f32,
    mover_sources: &[bool],
) -> (String, bool) {
    if stopped_on_mover(&walk.contacts, mover_sources) {
        (MOVER_BLOCKED_CAUSE.to_owned(), true)
    } else {
        (classify(world, walk, box_height), false)
    }
}

/// Imports a map, decodes its navigation and walks every eligible walking edge. Prints the
/// inheritance chain and the import header line (in the order `--reach-test` always has).
pub fn analyze(map: &str, game_dir: &Path) -> Result<ReachReport, String> {
    let report = analyze_with(map, game_dir, None, None)?;
    // The app's `--reach-test` printed the chain first and the import header afterwards; keep
    // that exact ordering (both lines come from analysis, with no output in between).
    println!(
        "[collision-test] inheritance chain: {}",
        report.player_chain.join(" <- ")
    );
    println!(
        "[reach-test] {map}: imported in {:.2}s; nav points {} edges {}; collision {} triangles",
        report.import_secs, report.nav_points, report.edges, report.collision_tris
    );
    Ok(report)
}

/// Runs the reach walk, optionally reusing an already-imported scene and decoded navigation
/// (so the campaign sweep does not import/decode twice). Prints nothing.
pub fn analyze_with(
    map: &str,
    game_dir: &Path,
    scene: Option<&crate::WorldScene>,
    nav: Option<&Navigation>,
) -> Result<ReachReport, String> {
    let import_started = Instant::now();
    // A caller may already have imported the map and decoded its navigation; only do the work
    // that was not supplied. The scene is not retained beyond this function.
    let owned_scene;
    let scene = match scene {
        Some(s) => s,
        None => {
            let mut cache = PackageCache::open(game_dir)?;
            owned_scene = crate::import_map(&mut cache, map)?;
            &owned_scene
        }
    };
    let owned_nav;
    let nav = match nav {
        Some(n) => n,
        None => {
            let mut cache = PackageCache::open(game_dir)?;
            let mut defaults = ClassDefaults::open(game_dir)?;
            owned_nav = crate::navigation::decode_navigation(&mut cache, &mut defaults, map)?;
            &owned_nav
        }
    };
    let import_secs = import_started.elapsed().as_secs_f32();

    let install = Installation::open(game_dir, &OpenOptions::default())
        .map_err(|e| format!("opening installation for class defaults: {e}"))?;
    let (set, _gameinfo, player_class) = resolve_player_class(&install)?;
    let (player_chain, player_radius, player_height) = player_extents(&set, &player_class)?;
    let half = [
        player_radius / UNREAL_UNITS_PER_METER,
        player_height / UNREAL_UNITS_PER_METER,
        player_radius / UNREAL_UNITS_PER_METER,
    ];

    // `--reach-test` models the pawn's swept movement (extent queries), so it uses the box soup.
    let world = CollisionWorld::new(scene.box_collision());
    // Which collision sources belong to movable brushes (`Mover` actors): a walk stopped only by
    // one of those is not testable against static geometry (item1l).
    let mover_sources = mover_collision_sources(game_dir, map, &scene.collision_sources)?;

    let params = WalkParams {
        skin: SKIN_UU / UNREAL_UNITS_PER_METER,
        // Measured `APawn::physWalking` sub-step bound (`Engine.dll` `0x103bde14`, `cmpl $0x8`).
        max_iterations: 8,
        max_step_height: MAXSTEPHEIGHT_UU / UNREAL_UNITS_PER_METER,
        min_floor_z: MINFLOORZ,
    };
    let step = STEP_UU / UNREAL_UNITS_PER_METER;

    let mut report = ReachReport {
        map: map.to_owned(),
        nav_points: nav.points.len(),
        class_counts: nav.class_counts.clone(),
        edges: nav.edges.len(),
        flags_hist: nav.flags_hist.clone(),
        walking_edges: 0,
        eligible: 0,
        passes: 0,
        failures: Vec::new(),
        groups: BTreeMap::new(),
        unresolved_end: 0,
        diagnostics: nav_diagnostics(nav),
        player_class,
        player_chain,
        player_radius,
        player_height,
        collision_tris: world.triangle_count(),
        spawn_failures: 0,
        mover_blocked: 0,
        movement_flagged: 0,
        import_secs,
    };

    for edge in &nav.edges {
        if !edge.is_walk() {
            continue;
        }
        report.walking_edges += 1;
        // `R_JUMP`/`R_LADDER`/... edges are only traversable with that movement; the walk harness
        // has no jump/climb, so they are outside its scope (item1l). Ladder edges are also
        // rejected by their endpoint class because XIII does not set `R_LADDER` on `Engine.Ladder`
        // edges (measured: the one `Ladder` edge has `reachFlags` 0x590001).
        if !is_pure_walk(edge.reach_flags) {
            report.movement_flagged += 1;
            continue;
        }
        if f32::from(edge.collision_radius) < player_radius
            || f32::from(edge.collision_height) < player_height
        {
            continue;
        }
        let Some(end_idx) = edge.end_point else {
            report.unresolved_end += 1;
            continue;
        };
        let start_idx = edge.start_point.unwrap_or(edge.owner);
        let start_pt = &nav.points[start_idx];
        let end_pt = &nav.points[end_idx];
        if node_is_ladder(&start_pt.class) || node_is_ladder(&end_pt.class) {
            report.movement_flagged += 1;
            continue;
        }
        report.eligible += 1;

        let start = to_bevy_position(start_pt.location);
        let end = to_bevy_position(end_pt.location);

        let end_radius = if end_pt.collision_radius.is_finite() {
            end_pt.collision_radius.max(0.0)
        } else {
            0.0
        };
        let goal_radius = (player_radius + end_radius) / UNREAL_UNITS_PER_METER;
        let edge_len_uu = if edge.distance > 0 {
            f32::from(edge.distance)
        } else {
            dist_xz(start, end) * UNREAL_UNITS_PER_METER
        };
        let budget = (((3.0 * edge_len_uu) / STEP_UU).ceil() as usize).max(4);

        // The NavigationPoint `Location` is the centre of its collision cylinder (measured:
        // `Location.Z - floor - CollisionHeight` clusters on 0; see `--collision-test`), so the
        // player box centre starts at the node and the FindSpot-style raise+drop follows.
        let spawn = match place_spawn(&world, [start[0], start[1] - half[1], start[2]], half) {
            Ok(s) => s,
            Err(e) => {
                report.spawn_failures += 1;
                let cause = if e.contains("no floor") {
                    "missing floor under start node (no downward hit within the 3 m drop)"
                        .to_owned()
                } else if e.contains("still overlaps") {
                    "spawn overlap (start node embedded in collision)".to_owned()
                } else {
                    "spawn placement failed".to_owned()
                };
                *report.groups.entry(cause.clone()).or_default() += 1;
                report.failures.push(Failure {
                    cause,
                    start: format!("{} {}", start_pt.class, start_pt.path),
                    end: format!("{} {}", end_pt.class, end_pt.path),
                    flags: edge.reach_flags,
                    spec_radius: edge.collision_radius,
                    spec_height: edge.collision_height,
                    distance_uu: edge.distance,
                    stop_uu: [
                        start[0] * UNREAL_UNITS_PER_METER,
                        start[1] * UNREAL_UNITS_PER_METER,
                        start[2] * UNREAL_UNITS_PER_METER,
                    ],
                    remaining_uu: dist_xz(start, end) * UNREAL_UNITS_PER_METER,
                    goal_uu: goal_radius * UNREAL_UNITS_PER_METER,
                    steps: 0,
                    budget,
                    falling: false,
                    blocked: false,
                    blocking_source: None,
                    contact_normal: None,
                    contact_triangle: None,
                    contact_height_above_bottom_uu: None,
                    floor_below_center_uu: floor_below(&world, start)
                        .map(|(d, _)| d * UNREAL_UNITS_PER_METER),
                    floor_normal_y: floor_below(&world, start).map(|(_, n)| n),
                    ceiling_above_center_uu: ceiling_above(&world, start)
                        .map(|(d, _)| d * UNREAL_UNITS_PER_METER),
                    note: Some(e),
                });
                continue;
            }
        };

        let walk = walk_edge(
            &world,
            spawn.position,
            end,
            half,
            &params,
            step,
            goal_radius,
            budget,
        );
        if walk.reached {
            report.passes += 1;
            continue;
        }

        // A walk stopped by a `Mover` (door/lift/table/...) is a static-harness artefact, not a
        // port bug: UE2 moves the brush (`PHYS_MovingBrush`) and the navigation network is
        // traversed with the mover at its moved pose. Only the final blocker is inspected: if the
        // walk had failed on static geometry first, that would be the last contact.
        let (cause, on_mover) = failure_cause(&world, &walk, 2.0 * half[1], &mover_sources);
        if on_mover {
            report.mover_blocked += 1;
        }
        *report.groups.entry(cause.clone()).or_default() += 1;
        let remaining_uu = dist_xz(walk.position, end) * UNREAL_UNITS_PER_METER;
        let blocking_source = walk
            .contacts
            .last()
            .map(|c| scene.collision_sources[c.source as usize].clone());
        let contact_normal = walk.contacts.last().map(|c| c.normal);
        let contact_triangle = walk.contacts.last().map(|c| *world.triangle(c.triangle));
        let contact_height_above_bottom_uu = walk.contacts.last().map(|c| {
            let tri = world.triangle(c.triangle);
            let centroid_y = (tri[0][1] + tri[1][1] + tri[2][1]) / 3.0;
            (centroid_y - (c.position[1] - half[1])) * UNREAL_UNITS_PER_METER
        });
        let floor_below_center_uu =
            floor_below(&world, walk.position).map(|(d, _)| d * UNREAL_UNITS_PER_METER);
        let floor_normal_y = floor_below(&world, walk.position).map(|(_, n)| n);
        let ceiling_above_center_uu =
            ceiling_above(&world, walk.position).map(|(d, _)| d * UNREAL_UNITS_PER_METER);
        report.failures.push(Failure {
            cause,
            start: format!("{} {}", start_pt.class, start_pt.path),
            end: format!("{} {}", end_pt.class, end_pt.path),
            flags: edge.reach_flags,
            spec_radius: edge.collision_radius,
            spec_height: edge.collision_height,
            distance_uu: edge.distance,
            stop_uu: [
                walk.position[0] * UNREAL_UNITS_PER_METER,
                walk.position[1] * UNREAL_UNITS_PER_METER,
                walk.position[2] * UNREAL_UNITS_PER_METER,
            ],
            remaining_uu,
            goal_uu: goal_radius * UNREAL_UNITS_PER_METER,
            steps: walk.steps,
            budget,
            falling: walk.falling,
            blocked: walk.blocked,
            blocking_source,
            contact_normal,
            contact_triangle,
            contact_height_above_bottom_uu,
            floor_below_center_uu,
            floor_normal_y,
            ceiling_above_center_uu,
            note: None,
        });
    }

    Ok(report)
}

/// Non-empty decode diagnostics of a navigation network.
/// Non-empty decode diagnostics of a navigation network.
pub fn nav_diagnostics(nav: &Navigation) -> Vec<String> {
    let mut out = Vec::new();
    if !nav.unresolved_class.is_empty() {
        out.push(format!(
            "{} navigation-looking exports had an unresolvable class (first: {:?})",
            nav.unresolved_class.len(),
            nav.unresolved_class.first()
        ));
    }
    if !nav.property_failures.is_empty() {
        out.push(format!(
            "{} navigation exports failed property decode (first: {:?})",
            nav.property_failures.len(),
            nav.property_failures.first()
        ));
    }
    if !nav.tail_failures.is_empty() {
        out.push(format!(
            "{} navigation exports failed PathList decode (first: {:?})",
            nav.tail_failures.len(),
            nav.tail_failures.first()
        ));
    }
    if nav.empty_path_lists > 0 {
        out.push(format!(
            "{} navigation exports have an empty native tail (no PathList)",
            nav.empty_path_lists
        ));
    }
    if nav.null_start_edges > 0 {
        out.push(format!(
            "{} edges have a null decoded Start (owner used as start)",
            nav.null_start_edges
        ));
    }
    if nav.start_mismatch_edges > 0 {
        out.push(format!(
            "{} edges have a Start different from the owning node",
            nav.start_mismatch_edges
        ));
    }
    if nav.import_end_edges > 0 {
        out.push(format!(
            "{} edges have an End that is an import, not a map instance",
            nav.import_end_edges
        ));
    }
    if nav.unresolved_end_edges > 0 {
        out.push(format!(
            "{} edges have an End export that is not a decoded navigation point",
            nav.unresolved_end_edges
        ));
    }
    out
}

/// One `bool` per collision source (index into `scene.collision_sources`): true when the source
/// belongs to an actor whose class chain contains `Mover` (`Porte`/`XIIIPorte`, `XIIIMover`,
/// `XIIIMovable`/`Movable`, `BreakableMover`, `LiftDoor`, ...). Evidence: the decoded script
/// classes (`XIIIPorte super XIIIMover`, `XIIIMover super Mover`, `BreakableMover super
/// XIIIMover`, ...; `xiii-tool script classes`) and `Engine.Mover`'s `PHYS_MovingBrush` physics.
///
/// Collision sources are `"{actor object path} -> {mesh label}"` (static-mesh actors), so the
/// actor path prefix is matched against the map's `Mover`-subclass exports. Classes whose chain
/// cannot be resolved (native-only classes) are skipped, not treated as movers.
fn mover_collision_sources(
    game_dir: &Path,
    map: &str,
    sources: &[String],
) -> Result<Vec<bool>, String> {
    let mut cache = PackageCache::open(game_dir)?;
    let map_pkg = cache.map(map)?;
    let mut defaults = ClassDefaults::open(game_dir)?;
    let p = &map_pkg.package;
    let mut movers: std::collections::HashSet<String> = std::collections::HashSet::new();
    for i in 0..p.exports().len() {
        let Some(class) = p.export_class_path(i) else {
            continue;
        };
        let Ok(chain) = defaults.class_chain(class) else {
            continue;
        };
        if chain.iter().any(|c| c == "mover")
            && let Some(path) = p.object_path(xiii_package::ObjectRef::Export(i as u32))
        {
            movers.insert(path.to_owned());
        }
    }
    Ok(sources
        .iter()
        .map(|s| collision_source_actor(s).is_some_and(|actor| movers.contains(actor)))
        .collect())
}

/// Actor path of a static-mesh collision source (`"{actor} -> {mesh label}"`); `None` for BSP
/// (`"ModelN (BSP)"`) and terrain (`"TerrainInfoN (terrain)"`) sources.
fn collision_source_actor(source: &str) -> Option<&str> {
    source.split_once(" -> ").map(|(actor, _)| actor)
}

/// True when the walk's **final** contact is on a mover collision source. Only the last contact
/// matters: if the walk had first failed on static geometry, that would be the recorded stop.
fn stopped_on_mover(contacts: &[MoveContact], mover_sources: &[bool]) -> bool {
    contacts.last().is_some_and(|c| {
        mover_sources
            .get(c.source as usize)
            .copied()
            .unwrap_or(false)
    })
}

// -------------------------------------------------------------------------------------------
// Player class resolution and spawn placement (moved from `xiii-app/src/collision.rs` so the
// Bevy-free tool can run the same walk). Behaviour is unchanged.
// -------------------------------------------------------------------------------------------

/// Result of a spawn placement.
#[derive(Debug)]
pub struct Spawn {
    /// Final box center (Bevy metres).
    pub position: Vec3,
    /// Floor height under the box.
    pub floor: f32,
    /// Vertical raise applied to clear an initial overlap (metres).
    pub raise: f32,
}

/// Places the player extent box at the navigation node with the ported **`ULevel::FindSpot`**
/// (`xiii_collision::find_spot`, `Engine.dll` `0x1038a080`; item1k). If the box overlaps at the
/// node it searches the engine's four corner offsets `(±0.5*Extent.X, ±0.5*Extent.Y)`,
/// extrapolates a single free candidate, and then settles onto a walkable floor. The previous
/// vertical raise (1 UU increments, cap `2*H`) is retained as the explicit embedded-box fallback
/// when the corner search finds nothing.
///
/// `player_start` is the desired box **bottom** (the reach caller places the node there); the
/// engine's `Location` is the box centre, so half the height is added.
pub fn place_spawn(
    world: &CollisionWorld,
    player_start: Vec3,
    half: Vec3,
) -> Result<Spawn, String> {
    let desired_center = [player_start[0], player_start[1] + half[1], player_start[2]];
    let params = xiii_collision::FindSpotParams {
        min_floor_z: MINFLOORZ,
        drop: 3.0,
        raise_step: RAISE_INC_UU / UNREAL_UNITS_PER_METER,
        max_raise: 2.0 * half[1],
    };
    let spot = xiii_collision::find_spot(world, desired_center, half, &params)
        .map_err(|e| e.to_string())?;
    Ok(Spawn {
        position: spot.position,
        floor: spot.floor,
        raise: spot.position[1] - desired_center[1],
    })
}

/// Loads every `.u` package of the installation into a `ScriptSet` (read-only).
fn load_script_set(install: &Installation) -> Result<ScriptSet, String> {
    let mut set = ScriptSet::new();
    for entry in install.packages() {
        if entry.kind != PackageKind::Code {
            continue;
        }
        let data =
            std::fs::read(&entry.path).map_err(|e| format!("{}: {e}", entry.path.display()))?;
        let p = ScriptPackage::load(
            &entry.name,
            data,
            &ScriptLimits::default(),
            &Limits::default(),
        )
        .map_err(|e| format!("{}: {e}", entry.path.display()))?;
        set.add(p);
    }
    Ok(set)
}

/// Finds the GameInfo class from `Default.ini` `DefaultGame=` and the pawn class from its
/// `DefaultPlayerClassName` default. Fails loudly when any step is missing.
pub fn resolve_player_class(install: &Installation) -> Result<(ScriptSet, String, String), String> {
    let gameinfo_path = {
        let mut found = None;
        for ev in install.ini_evidence() {
            if !ev.file.to_ascii_lowercase().ends_with("default.ini") {
                continue;
            }
            let text = std::fs::read_to_string(install.root().join(&ev.file))
                .map_err(|e| format!("{}: {e}", ev.file))?;
            if let Some(v) = parse_key(&text, "DefaultGame") {
                found = Some(v);
                break;
            }
        }
        found.ok_or_else(|| {
            "Default.ini has no DefaultGame= key; cannot resolve the player class".to_owned()
        })?
    };
    let (gi_pkg, _gi_class) = gameinfo_path
        .split_once('.')
        .ok_or_else(|| format!("DefaultGame {gameinfo_path:?} is not Package.Class"))?;
    let set = load_script_set(install)?;
    let pawn_path = default_player_class(&set, gi_pkg, &gameinfo_path)?;
    Ok((set, gameinfo_path, pawn_path))
}

/// Reads the `DefaultPlayerClassName` string from a GameInfo class's own default block.
fn default_player_class(
    set: &ScriptSet,
    package: &str,
    gameinfo_path: &str,
) -> Result<String, String> {
    let pi = set
        .package_index(package)
        .ok_or_else(|| format!("package {package} not loaded"))?;
    let p = &set.packages[pi];
    let class_path = gameinfo_path
        .split_once('.')
        .map(|(_, c)| c)
        .unwrap_or(gameinfo_path);
    let e = p
        .export_by_path(class_path)
        .ok_or_else(|| format!("class {gameinfo_path} not found in {package}"))?;
    let Some(ScriptObject::Class(cl)) = p.objects.get(&e) else {
        return Err(format!("{gameinfo_path} is not a decoded class"));
    };
    for prop in &cl.defaults.properties {
        if !p
            .package
            .property_name(prop)
            .eq_ignore_ascii_case("DefaultPlayerClassName")
        {
            continue;
        }
        return match &prop.value {
            PropertyValue::Str(s) => Ok(s.clone()),
            PropertyValue::Name(n) => Ok(p.package.name(*n).to_owned()),
            other => Err(format!(
                "DefaultPlayerClassName has unexpected type {other:?}"
            )),
        };
    }
    Err(format!(
        "{gameinfo_path} has no DefaultPlayerClassName (no silent fallback)"
    ))
}

/// `key=value` lookup (case-insensitive key), first occurrence, comments ignored.
fn parse_key(text: &str, key: &str) -> Option<String> {
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if let Some((k, v)) = line.split_once('=')
            && k.trim().eq_ignore_ascii_case(key)
        {
            return Some(v.trim().to_owned());
        }
    }
    None
}

/// A resolved class layout (`Vm::class_layout`): the class chain's slots and inherited
/// defaults, root-to-leaf.
type Layout = std::rc::Rc<xiii_script::vm::ClassLayout>;

/// Resolves the inherited default layout of `Package.Class` via `Vm::class_layout`.
fn class_layout_of(
    set: &ScriptSet,
    class_path: &str,
    default_package: Option<&str>,
) -> Result<Layout, String> {
    let (pkg_name, class_name) = match class_path.split_once('.') {
        Some((p, c)) => (p, c),
        None => (
            default_package.ok_or_else(|| format!("class {class_path:?} is not Package.Class"))?,
            class_path,
        ),
    };
    let pi = set
        .package_index(pkg_name)
        .ok_or_else(|| format!("package {pkg_name} not loaded"))?;
    let p = &set.packages[pi];
    let e = p
        .export_by_path(class_name)
        .ok_or_else(|| format!("class {class_path} not found in {pkg_name}"))?;
    let class = xiii_script::GlobalRef {
        package: pi,
        export: e,
    };
    let mut vm = Vm::new(set, VmLimits::default());
    vm.class_layout(class)
        .map_err(|err| format!("class layout of {class_path}: {err}"))
}

/// Resolved float default (first array element) of a class layout.
fn layout_float(layout: &Layout, name: &str) -> Result<f32, String> {
    let class = layout
        .chain_names
        .first()
        .cloned()
        .unwrap_or_else(|| "?".into());
    let slot = layout
        .slot_by_name(name)
        .ok_or_else(|| format!("class {class} has no property {name}"))?;
    match layout.defaults.get(slot.base) {
        Some(Value::Float(v)) => Ok(*v),
        other => Err(format!("class {class}.{name} is {other:?}, expected float")),
    }
}

/// Resolved float default or `NaN` when absent.
fn layout_float_opt(layout: &Layout, name: &str) -> f32 {
    layout_float(layout, name).unwrap_or(f32::NAN)
}

/// Resolved inherited collision defaults of the player pawn class, plus its inheritance chain.
pub fn player_extents(set: &ScriptSet, pawn_path: &str) -> Result<(Vec<String>, f32, f32), String> {
    let layout = class_layout_of(set, pawn_path, None)?;
    let _ = layout_float_opt(&layout, "BaseEyeHeight");
    let _ = layout_float_opt(&layout, "GroundSpeed");
    let _ = layout_float_opt(&layout, "JumpZ");
    Ok((
        layout.chain_names.clone(),
        layout_float(&layout, "CollisionRadius")?,
        layout_float(&layout, "CollisionHeight")?,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Resolves `XIII_GOG_DIR` against the workspace root, like the other opt-in tests.
    fn opt_in_game_dir() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        let ws = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    fn walk_params() -> WalkParams {
        WalkParams {
            skin: SKIN_UU / UNREAL_UNITS_PER_METER,
            max_iterations: 4,
            max_step_height: MAXSTEPHEIGHT_UU / UNREAL_UNITS_PER_METER,
            min_floor_z: MINFLOORZ,
        }
    }

    /// A 20x20 m floor at Bevy Y=0.
    fn floor_world() -> CollisionWorld {
        CollisionWorld::new(vec![
            (
                [[-10.0, 0.0, -10.0], [10.0, 0.0, -10.0], [10.0, 0.0, 10.0]],
                0u32,
            ),
            (
                [[-10.0, 0.0, -10.0], [10.0, 0.0, 10.0], [-10.0, 0.0, 10.0]],
                0u32,
            ),
        ])
    }

    /// The floor plus a 3 m wall at Bevy X=2.
    fn wall_world() -> CollisionWorld {
        CollisionWorld::new(vec![
            (
                [[-10.0, 0.0, -10.0], [10.0, 0.0, -10.0], [10.0, 0.0, 10.0]],
                0u32,
            ),
            (
                [[-10.0, 0.0, -10.0], [10.0, 0.0, 10.0], [-10.0, 0.0, 10.0]],
                0u32,
            ),
            ([[2.0, 0.0, -1.0], [2.0, 3.0, -1.0], [2.0, 3.0, 1.0]], 1u32),
            ([[2.0, 0.0, -1.0], [2.0, 3.0, 1.0], [2.0, 0.0, 1.0]], 1u32),
        ])
    }

    /// A floor only for Bevy X <= 0 (a ledge edge at X=0).
    fn ledge_world() -> CollisionWorld {
        CollisionWorld::new(vec![
            (
                [[-10.0, 0.0, -10.0], [0.0, 0.0, -10.0], [0.0, 0.0, 10.0]],
                0u32,
            ),
            (
                [[-10.0, 0.0, -10.0], [0.0, 0.0, 10.0], [-10.0, 0.0, 10.0]],
                0u32,
            ),
        ])
    }

    #[test]
    fn walk_edge_reaches_a_clear_target_on_a_floor() {
        let world = floor_world();
        let half = [
            34.0 / UNREAL_UNITS_PER_METER,
            75.0 / UNREAL_UNITS_PER_METER,
            34.0 / UNREAL_UNITS_PER_METER,
        ];
        let start = [0.0, half[1], 0.0];
        let end = [5.0, half[1], 0.0];
        let w = walk_edge(
            &world,
            start,
            end,
            half,
            &walk_params(),
            STEP_UU / UNREAL_UNITS_PER_METER,
            0.4,
            400,
        );
        assert!(
            w.reached,
            "not reached, remaining {}",
            dist_xz(w.position, end)
        );
    }

    #[test]
    fn walk_edge_wall_is_classified_as_vertical_wall() {
        let world = wall_world();
        let half = [
            34.0 / UNREAL_UNITS_PER_METER,
            75.0 / UNREAL_UNITS_PER_METER,
            34.0 / UNREAL_UNITS_PER_METER,
        ];
        let start = [0.0, half[1], 0.0];
        let end = [5.0, half[1], 0.0];
        let w = walk_edge(
            &world,
            start,
            end,
            half,
            &walk_params(),
            STEP_UU / UNREAL_UNITS_PER_METER,
            0.4,
            400,
        );
        assert!(!w.reached, "walked through the wall to {:?}", w.position);
        assert_eq!(classify(&world, &w, 2.0 * half[1]), "vertical wall");
    }

    #[test]
    fn walk_edge_below_a_gap_reports_falling() {
        let world = ledge_world();
        let half = [
            34.0 / UNREAL_UNITS_PER_METER,
            75.0 / UNREAL_UNITS_PER_METER,
            34.0 / UNREAL_UNITS_PER_METER,
        ];
        // Start fully past the ledge edge (box left face beyond X=0); the budget is too small
        // to reach the far end.
        let start = [0.5, half[1], 0.0];
        let end = [5.0, half[1], 0.0];
        let w = walk_edge(
            &world,
            start,
            end,
            half,
            &walk_params(),
            STEP_UU / UNREAL_UNITS_PER_METER,
            0.4,
            1,
        );
        assert!(!w.reached);
        assert!(w.falling, "expected falling, got {:?}", w);
        assert_eq!(
            classify(&world, &w, 2.0 * half[1]),
            "falling (no walkable floor under the stop)"
        );
    }

    #[test]
    fn edge_step_budget_is_three_times_the_length() {
        // 300 UU with a 2.5 UU step: 3*300/2.5 = 360 steps.
        let edge_len_uu = 300.0f32;
        let budget = (((3.0 * edge_len_uu) / STEP_UU).ceil() as usize).max(4);
        assert_eq!(budget, 360);
    }

    /// `place_spawn` must fail loudly (not silently) when there is no floor to drop onto.
    #[test]
    fn place_spawn_without_floor_is_an_error() {
        // An empty world has no floor under the PlayerStart.
        let world = CollisionWorld::new(Vec::<(xiii_collision::Triangle, u32)>::new());
        let half = [
            34.0 / UNREAL_UNITS_PER_METER,
            75.0 / UNREAL_UNITS_PER_METER,
            34.0 / UNREAL_UNITS_PER_METER,
        ];
        let err = place_spawn(&world, [0.0, 0.0, 0.0], half).unwrap_err();
        assert!(err.contains("no floor"), "{err}");
    }

    /// Regression guard: measured pass counts on the opening maps. `UseSimpleBoxCollision`
    /// defaults to true, so `bankesca2`'s staircase is in the box soup. A drop below either
    /// measured value is a regression. `mover-blocked` is the item1l count of edges whose only
    /// blocker is a `Mover` subclass (door/lift); it is a classification guard, not a target.
    #[test]
    fn opt_in_reach_regression_plage00_plage01_banque01() {
        let Some(path) = opt_in_game_dir() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        for (map, min_pass, want_missing_floor, want_mover, want_movement) in [
            ("Plage00", 12usize, 0usize, 0usize, 0usize),
            // Plage01 mover-blocked 6 (was 5 pre-item27k): the engine's measured MoveActor
            // back-off discard (Engine.dll 0x1038a89a-0x1038ac05) lets the walk leave a
            // start-penetrating static contact, so one doorpoint edge now advances until the
            // real closed lunch-room door (Porte5/6/7 set) and classifies as mover-blocked.
            // Passes rose to 296; no edge regressed.
            ("Plage01", 294usize, 0usize, 6usize, 0usize),
            ("Banque01", 631usize, 0usize, 20usize, 0usize),
        ] {
            let report = analyze(map, &path).expect("reach analyze");
            let missing_floor: usize = report
                .groups
                .iter()
                .filter(|(cause, _)| cause.starts_with("missing floor"))
                .map(|(_, n)| *n)
                .sum();
            println!(
                "[reach-test regression] {map}: eligible {} passes {} (>= {min_pass}), missing-floor {missing_floor} (== {want_missing_floor}), mover-blocked {} (== {want_mover}), movement-flagged {} (== {want_movement})",
                report.eligible, report.passes, report.mover_blocked, report.movement_flagged
            );
            assert!(
                report.passes >= min_pass,
                "{map}: pass count {} below measured {min_pass}",
                report.passes
            );
            assert_eq!(
                missing_floor, want_missing_floor,
                "{map}: missing-floor start nodes changed: {:?}",
                report.groups
            );
            assert_eq!(
                report.mover_blocked, want_mover,
                "{map}: mover/door-blocked count changed: {:?}",
                report.groups
            );
            assert_eq!(
                report.movement_flagged, want_movement,
                "{map}: movement-flagged edge count changed: {}",
                report.movement_flagged
            );
        }
    }

    /// Regression guard for the maps whose terrain payloads store extra editor vertices after
    /// the base heightfield grid (the base grid is `HeightmapX * HeightmapY`; see
    /// `xiii_decode::terrain::TerrainInfo::mesh`). Only the base grid is the engine's terrain
    /// (`ATerrainInfo::LineCheck`/`Render` index it; `Engine.dll` 0x10409eb0/0x1040c0c0), so the
    /// importer now imports only region 0. Importing the trailing vertices as detail geometry
    /// had regressed Hual01b 729 -> 668 (item1h); with the base-only import Hual01b is 726.
    /// A drop below the measured pass count, or an increase in missing-floor start nodes above
    /// the measured value, is a regression. Values measured 2026-10-05 on the GOG corpus after
    /// item1k ported `ULevel::FindSpot` (reach 22353 -> 22504, walkable-ledge 136 -> 35). The
    /// remaining missing-floor nodes are terrain slopes below `MINFLOORZ` 0.7 under the chosen
    /// spot; item1k's report lists them. The values are measured, not targets. item1l excludes
    /// `R_JUMP`/`Ladder` edges from `eligible` (Hual01b: 8 edges, 7 of them previously passing),
    /// so Hual01b's measured values changed 816/760 -> 808/753; the other three maps are
    /// unchanged.
    #[test]
    fn opt_in_reach_regression_multi_region_terrains() {
        let Some(path) = opt_in_game_dir() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        for (map, min_pass, eligible, max_missing_floor, want_mover, want_movement) in [
            // Hual01b mover-blocked 3 (was 2 pre-item27k) and Hual04c min-pass 434 (was 435):
            // the engine's measured MoveActor back-off discard (Engine.dll 0x1038a89a-0x1038ac05,
            // hits within 2 UU behind the start do not stop the move) changes walks whose spawn
            // node is embedded in collision: they leave the contact instead of step-up-climbing
            // over it. On Hual01b one more embedded-spawn edge now reaches the real closed door
            // (mover-blocked); on Hual04c the straight-line walk out of TelepheriquePoint3
            // (embedded in the TLcabine_cassee cabin mesh) no longer step-up-climbs the cabin
            // and stops short/falls instead — the harness walks straight lines, not engine AI
            // steering, and the discarded contact is what a real MoveActor does.
            ("Hual01b", 753usize, 808usize, 3usize, 3usize, 8usize),
            ("Hual04c", 434usize, 437usize, 0usize, 0usize, 0usize),
            ("Kello01a", 1459usize, 1488usize, 15usize, 0usize, 0usize),
            ("PRock04a", 702usize, 721usize, 16usize, 0usize, 0usize),
        ] {
            let report = analyze(map, &path).expect("reach analyze");
            let missing_floor: usize = report
                .groups
                .iter()
                .filter(|(cause, _)| cause.starts_with("missing floor"))
                .map(|(_, n)| *n)
                .sum();
            println!(
                "[reach-test regression] {map}: eligible {} passes {} (>= {min_pass}), missing-floor {missing_floor} (<= {max_missing_floor}), mover-blocked {} (== {want_mover}), movement-flagged {} (== {want_movement})",
                report.eligible, report.passes, report.mover_blocked, report.movement_flagged
            );
            assert_eq!(
                report.eligible, eligible,
                "{map}: eligible edge count changed from the measured {eligible}"
            );
            assert!(
                report.passes >= min_pass,
                "{map}: pass count {} below measured {min_pass}",
                report.passes
            );
            assert!(
                missing_floor <= max_missing_floor,
                "{map}: missing-floor start nodes increased above the measured {max_missing_floor}: {:?}",
                report.groups
            );
            assert_eq!(
                report.mover_blocked, want_mover,
                "{map}: mover/door-blocked count changed: {:?}",
                report.groups
            );
            assert_eq!(
                report.movement_flagged, want_movement,
                "{map}: movement-flagged edge count changed: {}",
                report.movement_flagged
            );
        }
    }

    /// Diagnostic dump for the item1l residual analysis: prints every reach failure of the maps
    /// named in `XIII_DUMP_MAPS` (comma-separated) with the blocking primitive, surface normal,
    /// triangle vertices and collision source. Opt-in: prints `SKIPPED` without the env var.
    /// Never runs in the default suite, so it does not read game data unless asked.
    #[test]
    fn opt_in_dump_reach_failures() {
        let Some(path) = opt_in_game_dir() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let Some(maps) = std::env::var_os("XIII_DUMP_MAPS") else {
            println!("SKIPPED: set XIII_DUMP_MAPS=a,b,.. to dump reach failures");
            return;
        };
        let maps = maps.to_string_lossy().into_owned();
        for map in maps.split(',').map(str::trim).filter(|s| !s.is_empty()) {
            let report = analyze_with(map, &path, None, None).expect("reach analyze");
            println!(
                "DUMP {map}: eligible {} passes {} mover_blocked {} movement_flagged {} groups {:?}",
                report.eligible,
                report.passes,
                report.mover_blocked,
                report.movement_flagged,
                report.groups
            );
            for f in &report.failures {
                let names = reach_flags::names(f.flags).join("|");
                let tri = f.contact_triangle.map_or_else(
                    || "-".to_owned(),
                    |t| {
                        t.iter()
                            .map(|v| {
                                format!(
                                    "({:.1},{:.1},{:.1})",
                                    v[0] * UNREAL_UNITS_PER_METER,
                                    v[1] * UNREAL_UNITS_PER_METER,
                                    v[2] * UNREAL_UNITS_PER_METER
                                )
                            })
                            .collect::<Vec<_>>()
                            .join(" ")
                    },
                );
                let n = f.contact_normal.map_or_else(
                    || "-".to_owned(),
                    |n| format!("({:.3},{:.3},{:.3})", n[0], n[1], n[2]),
                );
                println!(
                    "  [{}] {} -> {} flags=0x{:x}({}) specR/H={}/{} d={} stop=({:.1},{:.1},{:.1}) rem={:.1} steps={}/{} fall={} block={} src={} n={} tri={} ch={:?} floor={:?} fn={:?} ceil={:?} note={:?}",
                    f.cause,
                    f.start,
                    f.end,
                    f.flags,
                    names,
                    f.spec_radius,
                    f.spec_height,
                    f.distance_uu,
                    f.stop_uu[0],
                    f.stop_uu[1],
                    f.stop_uu[2],
                    f.remaining_uu,
                    f.steps,
                    f.budget,
                    f.falling,
                    f.blocked,
                    f.blocking_source.as_deref().unwrap_or("-"),
                    n,
                    tri,
                    f.contact_height_above_bottom_uu,
                    f.floor_below_center_uu,
                    f.floor_normal_y,
                    f.ceiling_above_center_uu,
                    f.note.as_deref().unwrap_or("-"),
                );
            }
        }
    }

    /// `collision_source_actor` extracts the actor path from a static-mesh collision source and
    /// rejects BSP/terrain sources (which have no `" -> "`).
    #[test]
    fn collision_source_actor_shape() {
        assert_eq!(
            collision_source_actor("Porte6 -> StaticPlage2.Pl_porte01T"),
            Some("Porte6")
        );
        assert_eq!(
            collision_source_actor("StaticMeshActor1 -> Staticbanque.hall"),
            Some("StaticMeshActor1")
        );
        assert_eq!(collision_source_actor("Model69 (BSP)"), None);
        assert_eq!(collision_source_actor("TerrainInfo0 (terrain)"), None);
    }

    /// `R_WALK` alone is a pure walk; any other movement bit (`R_JUMP`, `R_LADDER`, ...) makes the
    /// edge untestable by the harness. The unknown high bits XIII stores on its `Ladder` edge
    /// (0x590001) do **not** set a movement bit, so the ladder class check is the signal for it.
    #[test]
    fn pure_walk_requires_no_other_movement() {
        assert!(is_pure_walk(reach_flags::WALK));
        assert!(is_pure_walk(
            reach_flags::WALK | reach_flags::FORCED | reach_flags::PROSCRIBED
        ));
        assert!(!is_pure_walk(reach_flags::JUMP));
        assert!(!is_pure_walk(reach_flags::WALK | reach_flags::JUMP));
        assert!(!is_pure_walk(reach_flags::WALK | reach_flags::LADDER));
        assert!(!is_pure_walk(reach_flags::WALK | reach_flags::DOOR));
        assert!(!is_pure_walk(reach_flags::WALK | reach_flags::SWIM));
        assert!(is_pure_walk(0x590001), "high unknown bits are not movement");
        assert!(node_is_ladder("Engine.Ladder"));
        assert!(node_is_ladder("XIDPawn.ladder"));
        assert!(!node_is_ladder("Engine.PathNode"));
        assert!(!node_is_ladder("XIDPawn.doorpoint"));
    }

    /// A walk whose final contact is on a `Mover` source is classified as the mover/harness
    /// artefact (and counted), never as the geometric cause it would otherwise get: a moved
    /// brush is not a static wall.
    #[test]
    fn stopped_on_mover_selects_the_mover_cause() {
        let contact = |source: u32| MoveContact {
            source,
            triangle: 0,
            t: 0.5,
            normal: [1.0, 0.0, 0.0],
            height: 0.0,
            position: [0.0, 0.0, 0.0],
        };
        // A vertical wall (x-facing) that the geometric classifier would call "vertical wall".
        let world = wall_world();
        let edge = |contacts: Vec<MoveContact>| EdgeWalk {
            position: [0.0, 1.0, 0.0],
            steps: 5,
            falling: false,
            blocked: true,
            contacts,
            reached: false,
        };
        let (cause, on_mover) =
            failure_cause(&world, &edge(vec![contact(1)]), 2.0, &[false, false]);
        assert!(!on_mover, "a static contact must not be a mover case");
        assert_eq!(cause, "vertical wall");
        let (cause, on_mover) = failure_cause(&world, &edge(vec![contact(1)]), 2.0, &[false, true]);
        assert!(
            on_mover,
            "a final contact on source 1 (a mover) is the mover case"
        );
        assert_eq!(cause, MOVER_BLOCKED_CAUSE);
        // A static final contact stays a wall even when an earlier contact touched a mover.
        let (cause, on_mover) = failure_cause(
            &world,
            &edge(vec![contact(1), contact(0)]),
            2.0,
            &[false, true],
        );
        assert!(!on_mover, "only the final contact decides");
        assert_eq!(cause, "vertical wall");
        // An out-of-range source cannot be a mover; no contact means no mover attribution.
        assert!(!stopped_on_mover(&[contact(9)], &[false, true]));
        assert!(!stopped_on_mover(&[], &[false, true]));
    }
}
