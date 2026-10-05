//! Headless collision probes (M2a exit): resolve the player extent box from class defaults,
//! build a swept-collision world from the imported map, and run a doorway pass/block test at
//! `Porte6` on Plage01. No window is opened.
//!
//! Everything printed here is measured. Unknowns fail loudly; nothing is tuned to force a
//! PASS. The 50 units/m constant in `xiii-decode::common` is not changed by this module.

use std::time::Instant;

use xiii_collision::{CollisionWorld, MoveParams, Vec3, WalkParams, walk_move};
use xiii_decode::common::{UNREAL_UNITS_PER_METER, actor_to_bevy};
use xiii_decode::model::level;
use xiii_decode::skeletal::validate::bounds;
use xiii_decode::skeletal::{Skeleton, SkinnedMesh, decode_skeletal_mesh};
use xiii_install::{Installation, OpenOptions, PackageKind};
use xiii_package::{Limits, PropertyValue};
use xiii_script::{
    ObjRef, ScriptLimits, ScriptObject, ScriptPackage, ScriptSet, Value, Vm, VmLimits,
};

use xiii_world::{PackageCache, WorldScene};

const PLAYER_PAWN_FALLBACK: &str = "XIII.XIIIPlayerPawn";

/// UE2 `MINFLOORZ`: a surface is walkable (a floor) when its unit normal's up component is
/// at least this. Hypothesis for XIII, same as upstream UE2.
pub(crate) const MINFLOORZ: f32 = 0.7;

/// Upstream UE2 `MAXSTEPHEIGHT` in Unreal units. Used because the decoded XIII class
/// defaults contain no step-height property (see [`report_step_height_evidence`]); it is the
/// documented upstream hypothesis for XIII, not a measured XIII value.
pub(crate) const MAXSTEPHEIGHT_UU: f32 = 35.0;

// Harness distances in Unreal units, so the test does not change with the metre scale
// (values equal the original metre literals at the former 50 UU/m).
pub(crate) const SKIN_UU: f32 = 0.05;
pub(crate) const STEP_UU: f32 = 2.5;
const DOOR_FRONT_UU: f32 = 100.0;
const FLOOR_PROBE_UU: f32 = 100.0;
const START_LIFT_UU: f32 = 2.5;
const RAISE_INC_UU: f32 = 1.0;

/// Entry point for `--collision-test`.
pub fn run(map: &str, game_dir: &std::path::Path) -> bevy::app::AppExit {
    match run_inner(map, game_dir) {
        Ok(()) => bevy::app::AppExit::Success,
        Err(e) => {
            eprintln!("error: {e}");
            bevy::app::AppExit::Error(std::num::NonZeroU8::new(1).expect("nonzero"))
        }
    }
}

fn run_inner(map: &str, game_dir: &std::path::Path) -> Result<(), String> {
    let total_started = Instant::now();

    // ---- import the map (read-only) ------------------------------------------------------
    let import_started = Instant::now();
    let mut cache = PackageCache::open(game_dir)?;
    let map_pkg = cache.map(map)?;
    let actors = level::scan_level(&map_pkg.package, &map_pkg.data);
    let scene = xiii_world::import_map(&mut cache, map)?;
    println!(
        "[collision-test] {map}: imported in {:.2}s ({} collision triangles, {} sources)",
        import_started.elapsed().as_secs_f32(),
        scene.collision.len(),
        scene.collision_sources.len()
    );

    // ---- player extents from resolved class defaults -------------------------------------
    let install = Installation::open(game_dir, &OpenOptions::default())
        .map_err(|e| format!("opening installation for class defaults: {e}"))?;
    let (set, gameinfo, pawn_class_path) = resolve_player_class(&install)?;
    let (radius, height, eye_height, ground_speed, jump_z) =
        player_extents(&set, &pawn_class_path)?;
    let half = [
        radius / UNREAL_UNITS_PER_METER,
        height / UNREAL_UNITS_PER_METER,
        radius / UNREAL_UNITS_PER_METER,
    ];
    println!(
        "[collision-test] player class {pawn_class_path} (GameInfo {gameinfo} via Default.ini DefaultGame -> DefaultPlayerClassName)"
    );
    println!(
        "[collision-test] class defaults: CollisionRadius={radius} CollisionHeight={height} (half) BaseEyeHeight={eye_height} GroundSpeed={ground_speed} JumpZ={jump_z} Unreal units"
    );
    println!(
        "[collision-test] extent half extents (R,H,R)/{UNREAL_UNITS_PER_METER} = ({:.3}, {:.3}, {:.3}) m",
        half[0], half[1], half[2]
    );
    if let Err(e) = report_mesh_bounds(&set, &pawn_class_path) {
        println!("[collision-test] mesh-height: could not measure the pawn mesh: {e}");
    }

    // ---- collision world -----------------------------------------------------------------
    let build_started = Instant::now();
    let world = CollisionWorld::new(scene.collision.iter().map(|(t, src)| (*t, *src)));
    let build = build_started.elapsed();
    println!(
        "[collision-test] collision world: {} triangles ({} degenerate dropped), {} BVH nodes, built in {:.2} ms",
        world.triangle_count(),
        world.degenerate_count(),
        world.bvh_node_count(),
        build.as_secs_f64() * 1000.0
    );

    // NavigationPoint alignment test: in UE2 the editor places a NavigationPoint so that its
    // collision cylinder RESTS on the floor: Location.Z = floor + CollisionHeight. For every
    // PlayerStart / PathNode / other NavigationPoint subclass on the map, measure
    // (Location.Z - floor below - CollisionHeight) against the imported collision world.
    for c in [
        "Engine.PlayerStart",
        "Engine.PathNode",
        "XIDPawn.AttackPoint",
        "XIDPawn.doorpoint",
        "XIDPawn.StrategicPoint",
        "XIDPawn.PatrolPoint",
        "XIDPawn.SafePoint",
        "XIDPawn.gennmi",
    ] {
        navigation_point_alignment(&world, &set, &actors, c);
    }

    // ---- identify Porte6 ------------------------------------------------------------------
    let door = identify_door(&scene, &actors, "Porte6");
    let door = match door {
        Some(d) => d,
        None if map_has_porte6(&actors) => {
            return Err("no collision source contains 'Porte6' in this map".into());
        }
        None => {
            println!(
                "[collision-test] map has no Porte6 (no collision source, no actor); skipping the door cases"
            );
            return Ok(());
        }
    };
    println!(
        "[collision-test] Porte6: class {} location {:?}",
        door.class, door.location
    );
    for (path, tris, bbox) in &door.sources {
        println!(
            "[collision-test]   source {path}: {tris} triangles, bbox min {:.2},{:.2},{:.2} max {:.2},{:.2},{:.2}",
            bbox.0[0], bbox.0[1], bbox.0[2], bbox.1[0], bbox.1[1], bbox.1[2]
        );
    }
    let mut bbox = door_bbox_empty();
    for (_, _, b) in &door.sources {
        for k in 0..3 {
            bbox.0[k] = bbox.0[k].min(b.0[k]);
            bbox.1[k] = bbox.1[k].max(b.1[k]);
        }
    }
    let door_center = box_center(bbox);
    let (width_axis, height_axis) = horizontal_axes(bbox);
    println!(
        "[collision-test] Porte6 combined bbox min ({:.2},{:.2},{:.2}) m = ({:.1},{:.1},{:.1}) UU, max ({:.2},{:.2},{:.2}) m = ({:.1},{:.1},{:.1}) UU; center ({:.2},{:.2},{:.2}) m",
        bbox.0[0],
        bbox.0[1],
        bbox.0[2],
        bbox.0[0] * UNREAL_UNITS_PER_METER,
        bbox.0[1] * UNREAL_UNITS_PER_METER,
        bbox.0[2] * UNREAL_UNITS_PER_METER,
        bbox.1[0],
        bbox.1[1],
        bbox.1[2],
        bbox.1[0] * UNREAL_UNITS_PER_METER,
        bbox.1[1] * UNREAL_UNITS_PER_METER,
        bbox.1[2] * UNREAL_UNITS_PER_METER,
        door_center[0],
        door_center[1],
        door_center[2]
    );

    // ---- door opening measurement --------------------------------------------------------
    let without_door = CollisionWorld::new(
        scene
            .collision
            .iter()
            .filter(|(_, src)| !scene.collision_sources[*src as usize].contains("Porte6"))
            .map(|(t, src)| (*t, *src)),
    );
    let opening = measure_opening(
        &without_door,
        &scene.collision_sources,
        door_center,
        width_axis,
        height_axis,
    );
    println!(
        "[collision-test] door opening (clear passage, Porte6 excluded): width {:.3} m / {:.1} u ({:?}); height {:.3} m / {:.1} u ({:?})",
        opening.width_m,
        opening.width_m * UNREAL_UNITS_PER_METER,
        opening.width_hit,
        opening.height_m,
        opening.height_m * UNREAL_UNITS_PER_METER,
        opening.height_hit
    );

    // ---- PlayerStart placement and its own collision defaults ----------------------------
    let Some((player_start, rotation)) = scene.player_start else {
        return Err("map has no PlayerStart".into());
    };
    let _ = rotation;
    println!("[collision-test] PlayerStart position {:?}", player_start);

    // Effective placement of the PlayerStart (map property vs class default) and the map's
    // own CollisionHeight override when present.
    let mut class_defaults = xiii_world::ClassDefaults::open(game_dir)?;
    if let Some(ps) = actors.player_starts.first() {
        match class_defaults.resolve(&ps.class, ps) {
            Ok((eff, src)) => {
                let tag = |s: level::PlacementSource| match s {
                    level::PlacementSource::MapProperty => "map",
                    level::PlacementSource::ClassDefault => "class-default",
                    level::PlacementSource::EngineDefault => "engine-default",
                };
                println!(
                    "[collision-test] PlayerStart effective: Location=({:.1},{:.1},{:.1}) UU [{}], Rotation={:?} [{}], DrawScale={} [{}]",
                    eff.location[0],
                    eff.location[1],
                    eff.location[2],
                    tag(src[0]),
                    eff.rotation,
                    tag(src[1]),
                    eff.draw_scale,
                    tag(src[2]),
                );
                let ch = ps
                    .collision_height
                    .map(|v| v.to_string())
                    .unwrap_or_else(|| "<class default>".into());
                println!("[collision-test] PlayerStart map CollisionHeight override: {ch}");
            }
            Err(e) => println!("[collision-test] PlayerStart effective: unresolved: {e}"),
        }
    }

    // The map's PlayerStart class (Engine.PlayerStart) has its own collision cylinder; print
    // its resolved defaults so the PlayerStart->floor offset can be interpreted.
    match class_layout_of(&set, "Engine.PlayerStart", Some("Engine")) {
        Ok(ps_layout) => {
            let r = layout_float_opt(&ps_layout, "CollisionRadius");
            let h = layout_float_opt(&ps_layout, "CollisionHeight");
            println!(
                "[collision-test] PlayerStart class Engine.PlayerStart defaults: CollisionRadius={r} CollisionHeight={h} (half) Unreal units"
            );
        }
        Err(e) => {
            println!("[collision-test] PlayerStart class defaults unresolved: {e}");
        }
    }
    // Floor directly below the PlayerStart (downward ray), metres and Unreal units.
    let down = world.ray(
        player_start,
        [player_start[0], player_start[1] - 10.0, player_start[2]],
    );
    match down {
        Some(h) => {
            let f = player_start[1] - h.t * 10.0;
            println!(
                "[collision-test] PlayerStart height above the surface below it: {:.2} m = {:.1} UU (surface {:.2} m, normal ({:.2},{:.2},{:.2}), {})",
                player_start[1] - f,
                (player_start[1] - f) * UNREAL_UNITS_PER_METER,
                f,
                h.normal[0],
                h.normal[1],
                h.normal[2],
                if h.normal[1] > 0.5 {
                    "floor"
                } else {
                    "not up-facing"
                }
            );
        }
        None => println!(
            "[collision-test] PlayerStart height: no surface found below the PlayerStart within 10 m"
        ),
    }
    // The interior walkable mesh near the PlayerStart (this is what the flat box will stand on
    // when 50 u/m makes it 3 m tall). A second downward ray from above reports it.
    if let Some(f) = floor_y_at(&world, player_start, player_start[1] + 3.0) {
        println!(
            "[collision-test] interior floor above the PlayerStart: {:.2} m = {:.1} UU",
            f,
            f * UNREAL_UNITS_PER_METER
        );
    }

    // ---- StaticMeshActor286 / GR_interieur01 placement evidence --------------------------
    if let Some(sma) = actors
        .static_mesh_actors
        .iter()
        .find(|a| a.path.contains("StaticMeshActor286"))
    {
        report_static_mesh_actor(&scene, &mut class_defaults, sma, player_start);
        // Does the mesh's collision set 0 match its render geometry? (Count triangles and
        // compare vertex positions of the two decoded sets.)
        if let Some(r) = sma.static_mesh
            && let Ok((pkg, idx)) = cache.resolve(&map_pkg, r)
        {
            match xiii_decode::static_mesh::decode_static_mesh(&pkg.package, &pkg.data, idx) {
                Ok(m) => {
                    let (n0, n1) = (
                        m.collision[0].triangles.len(),
                        m.collision[1].triangles.len(),
                    );
                    let (v0, v1) = (m.collision[0].vertices.len(), m.collision[1].vertices.len());
                    // Collision set 0 shares the render vertex positions when its vertices
                    // equal the render vertex positions (same coordinates).
                    let render = &m.vertices;
                    let mut same_as_render = 0usize;
                    for c in &m.collision[0].vertices {
                        if render.iter().any(|v| {
                            (v.position[0] - c[0]).abs() < 1e-3
                                && (v.position[1] - c[1]).abs() < 1e-3
                                && (v.position[2] - c[2]).abs() < 1e-3
                        }) {
                            same_as_render += 1;
                        }
                    }
                    let render_tris = m.indices.len() / 3;
                    let enabled = m.materials.iter().filter(|mm| mm.enable_collision).count();
                    println!(
                        "[collision-test]   GR_interieur01: collision set 0 = {n0} tris / {v0} verts ({same_as_render} of {v0} positions equal render verts), render = {render_tris} tris / {nv} verts; collision set 1 (simplified) = {n1} tris / {v1} verts; material slots with EnableCollision: {enabled}/{nm}",
                        nv = render.len(),
                        nm = m.materials.len(),
                    );
                }
                Err(e) => println!("[collision-test]   GR_interieur01 mesh decode failed: {e}"),
            }
        }
    }

    let spawn = place_spawn(&world, player_start, half)?;
    println!(
        "[collision-test] spawn placement (UE2 FindSpot approximation): raise {:.3} m = {:.1} UU, final center {:?}; box bottom {:.3} m, floor below {:.3} m",
        spawn.raise,
        spawn.raise * UNREAL_UNITS_PER_METER,
        spawn.position,
        spawn.position[1] - half[1],
        spawn.floor
    );

    // ---- UE2 step-height evidence (before using the upstream constant) -------------------
    report_step_height_evidence(&set);

    // Walk parameters. The doorway harness uses UE2's MAXSTEPHEIGHT (35 UU, upstream
    // constant) converted with the coordinate policy, and MINFLOORZ 0.7.
    let walk_params = WalkParams {
        skin: SKIN_UU / UNREAL_UNITS_PER_METER,
        max_iterations: 4,
        max_step_height: MAXSTEPHEIGHT_UU / UNREAL_UNITS_PER_METER,
        min_floor_z: MINFLOORZ,
    };
    let step = STEP_UU / UNREAL_UNITS_PER_METER;

    // ---- Case 1: UE2-style walk from the real PlayerStart, re-aiming every step ----------
    let walk_started = Instant::now();
    let closed = walk_toward_walk(
        &world,
        spawn.position,
        door_center,
        half,
        step,
        1200,
        &walk_params,
    );
    let closed_time = walk_started.elapsed();
    let closed_past = past_plane(closed.position, door_center, closed.last_heading);
    let closed_door = closed
        .last_source
        .map(|s| scene.collision_sources[s as usize].clone());
    let closed_blocked_by_door = closed
        .last_source
        .is_some_and(|s| scene.collision_sources[s as usize].contains("Porte6"));
    let closed_past_ok = closed_past >= 1.0;
    let closed_ok = closed_blocked_by_door && !closed_past_ok;
    println!(
        "[collision-test] PlayerStart case closed (walk_move, max_step_height {:.1} UU): {} blocked={} falling={} last_source={:?} past_door_plane={:.2} m steps={} ({:.0} ms)",
        MAXSTEPHEIGHT_UU,
        pass_fail(closed_ok),
        closed.blocked,
        closed.falling,
        closed_door,
        closed_past,
        closed.steps,
        closed_time.as_secs_f64() * 1000.0
    );
    print_contacts(
        "PlayerStart closed",
        &world,
        &scene.collision_sources,
        &closed,
        half,
    );

    let walk_started = Instant::now();
    let open = walk_toward_walk(
        &without_door,
        spawn.position,
        door_center,
        half,
        step,
        1200,
        &walk_params,
    );
    let open_time = walk_started.elapsed();
    let open_past = past_plane(open.position, door_center, open.last_heading);
    let open_ok = open_past >= 1.0;
    let open_block = open
        .last_source
        .map(|s| scene.collision_sources[s as usize].clone());
    println!(
        "[collision-test] PlayerStart case open (walk_move): {} blocked={} falling={} last_source={:?} past_door_plane={:.2} m steps={} ({:.0} ms)",
        pass_fail(open_ok),
        open.blocked,
        open.falling,
        open_block,
        open_past,
        open.steps,
        open_time.as_secs_f64() * 1000.0
    );
    print_contacts(
        "PlayerStart open",
        &without_door,
        &scene.collision_sources,
        &open,
        half,
    );

    // Old walker kept as a labelled diagnostic (the previous task's flat `move_slide`).
    let slide_params = MoveParams {
        skin: SKIN_UU / UNREAL_UNITS_PER_METER,
        max_iterations: 4,
        max_step_height: 0.0,
    };
    let diag = walk_toward(
        &world,
        spawn.position,
        door_center,
        half,
        step,
        1200,
        Mover::Slide(&slide_params),
    );
    let diag_past = past_plane(diag.position, door_center, diag.last_heading);
    let diag_src = diag
        .last_source
        .map(|s| scene.collision_sources[s as usize].clone());
    println!(
        "[collision-test] PlayerStart move_slide diagnostic (old walker, max_step_height=0): blocked={} last_source={:?} past_door_plane={:.2} m steps={}",
        diag.blocked, diag_src, diag_past, diag.steps
    );

    // ---- Case 2: aligned door case, 100 UU in front of the leaf along its normal -------------
    let door_normal = door_plane_normal(bbox, spawn.position, door_center);
    let door_front = [
        door_center[0] - door_normal[0] * DOOR_FRONT_UU / UNREAL_UNITS_PER_METER,
        door_center[1],
        door_center[2] - door_normal[2] * DOOR_FRONT_UU / UNREAL_UNITS_PER_METER,
    ];
    let floor_door = floor_y_at(
        &world,
        door_front,
        door_center[1] + FLOOR_PROBE_UU / UNREAL_UNITS_PER_METER,
    )
    .ok_or_else(|| "no floor was found in front of Porte6".to_string())?;
    let aligned_start = [
        door_front[0],
        floor_door + START_LIFT_UU / UNREAL_UNITS_PER_METER + half[1],
        door_front[2],
    ];
    println!(
        "[collision-test] aligned door case start {:?} (floor {:.2} m, box bottom {:.2} m, top {:.2} m); heading along door normal ({:.2},{:.2},{:.2})",
        aligned_start,
        floor_door,
        aligned_start[1] - half[1],
        aligned_start[1] + half[1],
        door_normal[0],
        door_normal[1],
        door_normal[2]
    );

    let walk_started = Instant::now();
    let a_closed = walk(
        &world,
        aligned_start,
        door_normal,
        half,
        step,
        1200,
        Mover::Slide(&slide_params),
    );
    let a_closed_time = walk_started.elapsed();
    let a_closed_past = past_plane(a_closed.position, door_center, door_normal);
    let a_closed_door = a_closed
        .last_source
        .map(|s| scene.collision_sources[s as usize].clone());
    let a_closed_ok = a_closed
        .last_source
        .is_some_and(|s| scene.collision_sources[s as usize].contains("Porte6"))
        && a_closed_past < 1.0;
    println!(
        "[collision-test] aligned door case closed: {} blocked={} last_source={:?} past_door_plane={:.2} m steps={} ({:.0} ms)",
        pass_fail(a_closed_ok),
        a_closed.blocked,
        a_closed_door,
        a_closed_past,
        a_closed.steps,
        a_closed_time.as_secs_f64() * 1000.0
    );
    print_contacts(
        "aligned closed",
        &world,
        &scene.collision_sources,
        &a_closed,
        half,
    );

    let walk_started = Instant::now();
    let a_open = walk(
        &without_door,
        aligned_start,
        door_normal,
        half,
        step,
        1200,
        Mover::Slide(&slide_params),
    );
    let a_open_time = walk_started.elapsed();
    let a_open_past = past_plane(a_open.position, door_center, door_normal);
    let a_open_ok = a_open_past >= 1.0;
    println!(
        "[collision-test] aligned door case open: {} blocked={} past_door_plane={:.2} m steps={} ({:.0} ms)",
        pass_fail(a_open_ok),
        a_open.blocked,
        a_open_past,
        a_open.steps,
        a_open_time.as_secs_f64() * 1000.0
    );
    print_contacts(
        "aligned open",
        &without_door,
        &scene.collision_sources,
        &a_open,
        half,
    );

    println!(
        "[collision-test] timings: BVH build {:.2} ms, PlayerStart closed {:.0} ms, PlayerStart open {:.0} ms, aligned closed {:.0} ms, aligned open {:.0} ms, total {:.2} s",
        build.as_secs_f64() * 1000.0,
        closed_time.as_secs_f64() * 1000.0,
        open_time.as_secs_f64() * 1000.0,
        a_closed_time.as_secs_f64() * 1000.0,
        a_open_time.as_secs_f64() * 1000.0,
        total_started.elapsed().as_secs_f32()
    );

    println!(
        "[collision-test] RESULT PlayerStart case: closed {} open {}",
        pass_fail(closed_ok),
        pass_fail(open_ok)
    );
    println!(
        "[collision-test] RESULT aligned door case: closed {} open {}",
        pass_fail(a_closed_ok),
        pass_fail(a_open_ok)
    );
    let aligned_ok = a_closed_ok && a_open_ok;
    if aligned_ok {
        println!(
            "[collision-test] RESULT: PASS on the aligned door case ({} for the PlayerStart case; see numbers)",
            if closed_ok && open_ok {
                "also PASS"
            } else {
                "FAIL"
            }
        );
    } else {
        println!("[collision-test] RESULT: FAIL (aligned door case did not pass; not tuned)");
    }
    if aligned_ok {
        Ok(())
    } else {
        Err(format!(
            "aligned door case failed: closed_ok={a_closed_ok} open_ok={a_open_ok} (closed_past={a_closed_past:.2}, open_past={a_open_past:.2})"
        ))
    }
}

// -------------------------------------------------------------------------------------------
// Player class resolution
// -------------------------------------------------------------------------------------------

/// NavigationPoint alignment test (see the report): for every actor of one class with a
/// decoded `Location`, compute `Location.Z - floor below - CollisionHeight` in UU where
/// - CollisionHeight = the map's own property, else the class's inherited default;
/// - "floor below" = nearest upward-facing hit of a downward ray starting just above the
///   actor origin, from the full collision world (BSP + static meshes + terrain).
///
/// Prints the (n, min, median, max, histogram) of the offsets and examples.
fn navigation_point_alignment(
    world: &CollisionWorld,
    set: &ScriptSet,
    actors: &level::LevelActors,
    class: &str,
) {
    let placements: Vec<&level::ActorPlacement> = actors
        .all_located
        .iter()
        .filter(|a| a.class.eq_ignore_ascii_case(class))
        .collect();
    if placements.is_empty() {
        return;
    }
    let class_h = class_layout_of(set, class, None)
        .ok()
        .and_then(|l| layout_float(&l, "CollisionHeight").ok());
    let mut rows: Vec<(String, f32, f32, &'static str)> = Vec::new();
    for a in &placements {
        let Some(loc) = a.location else {
            continue;
        };
        let bevy = xiii_decode::common::to_bevy_position(loc);
        // The engine's "floor below" is the surface the collision cylinder would rest on: the
        // nearest hit below the actor origin. Probing from far above would return an
        // overhang/deck above an interior actor instead (the Plage01 PlayerStart sits in a
        // 48 UU crawlspace under a deck), so start just above the origin.
        let Some(floor) = floor_y_at(world, bevy, bevy[1] + 0.1) else {
            continue;
        };
        let floor_uu = floor * UNREAL_UNITS_PER_METER;
        let (ch, ch_src) = match a.collision_height {
            Some(v) => (v, "map"),
            None => match class_h {
                Some(v) => (v, "class-default"),
                None => (f32::NAN, "none"),
            },
        };
        rows.push((
            a.path.clone(),
            loc[2] - floor_uu - if ch.is_nan() { 0.0 } else { ch },
            ch,
            ch_src,
        ));
    }
    let tested = rows.iter().filter(|(_, _, _, s)| *s != "none").count();
    let mut offs: Vec<f32> = rows
        .iter()
        .filter(|(_, _, _, s)| *s != "none")
        .map(|(_, o, _, _)| *o)
        .collect();
    offs.sort_by(|a, b| a.total_cmp(b));
    if offs.is_empty() {
        println!("[collision-test] alignment {class}: no testable rows");
        return;
    }
    let n = offs.len();
    let median = if n % 2 == 1 {
        offs[n / 2]
    } else {
        (offs[n / 2 - 1] + offs[n / 2]) / 2.0
    };
    let mut hist: std::collections::BTreeMap<i64, usize> = Default::default();
    for &o in &offs {
        *hist.entry(((o * 2.0).round() / 2.0) as i64).or_default() += 1;
    }
    let top = hist
        .iter()
        .rev()
        .take(6)
        .map(|(k, v)| format!("{k}:{v}"))
        .collect::<Vec<_>>()
        .join(" ");
    println!(
        "[collision-test] alignment {class}: {} placed, floor found for {tested}; Location.Z - floor - CollisionHeight (UU) min {:+.1} median {:+.1} max {:+.1}; histogram (round UU:count) {top}",
        tested,
        offs[0],
        median,
        offs[n - 1],
    );
    for (path, o, ch, src) in rows.iter().take(2) {
        println!(
            "[collision-test]   alignment {class} example {path}: offset {o:+.1} UU (CollisionHeight {ch} [{src}])"
        );
    }
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
pub(crate) fn resolve_player_class(
    install: &Installation,
) -> Result<(ScriptSet, String, String), String> {
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
    // The stock engine field is named `DefaultPlayerClass`; report both spellings clearly.
    Err(format!(
        "{gameinfo_path} has no DefaultPlayerClassName; using fallback {PLAYER_PAWN_FALLBACK} is not allowed (no silent fallback)"
    ))
}

/// A resolved class layout (`Vm::class_layout`: the class chain's slots and inherited
/// defaults, root-to-leaf). Shared by the pawn and PlayerStart resolutions.
type Layout = std::rc::Rc<xiii_script::vm::ClassLayout>;

/// Resolves the inherited default layout of `Package.Class` via `Vm::class_layout`.
/// `package`/`class` are split on the first `.`; a bare class name resolves only when
/// `default_package` is given.
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

/// Resolved float default or `NaN` when absent (secondary calibration fields).
fn layout_float_opt(layout: &Layout, name: &str) -> f32 {
    layout_float(layout, name).unwrap_or(f32::NAN)
}

/// Resolved vector default of a class layout, if present and a vector.
fn layout_vector(layout: &Layout, name: &str) -> Option<Vec3> {
    let slot = layout.slot_by_name(name)?;
    match layout.defaults.get(slot.base) {
        Some(Value::Vector(v)) => Some(*v),
        _ => None,
    }
}

/// Resolved object default of a class layout (e.g. the pawn's `Mesh`).
fn layout_object(layout: &Layout, name: &str) -> Option<xiii_script::ObjRef> {
    let slot = layout.slot_by_name(name)?;
    match layout.defaults.get(slot.base) {
        Some(Value::Object(Some(r))) => Some(*r),
        _ => None,
    }
}

/// Resolved inherited collision defaults of the player pawn class.
pub(crate) fn player_extents(
    set: &ScriptSet,
    pawn_path: &str,
) -> Result<(f32, f32, f32, f32, f32), String> {
    let layout = class_layout_of(set, pawn_path, None)?;
    println!(
        "[collision-test] inheritance chain: {}",
        layout.chain_names.join(" <- ")
    );
    Ok((
        layout_float(&layout, "CollisionRadius")?,
        layout_float(&layout, "CollisionHeight")?,
        layout_float_opt(&layout, "BaseEyeHeight"),
        layout_float_opt(&layout, "GroundSpeed"),
        layout_float_opt(&layout, "JumpZ"),
    ))
}

/// Third calibration reference: the pawn's visible mesh bind-pose bounds (Unreal units).
///
/// Resolves `Mesh` from the pawn's inherited defaults, decodes the SkeletalMesh from its
/// package and reports the bind-pose vertex bounds. `DrawScale`, `DrawScale3D` and `PrePivot`
/// are reported (applied? shown); `MeshScale`/`MeshOrigin`/`RotOrigin` from the mesh are
/// reported as present or not, but are not applied to the bind-pose bounds.
fn report_mesh_bounds(set: &ScriptSet, pawn_path: &str) -> Result<(), String> {
    let layout = class_layout_of(set, pawn_path, None)?;
    let Some(obj) = layout_object(&layout, "Mesh") else {
        println!(
            "[collision-test] mesh-height: {pawn_path}.Mesh is none/absent; cannot measure the visible mesh"
        );
        return Ok(());
    };
    let ObjRef::Static(g) = obj else {
        println!(
            "[collision-test] mesh-height: {pawn_path}.Mesh is an instance reference; cannot measure"
        );
        return Ok(());
    };
    let p = &set.packages[g.package];
    let class_name = p
        .package
        .object_name(xiii_package::ObjectRef::Export(g.export));
    let label = format!("{}.{}", p.name, class_name.unwrap_or("?"));
    let raw = decode_skeletal_mesh(&p.package, &p.data, g.export as usize)
        .map_err(|e| format!("Mesh {label}: {e}"))?;
    let skeleton = Skeleton::from_mesh(&raw).map_err(|e| format!("Mesh {label}: skeleton: {e}"))?;
    let mesh = SkinnedMesh::from_raw(&raw, &skeleton, Some(&p.package))
        .map_err(|e| format!("Mesh {label}: skin: {e}"))?;
    let (bmin, bmax) = bounds(&mesh.positions);
    // Bind-pose bounds are in mesh space (X forward, Y right, Z up), Unreal units.
    let (forward, right, up) = (bmax.x - bmin.x, bmax.y - bmin.y, bmax.z - bmin.z);
    println!(
        "[collision-test] mesh-height: {label} bind bounds min ({:.1},{:.1},{:.1}) max ({:.1},{:.1},{:.1}) UU; foot-to-head Z {:.1} UU = {:.3} m at 50 u/m (right Y {:.1} UU, forward X {:.1} UU)",
        bmin.x,
        bmin.y,
        bmin.z,
        bmax.x,
        bmax.y,
        bmax.z,
        up,
        up / UNREAL_UNITS_PER_METER,
        right,
        forward
    );
    let draw_scale = layout_float_opt(&layout, "DrawScale");
    let draw_scale3d = layout_vector(&layout, "DrawScale3D");
    let pre_pivot = layout_vector(&layout, "PrePivot");
    println!(
        "[collision-test] mesh-height: actor defaults DrawScale={draw_scale} DrawScale3D={draw_scale3d:?} PrePivot={pre_pivot:?} (bind bounds above are raw mesh space, before these); mesh MeshScale={:?} MeshOrigin={:?} RotOrigin={:?}",
        mesh.mesh_scale.to_array(),
        mesh.mesh_origin.to_array(),
        mesh.rot_origin
    );
    Ok(())
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

// -------------------------------------------------------------------------------------------
// Door identification and measurement
// -------------------------------------------------------------------------------------------

type Box3 = (Vec3, Vec3);

struct DoorInfo {
    class: String,
    location: Vec3,
    sources: Vec<(String, usize, Box3)>,
}

fn door_bbox_empty() -> Box3 {
    ([f32::INFINITY; 3], [f32::NEG_INFINITY; 3])
}

fn box_center(b: Box3) -> Vec3 {
    [
        (b.0[0] + b.1[0]) * 0.5,
        (b.0[1] + b.1[1]) * 0.5,
        (b.0[2] + b.1[2]) * 0.5,
    ]
}

/// The two axes with the largest extents are the door's width and height; the smallest is
/// the face normal axis. Returns `(width_axis, height_axis)` as unit vectors.
fn horizontal_axes(b: Box3) -> (Vec3, Vec3) {
    let ext = [b.1[0] - b.0[0], b.1[1] - b.0[1], b.1[2] - b.0[2]];
    let height = 1usize;
    let mut horizontal: Vec<usize> = (0..3).filter(|&i| i != height).collect();
    horizontal.sort_by(|&a, &b| ext[b].total_cmp(&ext[a]));
    let mut w = [0.0f32; 3];
    w[horizontal[0]] = 1.0;
    let mut h = [0.0f32; 3];
    h[height] = 1.0;
    (w, h)
}

/// Finds the actor class/location and the collision sources for an actor path fragment.
fn identify_door(scene: &WorldScene, actors: &level::LevelActors, name: &str) -> Option<DoorInfo> {
    let mut sources = Vec::new();
    for (i, s) in scene.collision_sources.iter().enumerate() {
        if !s.contains(name) {
            continue;
        }
        let mut b = door_bbox_empty();
        let mut n = 0usize;
        for (t, src) in &scene.collision {
            if *src as usize != i {
                continue;
            }
            n += 1;
            for v in t {
                for (k, c) in v.iter().enumerate() {
                    b.0[k] = b.0[k].min(*c);
                    b.1[k] = b.1[k].max(*c);
                }
            }
        }
        sources.push((s.clone(), n, b));
    }
    if sources.is_empty() {
        return None;
    }
    let actor: Option<&level::ActorPlacement> = actors
        .static_mesh_actors
        .iter()
        .find(|a| a.path.contains(name));
    Some(DoorInfo {
        class: actor
            .map(|a| a.class.clone())
            .unwrap_or_else(|| "<no actor record>".to_owned()),
        location: actor
            .and_then(|a| a.location)
            .map_or([0.0; 3], xiii_decode::common::to_bevy_position),
        sources,
    })
}

struct Opening {
    width_m: f32,
    width_hit: Option<String>,
    height_m: f32,
    height_hit: Option<String>,
}

/// Measures the clear opening by casting rays from the door-leaf center outward along the
/// width axis and upward, against a world with the door leaf removed.
fn measure_opening(
    clear: &CollisionWorld,
    sources: &[String],
    center: Vec3,
    width_axis: Vec3,
    height_axis: Vec3,
) -> Opening {
    let reach = 5.0f32;
    let probe = |dir: Vec3| -> (f32, Option<u32>) {
        let start = center;
        let end = [
            start[0] + dir[0] * reach,
            start[1] + dir[1] * reach,
            start[2] + dir[2] * reach,
        ];
        match clear.ray(start, end) {
            Some(h) => (h.t * reach, Some(h.source)),
            None => (reach, None),
        }
    };
    let plus_w = probe(width_axis);
    let minus_w = probe([-width_axis[0], -width_axis[1], -width_axis[2]]);
    let up = probe(height_axis);
    let down = probe([-height_axis[0], -height_axis[1], -height_axis[2]]);
    let label = |s: Option<u32>| s.map(|id| sources[id as usize].clone());
    Opening {
        width_m: (plus_w.0 + minus_w.0).min(2.0 * reach),
        width_hit: label(plus_w.1.or(minus_w.1)),
        height_m: (up.0 + down.0).min(2.0 * reach),
        height_hit: label(up.1.or(down.1)),
    }
}

// -------------------------------------------------------------------------------------------
// Movement
// -------------------------------------------------------------------------------------------

/// Result of a spawn placement.
pub(crate) struct Spawn {
    /// Final box center (Bevy metres).
    pub position: Vec3,
    /// Floor height under the box.
    pub floor: f32,
    /// Vertical raise applied to clear an initial overlap (metres).
    pub raise: f32,
}

/// Places the player extent box at the PlayerStart, approximating UE2 spawn `FindSpot`:
/// start with the box bottom at the PlayerStart; if it overlaps anything, raise it in small
/// increments until free (cap `2*H`); then sweep straight down onto the floor.
pub(crate) fn place_spawn(
    world: &CollisionWorld,
    player_start: Vec3,
    half: Vec3,
) -> Result<Spawn, String> {
    // Start with the box bottom at the PlayerStart (UE2 spawns the pawn with its feet there).
    let base_center = [player_start[0], player_start[1] + half[1], player_start[2]];
    let cap = 2.0 * half[1];
    let inc = RAISE_INC_UU / UNREAL_UNITS_PER_METER;
    let mut y = base_center[1];
    let mut raise = 0.0f32;
    loop {
        let center = [base_center[0], y, base_center[2]];
        if world.overlap_aabb(center, half).is_empty() {
            break;
        }
        if raise >= cap {
            return Err(format!(
                "spawn box still overlaps after raising {raise:.3} m (cap {cap:.3} m); no free spot at the PlayerStart"
            ));
        }
        y += inc;
        raise += inc;
    }
    let center = [base_center[0], y, base_center[2]];
    // Sweep straight down from the free position onto the floor.
    let target = [player_start[0], player_start[1] - 3.0, player_start[2]];
    let hit = world
        .sweep(center, target, half)
        .ok_or_else(|| "no floor was found below the PlayerStart".to_string())?;
    if hit.normal[1] <= 0.5 {
        return Err(format!(
            "drop to floor hit a non-floor surface (normal {:?})",
            hit.normal
        ));
    }
    let landed = add(center, scale(sub(target, center), hit.t));
    Ok(Spawn {
        position: landed,
        floor: landed[1] - half[1],
        raise,
    })
}

struct Walk {
    position: Vec3,
    blocked: bool,
    falling: bool,
    steps: usize,
    last_source: Option<u32>,
    last_heading: Vec3,
    contacts: Vec<xiii_collision::MoveContact>,
}

/// Which movement primitive the walk loop uses: the old flat `move_slide` or the UE2-style
/// `walk_move` (step-up / floor-follow).
#[derive(Clone, Copy)]
enum Mover<'a> {
    Slide(&'a MoveParams),
    Walk(&'a WalkParams),
}

impl Mover<'_> {
    fn step(
        &self,
        world: &CollisionWorld,
        pos: Vec3,
        delta: Vec3,
        half: Vec3,
    ) -> xiii_collision::MoveResult {
        match self {
            Mover::Slide(p) => xiii_collision::move_slide(world, pos, delta, half, p),
            Mover::Walk(p) => walk_move(world, pos, delta, half, p),
        }
    }
}

/// Walks `max_steps` of `step` metres along a fixed `heading`, stopping after several steps
/// without progress.
#[allow(clippy::too_many_arguments)]
fn walk(
    world: &CollisionWorld,
    start: Vec3,
    heading: Vec3,
    half: Vec3,
    step: f32,
    max_steps: usize,
    mover: Mover<'_>,
) -> Walk {
    walk_inner(world, start, heading, None, half, step, max_steps, mover)
}

/// Walks toward `target`, re-aiming the heading (horizontal) at `target` on every step.
fn walk_toward(
    world: &CollisionWorld,
    start: Vec3,
    target: Vec3,
    half: Vec3,
    step: f32,
    max_steps: usize,
    mover: Mover<'_>,
) -> Walk {
    let heading = heading_xz(start, target);
    walk_inner(
        world,
        start,
        heading,
        Some(target),
        half,
        step,
        max_steps,
        mover,
    )
}

/// Walks toward `target` with the UE2-style [`xiii_collision::walk_move`].
#[allow(clippy::too_many_arguments)]
fn walk_toward_walk(
    world: &CollisionWorld,
    start: Vec3,
    target: Vec3,
    half: Vec3,
    step: f32,
    max_steps: usize,
    params: &WalkParams,
) -> Walk {
    walk_toward(
        world,
        start,
        target,
        half,
        step,
        max_steps,
        Mover::Walk(params),
    )
}

#[allow(clippy::too_many_arguments)]
fn walk_inner(
    world: &CollisionWorld,
    start: Vec3,
    heading: Vec3,
    target: Option<Vec3>,
    half: Vec3,
    step: f32,
    max_steps: usize,
    mover: Mover<'_>,
) -> Walk {
    let mut pos = start;
    let mut heading = heading;
    let mut blocked = false;
    let mut falling = false;
    let mut last_source = None;
    let mut contacts = Vec::new();
    let mut stuck = 0;
    let mut used = 0;
    for _ in 0..max_steps {
        used += 1;
        if let Some(t) = target {
            let h = heading_xz(pos, t);
            if h != [0.0, 0.0, 0.0] {
                heading = h;
            }
        }
        let delta = scale(heading, step);
        let r = mover.step(world, pos, delta, half);
        falling = r.falling;
        let before = pos;
        pos = r.position;
        if r.blocked
            && let Some(c) = r.contacts.last()
        {
            blocked = true;
            let progressed = (pos[0] - before[0]) * heading[0]
                + (pos[1] - before[1]) * heading[1]
                + (pos[2] - before[2]) * heading[2];
            // `last_source` names the surface that actually stopped forward progress.
            if progressed < 0.001 {
                last_source = Some(c.source);
            }
            contacts.extend(r.contacts);
        }
        let progress = before[0] * heading[0] + before[1] * heading[1] + before[2] * heading[2];
        let now = pos[0] * heading[0] + pos[1] * heading[1] + pos[2] * heading[2];
        if now - progress < 0.001 {
            stuck += 1;
        } else {
            stuck = 0;
        }
        if stuck >= 5 {
            break;
        }
    }
    Walk {
        position: pos,
        blocked,
        falling,
        steps: used,
        last_source,
        last_heading: heading,
        contacts,
    }
}

/// Scans every decoded class default for a property name containing "step" and reports it.
/// The doorway harness uses the upstream UE2 `MAXSTEPHEIGHT` only if there is no XIII
/// evidence; this makes that decision visible rather than assumed.
fn report_step_height_evidence(set: &ScriptSet) {
    let mut classes = 0usize;
    let mut hits: Vec<String> = Vec::new();
    for pkg in &set.packages {
        for (idx, obj) in &pkg.objects {
            let xiii_script::ScriptObject::Class(cl) = obj else {
                continue;
            };
            classes += 1;
            for prop in &cl.defaults.properties {
                let name = pkg.package.property_name(prop);
                if name.to_ascii_lowercase().contains("step") {
                    let path = pkg
                        .package
                        .object_path(xiii_package::ObjectRef::Export(*idx))
                        .unwrap_or("?");
                    hits.push(format!("{}.{}", pkg.name, path));
                }
            }
        }
    }
    hits.sort();
    hits.dedup();
    if hits.is_empty() {
        println!(
            "[collision-test] step-height evidence: {classes} class defaults scanned, no property name contains 'step'; using upstream UE2 MAXSTEPHEIGHT = {MAXSTEPHEIGHT_UU} UU (hypothesis)"
        );
    } else {
        println!(
            "[collision-test] step-height evidence: {classes} class defaults scanned; property names containing 'step' ({}) {}",
            hits.len(),
            hits.join(", ")
        );
    }
}

/// Prints the last contacts of a walk, with contact heights above the box bottom and the
/// floor-to-ceiling clearance at the stuck point (metres and Unreal units).
fn print_contacts(
    label: &str,
    world: &CollisionWorld,
    sources: &[String],
    walk: &Walk,
    half: Vec3,
) {
    if walk.contacts.is_empty() {
        return;
    }
    let box_bottom = walk.position[1] - half[1];
    for c in walk.contacts.iter().rev().take(3) {
        // Height of the contact triangle's centroid above the box's bottom at impact.
        let tri = world.triangle(c.triangle);
        let centroid_y = (tri[0][1] + tri[1][1] + tri[2][1]) / 3.0;
        let contact_bottom = c.position[1] - half[1];
        let height_above_bottom = centroid_y - contact_bottom;
        println!(
            "[collision-test]   {label} contact {} normal=({:.2},{:.2},{:.2}) at ({:.3},{:.3},{:.3}) m = ({:.1},{:.1},{:.1}) UU; contact triangle centroid {:.3} m = {:.1} UU above the box bottom",
            sources[c.source as usize],
            c.normal[0],
            c.normal[1],
            c.normal[2],
            c.position[0],
            c.position[1],
            c.position[2],
            c.position[0] * UNREAL_UNITS_PER_METER,
            c.position[1] * UNREAL_UNITS_PER_METER,
            c.position[2] * UNREAL_UNITS_PER_METER,
            height_above_bottom,
            height_above_bottom * UNREAL_UNITS_PER_METER
        );
    }
    // Clearance at the final (stuck) point: floor and ceiling rays from the box center.
    let p = walk.position;
    if let Some(f) = world.ray(p, add(p, [0.0, -20.0, 0.0])) {
        let floor_y = p[1] - f.t * 20.0;
        let headroom = p[1] - floor_y;
        let ceiling = world.ray(p, add(p, [0.0, 20.0, 0.0]));
        let ceil_text = match ceiling {
            Some(c) => {
                let above = c.t * 20.0;
                format!(
                    "ceiling {above:.3} m = {:.1} UU above; total {:.3} m = {:.1} UU",
                    above * UNREAL_UNITS_PER_METER,
                    headroom + above,
                    (headroom + above) * UNREAL_UNITS_PER_METER
                )
            }
            None => "no ceiling within 20 m".to_owned(),
        };
        println!(
            "[collision-test]   {label} stuck-point clearance: floor {:.3} m = {:.1} UU below box center; {ceil_text} (box is {:.3} m = {:.1} UU tall)",
            headroom,
            headroom * UNREAL_UNITS_PER_METER,
            2.0 * half[1],
            2.0 * half[1] * UNREAL_UNITS_PER_METER
        );
    }
    let _ = box_bottom;
}

/// Floor height (Bevy Y) directly below `p`, found by a downward ray from `p[1]`, or `None`.
pub(crate) fn floor_y_at(world: &CollisionWorld, p: Vec3, from_y: f32) -> Option<f32> {
    let start = [p[0], from_y, p[2]];
    let end = [p[0], from_y - 10.0, p[2]];
    world
        .ray(start, end)
        .filter(|h| h.normal[1] > 0.5)
        .map(|h| start[1] - h.t * 10.0)
}

/// The door-leaf plane normal: the bbox axis of smallest extent, oriented so that moving
/// from `start` toward `center` has a positive component (i.e. "past" is positive).
fn door_plane_normal(bbox: Box3, start: Vec3, center: Vec3) -> Vec3 {
    let ext = [
        bbox.1[0] - bbox.0[0],
        bbox.1[1] - bbox.0[1],
        bbox.1[2] - bbox.0[2],
    ];
    let axis = if ext[0] <= ext[1] && ext[0] <= ext[2] {
        0
    } else if ext[1] <= ext[2] {
        1
    } else {
        2
    };
    let mut n = [0.0f32; 3];
    n[axis] = 1.0;
    if (center[axis] - start[axis]) < 0.0 {
        n[axis] = -1.0;
    }
    n
}

// -------------------------------------------------------------------------------------------
// Small vector helpers
// -------------------------------------------------------------------------------------------

fn add(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] + b[0], a[1] + b[1], a[2] + b[2]]
}
fn sub(a: Vec3, b: Vec3) -> Vec3 {
    [a[0] - b[0], a[1] - b[1], a[2] - b[2]]
}
fn scale(a: Vec3, s: f32) -> Vec3 {
    [a[0] * s, a[1] * s, a[2] * s]
}

fn heading_xz(from: Vec3, to: Vec3) -> Vec3 {
    let d = [to[0] - from[0], 0.0, to[2] - from[2]];
    let l = (d[0] * d[0] + d[2] * d[2]).sqrt().max(1e-6);
    [d[0] / l, 0.0, d[2] / l]
}

/// Signed distance of `p` past the plane through `origin` with normal `heading`.
fn past_plane(p: Vec3, origin: Vec3, heading: Vec3) -> f32 {
    (p[0] - origin[0]) * heading[0]
        + (p[1] - origin[1]) * heading[1]
        + (p[2] - origin[2]) * heading[2]
}

fn pass_fail(ok: bool) -> &'static str {
    if ok { "PASS" } else { "FAIL" }
}

/// True when the map has an actor path containing the fragment (case-insensitive).
fn map_has_porte6(actors: &level::LevelActors) -> bool {
    actors
        .static_mesh_actors
        .iter()
        .any(|a| a.path.to_ascii_lowercase().contains("porte6"))
}

/// Full placement report for one static-mesh actor, from the imported `WorldScene`: every
/// effective placement value with its source (map / class default), the mesh's local bounding
/// box, its world bbox after the transform, and the heights of its walkable surfaces nearby.
fn report_static_mesh_actor(
    scene: &WorldScene,
    class_defaults: &mut xiii_world::ClassDefaults,
    a: &level::ActorPlacement,
    player_start: Vec3,
) {
    let prefix = format!("{} -> ", a.path);
    let objs: Vec<&xiii_world::SceneObject> = scene
        .objects
        .iter()
        .filter(|o| o.path.starts_with(&prefix))
        .collect();
    let Some(first) = objs.first() else {
        println!("[collision-test] StaticMeshActor286: not placed (skipped) in the scene");
        return;
    };
    let (eff, src) = match class_defaults.resolve(&a.class, a) {
        Ok(v) => v,
        Err(e) => {
            println!("[collision-test] StaticMeshActor286: class defaults unresolved: {e}");
            return;
        }
    };
    let tag = |s: level::PlacementSource| match s {
        level::PlacementSource::MapProperty => "map",
        level::PlacementSource::ClassDefault => "class-default",
        level::PlacementSource::EngineDefault => "engine-default",
    };
    println!("[collision-test] StaticMeshActor286 -> {0}", {
        let p = first.path.strip_prefix(&prefix).unwrap_or("?");
        p.to_owned()
    });
    println!(
        "[collision-test]   Location ({:.1},{:.1},{:.1}) UU [{}]; Rotation {:?} [{}]; DrawScale {:.3} [{}]; DrawScale3D {:?} [{}]; PrePivot {:?} [{}]",
        eff.location[0],
        eff.location[1],
        eff.location[2],
        tag(src[0]),
        eff.rotation,
        tag(src[1]),
        eff.draw_scale,
        tag(src[2]),
        eff.draw_scale_3d,
        tag(src[3]),
        eff.pre_pivot,
        tag(src[4]),
    );
    println!(
        "[collision-test]   flags bCollideActors={:?} bBlockActors={:?} bBlockPlayers={:?} bStaticMeshActor-hidden={}",
        a.collision_flags[0], a.collision_flags[1], a.collision_flags[2], a.hidden
    );
    // Local/world bounds of the mesh (all placed sections share one transform; bounds of the
    // union over sections).
    let ident = |m: &xiii_world::SceneMesh| {
        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
        for p in &m.positions {
            for k in 0..3 {
                lo[k] = lo[k].min(p[k]);
                hi[k] = hi[k].max(p[k]);
            }
        }
        (lo, hi)
    };
    let t = first.transform;
    let mut world_lo = [f32::MAX; 3];
    let mut world_hi = [f32::MIN; 3];
    for o in &objs {
        let (lo, hi) = ident(&scene.meshes[o.mesh]);
        for p in [&lo, &hi] {
            let w = xiii_world::apply_transform_pub(&t, *p);
            for k in 0..3 {
                world_lo[k] = world_lo[k].min(w[k]);
                world_hi[k] = world_hi[k].max(w[k]);
            }
        }
    }
    let (mesh_lo, mesh_hi) = ident(&scene.meshes[first.mesh]);
    let uu = |v: [f32; 3]| -> [f32; 3] {
        [
            v[0] * UNREAL_UNITS_PER_METER,
            v[1] * UNREAL_UNITS_PER_METER,
            v[2] * UNREAL_UNITS_PER_METER,
        ]
    };
    println!(
        "[collision-test]   mesh bbox (converted mesh-space, m) min {:?} max {:?} = UU {:?}..{:?}",
        mesh_lo,
        mesh_hi,
        uu(mesh_lo),
        uu(mesh_hi)
    );
    println!(
        "[collision-test]   world bbox (after placement incl. PrePivot) m min {:?} max {:?} = UU {:?}..{:?}",
        world_lo,
        world_hi,
        uu(world_lo),
        uu(world_hi)
    );
    // "Before the fix": the legacy transform (map property, else hardcoded engine defaults;
    // no PrePivot). Identical when every placement property is present and PrePivot is zero.
    let legacy_scale = [
        a.draw_scale_3d.unwrap_or([1.0; 3])[0] * a.draw_scale.unwrap_or(1.0),
        a.draw_scale_3d.unwrap_or([1.0; 3])[1] * a.draw_scale.unwrap_or(1.0),
        a.draw_scale_3d.unwrap_or([1.0; 3])[2] * a.draw_scale.unwrap_or(1.0),
    ];
    let legacy = actor_to_bevy(
        a.location.unwrap_or([0.0; 3]),
        a.rotation.unwrap_or([0; 3]),
        legacy_scale,
    );
    let mut legacy_lo = [f32::MAX; 3];
    let mut legacy_hi = [f32::MIN; 3];
    for o in &objs {
        let (lo, hi) = ident(&scene.meshes[o.mesh]);
        for p in [&lo, &hi] {
            let w = xiii_world::apply_transform_pub(&legacy, *p);
            for k in 0..3 {
                legacy_lo[k] = legacy_lo[k].min(w[k]);
                legacy_hi[k] = legacy_hi[k].max(w[k]);
            }
        }
    }
    println!(
        "[collision-test]   world bbox BEFORE fix (map values, no PrePivot/class-defaults) m min {:?} max {:?} = UU {:?}..{:?}",
        legacy_lo,
        legacy_hi,
        uu(legacy_lo),
        uu(legacy_hi)
    );
    // Walkable surfaces: upward-facing collision triangles of this actor, grouped by floor
    // height (Bevy Y), with the fraction of area near the PlayerStart XY.
    let mut surfaces: Vec<(f32, f32)> = Vec::new(); // (u UU, area m^2)
    for (tris, src) in &scene.collision {
        let src_path = &scene.collision_sources[*src as usize];
        if !src_path.starts_with(&prefix) {
            continue;
        }
        let n = (tris[0][1] + tris[1][1] + tris[2][1]) / 3.0;
        let e1 = [
            tris[1][0] - tris[0][0],
            tris[1][1] - tris[0][1],
            tris[1][2] - tris[0][2],
        ];
        let e2 = [
            tris[2][0] - tris[0][0],
            tris[2][1] - tris[0][1],
            tris[2][2] - tris[0][2],
        ];
        let nvec = [
            e1[1] * e2[2] - e1[2] * e2[1],
            e1[2] * e2[0] - e1[0] * e2[2],
            e1[0] * e2[1] - e1[1] * e2[0],
        ];
        let len = (nvec[0] * nvec[0] + nvec[1] * nvec[1] + nvec[2] * nvec[2]).sqrt();
        if len < 1e-9 || nvec[1] / len < 0.5 {
            continue;
        }
        let d = ((tris[0][0] - player_start[0]).powi(2) + (tris[0][2] - player_start[2]).powi(2))
            .sqrt();
        if d > 4.0 {
            continue;
        }
        surfaces.push((n * UNREAL_UNITS_PER_METER, len / 2.0));
    }
    surfaces.sort_by(|x, y| x.0.total_cmp(&y.0));
    let merged: Vec<(f32, f32)> = {
        let mut out: Vec<(f32, f32)> = Vec::new();
        for (u, area) in surfaces {
            if let Some(last) = out.last_mut()
                && (u - last.0).abs() < 1.0
            {
                last.1 += area;
                continue;
            }
            out.push((u, area));
        }
        out
    };
    for (u, area) in &merged {
        println!(
            "[collision-test]   walkable surface at {:.1} UU (PlayerStart XY within 4 m): area {:.0} UU^2, {:.1} UU above the BSP floor below the PlayerStart (1184.0), {:+.1} vs PlayerStart.Z (1196.0)",
            u,
            area * UNREAL_UNITS_PER_METER * UNREAL_UNITS_PER_METER,
            u - 1184.0,
            u - 1196.0
        );
    }
}
