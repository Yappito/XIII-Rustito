# xiii-collision

Filesystem-free collision queries against a decoded world triangle soup. No dependencies,
no Bevy, `unsafe` forbidden. Input is triangles in Bevy space (metres, Y up) with a `u32`
source id each; the crate knows nothing about packages or the installation.

This closes part of the M2a exit: it replaces the viewer's brute-force ray probe with a real
swept primitive and a broad phase, and provides the movement helper used by the headless
`--collision-test` doorway probe.

## Primitive and evidence

UE2 (upstream engine) moves pawns against world geometry with an axis-aligned **extent box**
of half-size `(CollisionRadius, CollisionRadius, CollisionHeight)` in Unreal units; the
cylinder is used for actor-vs-actor. The extracted world collision is a triangle soup
(static-mesh collision set 0, BSP solid node polygons, terrain quads) with no reliable
per-triangle facing, so:

- the swept query is **AABB vs triangle** via the separating-axis theorem (13 axes: 3 box
  axes, the triangle normal, 9 box-edge x triangle-edge cross products; near-zero axes are
  skipped);
- triangles are **two-sided**; the reported contact normal is oriented to oppose the sweep
  direction, not taken from the winding;
- the source half-extents in Bevy space are `(R, H, R) / units_per_metre`, so a 34/75 pawn
  (XIII default) is `(0.38, 0.83, 0.38)` m at 90 units/m (the project's approved scale).

## API

| Function | Behaviour |
|---|---|
| `CollisionWorld::new(entries)` | Retains non-degenerate triangles; builds the BVH once; counts dropped degenerate triangles. |
| `sweep(start, end, half_extents)` | Continuous swept AABB. `SweepHit { t in [0,1], unit normal toward the box, triangle, source, start_penetrating }`. |
| `ray(start, end)` | Nearest hit along a segment, both sides, via Möller-Trumbore over BVH candidates; result-equivalent to the viewer's per-triangle `ray_triangle`. (The swept SAT is not used for zero extent: at an exact point its edge-axis cross products can be near-degenerate and reject a coplanar triangle.) |
| `overlap_aabb(center, half_extents)` / `overlaps_aabb` | Static SAT overlap list / boolean. |
| `move_slide(start, delta, half_extents, &MoveParams)` | Sweep, back off by `skin`, slide along the hit plane, up to `max_iterations`; optional three-sweep step-up of `max_step_height`. Returns final position, `blocked`, ordered `MoveContact`s, `iterations`, `on_floor`, `falling` (always `false`). |
| `walk_move(start, delta, half_extents, &WalkParams)` | UE2-style walking: on a blocking hit whose surface is **not walkable** (normal up component below `min_floor_z`), step up by `max_step_height`, forward, then down; otherwise slide as `move_slide`. Afterwards floor-follow down by `max_step_height` and snap to a walkable floor, else report `falling`. |

Start-overlap is reported as a `t = 0` hit with `start_penetrating = true` and never panics.
By default, a triangle already touching at `t = 0` is skipped **only when the motion does not
drive into it** (`dot(d, n_plane) >= -1e-6`, with the plane normal oriented from the triangle
toward the box start). So a resting pawn ignores the floor it stands on and a box sliding
along a wall it touches is not caught, but a box that is flush with (or a hair inside) a wall
and moves into it is blocked at `t = 0` rather than tunnelling through.

## Broad phase

A flat binary BVH (median split on the longest centroid axis, leaves of up to 8 triangles) is
built once per world. `traverse` visits only nodes whose bounds overlap the swept query box,
so queries are not O(all triangles). A deterministic-LCG test asserts the BVH sweep equals a
brute-force oracle over 500 random queries on a 400-triangle soup.

## Engine evidence (item1j, `Engine.dll`, read-only)

Disassembled (`llvm-objdump`, raw in `local/re/`) and read against the engine's own
`APawn::physWalking` (`0x103bdac0`), `APawn::stepUp` (`0x103baa30`), `AActor::stepUp`
(`0x103bb0a0`), `ULevel::MoveActor` (`0x1038a770`), `ULevel::SingleLineCheck`/`MultiLineCheck`,
`UModel::LineCheck` (`0x10419b80`), `UStaticMesh::LineCheck` (`0x10402e00`) and
`ATerrainInfo::LineCheck` (`0x10409eb0`):

- The pawn moves an **extent box** `(R,R,H)`, confirmed: `MoveActor` receives an extent vector
  and `stepUp` multiplies the step vector by the fixed up magnitude `35.0` UU.
- `MINFLOORZ = 0.7` (`0x10483428`) is the walkability threshold. The branch at `0x103be058`
  tests the **horizontal `MoveActor` contact normal** (`FCheckResult.Normal.Z` at struct offset
  `0x14`; `Time` at `0x24`) with `fld Normal.Z; fcomp 0.7; test ah,0x5; jp`: the parity test is
  taken when `Normal.Z > 0.7`, so a **walkable** contact enters `0x103be547` and an **unwalkable**
  one falls through to `0x103be06c`; which body is the stair-step and which the slope-walk is
  **hypothesis**. The `0x104831ac` = 37 UU constant is **measured**, but no branch where its
  result gates `stepUp` was located, so the crate's floor gate is a measurement-chosen
  **approximation (hypothesis)**.
- `physWalking` iterates at most `8` sub-steps (`cmpl $0x8`, `0x103bde14`) and uses the
  `1.9`/`2.4` UU constants (`0x10483420`/`0x10483424`) beside its floor snap. Our harness's
  `0.05` UU skin is empirical, not an engine constant (`stepUp` uses no separate skin).
- Every geometry `LineCheck` (`UModel` BSP, `UStaticMesh`, `ATerrainInfo`) is a **segment with
  extent**, not a swept box: the extent expands the endpoint box. `ATerrainInfo::LineCheck`
  clamps to the base heightmap and indexes `Vertices[HeightmapX*y + x]` (base grid only).

## Approximations (not fidelity claims)

`move_slide` is a simple approximation of `APawn::physWalking` / `stepUp`: one swept AABB
against two-sided triangles, an empirical 1 mm skin, and a step-up heuristic gated on a
near-vertical contact normal (`|n_y| < 0.3`). A near-horizontal small floor rise reported
through a near-horizontal contact therefore never triggers it.

`walk_move` keeps the measured branch structure of `APawn::physWalking` (item1k) and adds
host approximations:

1. sweep the full delta; on no hit, move and finish;
2. on a hit, back off by `skin`; run a downward **floor probe** of
   `max_step_height * 37/35` (the 37 UU constant is measured, but the gate is a
   **measurement-chosen approximation**, not a proven engine branch) and, when a walkable floor
   is under the pawn, try the three-sweep step-up — up by `max_step_height`, forward by the
   travel, then down by the raise plus `max_step_height`. Two **host approximations** in the
   step: the up and forward sweeps ignore a walkable floor the box is already touching
   (`SweepParams::ignore_resting_floor_z`), and the down-sweep spans the raise plus
   `max_step_height`; together they stop a grazing near-horizontal floor from defeating the step
   (the walkable-ledge stall). `physWalking`'s own measured branch keys on the horizontal
   `MoveActor` contact normal; attempting the step on any block with a walkable landing covers
   both bodies. If the step fails, slide along the hit plane exactly as `move_slide`;
3. after the move, floor-follow: sweep down by `max_step_height` and snap to a walkable floor,
   else set `falling` (no gravity is applied; the caller stops and reports).

`ULevel::FindSpot` (`0x1038a080`, item1k) is ported as `find_spot`: the engine's four corner
candidates at `(±0.5*Extent.X, ±0.5*Extent.Y)` with single-candidate extrapolation to twice the
offset, then a caller-visible settle onto a walkable floor. The previous vertical raise is an
explicit, disabled-by-default fallback for embedded boxes.

Both retain UE2's extent-box primitive and the empirical 1 mm skin; neither reproduces UE2's
cylinder/contact ordering, penetration resolution, the `8`-sub-step loop, or per-step
friction/acceleration. The engine's `LineCheck`s are segment-with-extent, not swept; `walk_move`
deliberately uses a continuous swept box because the reach harness needs no-tunnelling, and the
difference is reported, not hidden.

## Tests (synthetic geometry only, no game data)

Box into a wall stops at the right `t`; a 1% wider corridor passes and a 1% narrower one
blocks; slide keeps tangential motion; step-up below/above the limit; a resting box moves
horizontally; a box flush/inside a wall moving into it blocks at `t=0` while moving away or
parallel does not; a resting box moving down is blocked at `t=0`; 1000 shallow-angle slide
steps never reach the far side of a wall; a zero-extent downward ray onto a coplanar floor
hits; start-overlap and zero-length sweeps; degenerate triangles dropped; BVH equals brute
force on a randomized soup.

`walk_move`: over a 1.8 cm plank edge; up a 14-degree ramp; a step just below the limit passes
and just above is blocked; walking off a ledge reports `falling`; a corridor floor of 0.5 m
tiles with randomized sub-2 mm seams never sticks or falls through (deterministic LCG); 1000
small steps into a wall never tunnel through it; a series of walkable plank seams and a
walkable bevel are crossed; a walkable ledge under a low overhang steps through.

`find_spot` (`ULevel::FindSpot` port): a free centre is used and settled; a zero extent is
rejected; no floor is `NoFloor`; an embedded box raises when enabled and is `NoFreeSpot` when
not; a single free corner extrapolates to twice its offset.
