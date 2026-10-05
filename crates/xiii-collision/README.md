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
  (XIII default) is `(0.68, 1.5, 0.68)` m at 50 units/m.

## API

| Function | Behaviour |
|---|---|
| `CollisionWorld::new(entries)` | Retains non-degenerate triangles; builds the BVH once; counts dropped degenerate triangles. |
| `sweep(start, end, half_extents)` | Continuous swept AABB. `SweepHit { t in [0,1], unit normal toward the box, triangle, source, start_penetrating }`. |
| `ray(start, end)` | Nearest hit along a segment, both sides, via Möller-Trumbore over BVH candidates; result-equivalent to the viewer's per-triangle `ray_triangle`. (The swept SAT is not used for zero extent: at an exact point its edge-axis cross products can be near-degenerate and reject a coplanar triangle.) |
| `overlap_aabb(center, half_extents)` / `overlaps_aabb` | Static SAT overlap list / boolean. |
| `move_slide(start, delta, half_extents, &MoveParams)` | Sweep, back off by `skin`, slide along the hit plane, up to `max_iterations`; optional three-sweep step-up of `max_step_height`. Returns final position, `blocked`, ordered `MoveContact`s, `iterations`, `on_floor`. |

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

## Approximations (not fidelity claims)

`move_slide` is a simple approximation of `UPawn::physWalking` / `stepUp`: one swept AABB
against two-sided triangles, an empirical 1 mm skin, and a step-up heuristic. It is fit for a
doorway pass/block test; it does not reproduce UE2's cylinder/contact ordering, penetration
resolution, or per-step friction/acceleration.

## Tests (synthetic geometry only, no game data)

Box into a wall stops at the right `t`; a 1% wider corridor passes and a 1% narrower one
blocks; slide keeps tangential motion; step-up below/above the limit; a resting box moves
horizontally; a box flush/inside a wall moving into it blocks at `t=0` while moving away or
parallel does not; a resting box moving down is blocked at `t=0`; 1000 shallow-angle slide
steps never reach the far side of a wall; a zero-extent downward ray onto a coplanar floor
hits; start-overlap and zero-length sweeps; degenerate triangles dropped; BVH equals brute
force on a randomized soup.
