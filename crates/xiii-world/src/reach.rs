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
    /// Last contact triangle centroid height above the box bottom (Unreal units).
    pub contact_height_above_bottom_uu: Option<f32>,
    /// Floor distance below the box centre at the stop (Unreal units).
    pub floor_below_center_uu: Option<f32>,
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
        import_secs,
    };

    for edge in &nav.edges {
        if !edge.is_walk() {
            continue;
        }
        report.walking_edges += 1;
        if f32::from(edge.collision_radius) < player_radius
            || f32::from(edge.collision_height) < player_height
        {
            continue;
        }
        let Some(end_idx) = edge.end_point else {
            report.unresolved_end += 1;
            continue;
        };
        report.eligible += 1;

        let start_idx = edge.start_point.unwrap_or(edge.owner);
        let start_pt = &nav.points[start_idx];
        let end_pt = &nav.points[end_idx];
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
                    contact_height_above_bottom_uu: None,
                    floor_below_center_uu: floor_below(&world, start)
                        .map(|(d, _)| d * UNREAL_UNITS_PER_METER),
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

        let cause = classify(&world, &walk, 2.0 * half[1]);
        *report.groups.entry(cause.clone()).or_default() += 1;
        let remaining_uu = dist_xz(walk.position, end) * UNREAL_UNITS_PER_METER;
        let blocking_source = walk
            .contacts
            .last()
            .map(|c| scene.collision_sources[c.source as usize].clone());
        let contact_normal = walk.contacts.last().map(|c| c.normal);
        let contact_height_above_bottom_uu = walk.contacts.last().map(|c| {
            let tri = world.triangle(c.triangle);
            let centroid_y = (tri[0][1] + tri[1][1] + tri[2][1]) / 3.0;
            (centroid_y - (c.position[1] - half[1])) * UNREAL_UNITS_PER_METER
        });
        let floor_below_center_uu =
            floor_below(&world, walk.position).map(|(d, _)| d * UNREAL_UNITS_PER_METER);
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
            contact_height_above_bottom_uu,
            floor_below_center_uu,
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
    /// measured value is a regression.
    #[test]
    fn opt_in_reach_regression_plage00_plage01_banque01() {
        let Some(path) = opt_in_game_dir() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        for (map, min_pass, want_missing_floor) in [
            ("Plage00", 12usize, 0usize),
            ("Plage01", 294usize, 0usize),
            ("Banque01", 631usize, 0usize),
        ] {
            let report = analyze(map, &path).expect("reach analyze");
            let missing_floor: usize = report
                .groups
                .iter()
                .filter(|(cause, _)| cause.starts_with("missing floor"))
                .map(|(_, n)| *n)
                .sum();
            println!(
                "[reach-test regression] {map}: eligible {} passes {} (>= {min_pass}), missing-floor {missing_floor} (== {want_missing_floor})",
                report.eligible, report.passes
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
    /// spot; item1k's report lists them. The values are measured, not targets.
    #[test]
    fn opt_in_reach_regression_multi_region_terrains() {
        let Some(path) = opt_in_game_dir() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        for (map, min_pass, eligible, max_missing_floor) in [
            ("Hual01b", 760usize, 816usize, 3usize),
            ("Hual04c", 435usize, 437usize, 0usize),
            ("Kello01a", 1459usize, 1488usize, 15usize),
            ("PRock04a", 702usize, 721usize, 16usize),
        ] {
            let report = analyze(map, &path).expect("reach analyze");
            let missing_floor: usize = report
                .groups
                .iter()
                .filter(|(cause, _)| cause.starts_with("missing floor"))
                .map(|(_, n)| *n)
                .sum();
            println!(
                "[reach-test regression] {map}: eligible {} passes {} (>= {min_pass}), missing-floor {missing_floor} (<= {max_missing_floor})",
                report.eligible, report.passes
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
        }
    }
}
