# xiii-world

Bevy-free map import and the Unreal-space physics adapter. This crate turns a map from an
owned installation into converted (Bevy-space) meshes, RGBA textures, placed objects and a
collision triangle soup, and implements `xiii_script::physics::WorldPhysics` on top of that
collision. `xiii-app` keeps only the Bevy-specific conversion (spawning meshes, materials and
the camera) and depends on this crate.

Dependencies: `xiii-package`, `xiii-install`, `xiii-decode`, `xiii-script`, `xiii-collision`.
No Bevy, no windowing. `unsafe` is forbidden workspace-wide.

## Import (`import_map`, `PackageCache`)

`PackageCache` opens an installation read-only (`xiii-install`) and resolves packages,
maps and imports by logical name, case-insensitively, caching parsed packages and following
imports into their root package. `import_map(&mut cache, "Plage01")` returns a `WorldScene`:

- `meshes` / `textures` / `objects`: render-ready geometry (positions, normals, UVs, triangle
  indices, a material slot), mip-0 RGBA8 textures and one `SceneObject` per placed mesh
  section with its [`xiii_decode::common::BevyTransform`].
- `player_start`: the first PlayerStart's Bevy-space position and Unreal rotator.
- `collision` / `collision_sources`: the world triangle soup in Bevy space (metres, Y up) with
  a source id per triangle. Static meshes contribute their decoded collision set 0, the BSP its
  solid node polygons (invisible walls included), the terrain its base-region quads.
  A `TerrainInfo` whose `Vertices` array is longer than `HeightmapX * HeightmapY` stores trailing
  editor grids after the base heightfield (see `xiii_decode::terrain`); only the base grid is the
  engine's terrain (`ATerrainInfo::LineCheck`/`Render` in `Engine.dll` index it), so the importer
  ignores the trailing vertices and counts them as `note.terrain.extra_vertices_ignored` /
  `terrain.region_extra_vertices_ignored`. Treating them as extra geometry regressed the Hual01b
  reach walk.
- `counters` / `examples`: every import outcome, including `skip.*`, `fail.*` and `note.*`
  categories. Nothing is dropped silently; [`WorldScene::problem_total`] sums the `skip.`/
  `fail.` counters.

Actor placement uses **effective** values: the map's tagged property if present, else the
inherited class default resolved read-only through `xiii_script` (`Vm::class_layout`), else the
documented `Engine.Actor` default (Location 0, Rotation 0, DrawScale 1, DrawScale3D 1,
PrePivot 0). The source of every field is counted as `placement.<field>.<source>` and
`actor.player_start.<field>.<source>`. `PrePivot` is applied before scale/rotation
(`xiii_decode::common::actor_to_bevy_pre_pivot`). Materials are followed through
Shader/FinalBlend/Tex*/SinusModifier/Combiner down to a texture; an unresolved material is an
explicit `skip.mesh.material (...)` counter and a `MaterialSlot::Missing`.

This is the import that the diagnostic map viewer (`xiii-app`) renders; it is not a playable
mode, and no class defaults are applied to actors beyond placement.

### Baked vertex lighting

Each `SceneObject` carries an optional `colors: Vec<[u8; 4]>` (RGBA). For a placed
static-mesh actor the importer decodes the actor's `Engine.StaticMeshInstance` export
(`xiii_decode::static_mesh_instance`), checks the colour count against the mesh's vertex count,
swaps the stored BGRA bytes to RGBA and attaches one colour per render vertex. Because the
lighting is per placed actor, the viewer builds a private mesh per coloured object; the shared
asset mesh stays uncoloured. Counters: `lighting.instances.decoded`, `lighting.objects.lit`
/ `unlit` / `mismatch`, `lighting.colors.rgba`. An all-zero (`[0,0,0,0]`) instance is `unlit`
and left uncoloured rather than modulated to black. Terrain sectors contribute
`lighting.terrain.colors` through `xiii_decode::terrain::color_grid`; shared sector borders
that agree are not conflicts.

## Physics adapter (`physics::WorldPhysicsAdapter`)

`xiii_script::physics::WorldPhysics` speaks Unreal units and axes (X forward, Y right, Z up);
`xiii-collision` works in Bevy metres (Y up). The adapter converts every crossing with the
single coordinate policy in `xiii_decode::common` — `to_bevy_position` / `to_bevy_scale` and
`UNREAL_UNITS_PER_METER` for the forward direction, and the documented inverses
(`bevy_to_unreal_position`, `bevy_to_unreal_direction`, `unreal_extent_to_bevy`) for the
result — so the scale constant and axis mapping are never duplicated.

- `trace(start, end, extent)`: swept AABB (or a ray when `extent` is zero) from `start` to
  `end`, returning the first hit as an Unreal-space `WorldHit`.
- `move_box(start, delta, extent)`: UE2 `MoveActor`-like move that stops at the first blocking
  hit; no sliding (sliding is script/physics logic above it).
- `point_free(location, extent)`: placement overlap test.

`WorldPhysicsAdapter::from_scene(&WorldScene)` builds it from an imported map;
`from_entries` takes raw `(triangle, source id)` pairs. The adapter is deliberately **not**
wired into the VM (`xiii-tool`) yet.

## Tests

- Unit: coordinate round trips (position, direction, extent), a downward trace onto a
  synthetic floor, an extent trace into a wall, `move_box`/`point_free` against a tiny soup.
- Opt-in corpus (`XIII_GOG_DIR`, prints `SKIPPED` otherwise): a downward trace in Unreal space
  from the Plage01 PlayerStart hits the floor at the measured Unreal Z 1184.0.

## References

Upstream `UPawn::physWalking` / `stepUp` and the `MINFLOORZ` floor test are the basis for the
walking approximation in `xiii-collision`; the coordinate policy is documented in
`crates/xiii-decode/src/common.rs`. No proprietary bytes are stored in this crate or its
fixtures.
