# xiii-decode

Payload decoders that turn XIII export payloads into normalized, renderer-independent assets.
Each section below is maintained by the module's owner.

## Skeletal meshes and animation

Module `skeletal` (M2b). It decodes `Engine.SkeletalMesh` and `Engine.MeshAnimation` exports
into field-for-field raw structures (`mesh::RawSkeletalMesh`, `anim::RawMeshAnimation`), then
into `normalize::{Skeleton, SkinnedMesh, AnimSet/Clip}`. It also evaluates poses
(`evaluate_pose`) and applies linear-blend skinning (`SkinnedMesh::skin`). Every decoder must
consume the payload exactly. Leftover bytes are reported as `TrailingBytes` with the offset of
the unsupported tail. A layout variant never seen in the corpus is reported as `Unsupported`,
an index violation as `Invalid`. Errors carry the class, export index, field and absolute file
offset. The module has no new dependencies. Tool: `xiii-tool anim coverage|list|validate|render|export`.

### Coverage (GOG, measured 2026-10-04/05, `xiii-tool anim coverage XIII_Game`)

| Class | Instances | Decoded exactly | Packages |
|---|---|---|---|
| Engine.SkeletalMesh | 129 | 129 | xiiipersos 74, xiiiarmes 38, xiiideco 14, xiiipersosG 2, xiiivehicule 1 |
| Engine.MeshAnimation | 96 | 96 | xiiipersos 41, xiiiarmes 38, xiiideco 14, xiiipersosG 2, xiiivehicule 1 |
| Engine.VertMesh | 3 (xiiideco) | not attempted | |

All instances are licensee 58. The corpus holds 1,085 sequences and 294,533 rotation keys. All
96 animations pass the structure checks: track count equals the bone count; key times are
strictly increasing, start at 0 and fall in `0..TrackTime`; position counts are 1 or equal to
the rotation count. All 129 meshes give one root, weight sums of 1 and a bind-pose skinning
error below 1e-2. These checks are an opt-in test: `skeletal::tests::local_corpus_skeletal`
with `XIII_GOG_DIR` set.

### SkeletalMesh layout (version 100, ULodMesh version 1, after the property block)

Order follows UModel `UnMesh2.cpp`/`UnMesh2.h` (UPrimitive, `ULodMesh::Serialize`, the
`USkeletalMesh::Serialize` "Version <= 1" path). UModel's XIII path stops reading after the
collision fields (`goto skip_remaining`). The layout below was verified byte-exactly on all 129
instances.

| Field | Encoding | Observed |
|---|---|---|
| BoundingBox | 6 f32 + u8 valid | |
| BoundingSphere | 4 f32 | |
| XIII UPrimitive extension | 4 bytes + f32 (UModel: licensee >= 19) | `01 01 00 00`, 1.0 |
| Version, VertexCount | i32, i32 | Version 1 everywhere |
| Verts | TArray<u32> | empty |
| legacy FMeshTri2 (Version <= 1) | TArray of 38 B | empty |
| Textures | TArray<compact object ref> | |
| MeshScale, MeshOrigin, RotOrigin | 3 f32, 3 f32, 3 i32 | e.g. XIIIM (1,1,1), (0,0,80.15), yaw 49152 |
| legacy u16 array (Version <= 1) | TArray<u16> | length = VertexCount |
| FaceLevel, Faces, CollapseWedgeThus | TArray<u16>, TArray<{u16 x3 wedge, u16 mat}>, TArray<u16> | |
| Wedges | TArray<{u16 point, f32 u, f32 v}> (10 B) | |
| Materials | TArray<{u32 polyflags, i32 texture index}> | |
| MeshScaleMax, LODHysteresis, LODStrength, LODMinVerts, LODMorph, LODZDisplace | f32, f32, f32, i32, f32, f32 | |
| Points | TArray<FVector> | count = VertexCount |
| RefSkeleton | TArray<FMeshBone> | 3 to 48 bones |
| Animation | compact object ref | e.g. XIIIM -> import `XIIIPersosG.MigA` |
| SkeletalDepth | i32 | |
| WeightIndices | TArray<{TArray<u16> points, i32 start}> | group g = points with g+1 influences |
| BoneInfluences | TArray<{u16 weight/65535, u16 bone}> | read sequentially by the groups |
| AttachAliases, AttachBoneNames, AttachCoords | TArray<name>, TArray<name>, TArray<4 FVector> | only GiM has 2 sockets |
| legacy FLODMeshSection | TArray of 9 x u16 | one per material |
| second legacy section array | TArray (count only) | always empty; non-empty is `Unsupported` |
| **XIII hit boxes** | TArray<{FBox (25 B), u16 bone}> | humans: 2 (head ±11, spine ±38); animals: 0 to 1 |

Differences from UModel, measured on the bytes:

- `FMeshBone` has **no `NumChildren`**. A bone is a compact name, u32 flags, f32 quat
  (x,y,z,w), FVector position, f32 length, 3 f32 size and i32 parent: 53 bytes with a one-byte
  name. Reading UModel's 57-byte form misaligns at bone 1. The root stores parent 0 (itself).
- The third legacy array (`TArray<u16>` in UModel) is not there. The bytes after the second
  (empty) section array form the box array above.
- The hit-box bone field is two bytes, decoded as `u16`. A compact index followed by a zero
  byte would read the same for indices below 64, the only values observed.
- One mesh (`xiiipersos.u` SlaterM) has 10 influences on bone 0xFFFF, with weights of 2.8% or
  16.7%. They are dropped, the remaining weights renormalized and the count reported
  (`InfluenceStats::invalid_bone_influences`, shown in `anim coverage`). The original engine's
  handling is unknown.

### MeshAnimation layout

The header, sequences and notifies follow UModel `UMeshAnimation`/`FNamedBone`/`FMeshAnimSeq`/
`FMeshAnimNotify`. Version 100 is below 112 (no notify object) and below 115 (no leading
float). **UModel does not support the XIII `MotionChunk`.** The packed layout below was worked
out from the bytes and verified on all 96 instances:

```text
i32 Version (0); TArray<{name, u32 flags, i32 parent}> RefBones;
TArray<MotionChunk> Moves;            // Moves[i] belongs to AnimSeqs[i]; counts equal everywhere
TArray<{name, TArray<name> groups, i32 start, i32 frames, TArray<{f32 time, name}> notifies, f32 rate}> AnimSeqs;
MotionChunk:
  FVector RootSpeed3D (always 0); f32 TrackTime (== frames); i32 StartBone (0); u32 Flags (0);
  TArray<i32> BoneIndices (identity);
  TArray<{u16 PosStart, u16 QuatStart, u16 QuatCount, u16 PosCount}> Tracks;  // one per RefBone
  TArray<{i16 x, y, z, w}> Quats;    // component / 32767; norms ~1 (e.g. 0.469,-0.709,-0.439,-0.290)
  TArray<FVector> Positions;
  TArray<f32> QuatTimes;             // frames, one per quat; QuatTimes.len == Quats.len everywhere
```

Evidence: the starts are contiguous (`QuatStart[k+1] = QuatStart[k] + QuatCount[k]`, the same
for positions), and the summed counts equal the array lengths in every chunk. PosCount is 1
(constant) or equal to QuatCount, so positions share the rotation key times. There is no
separate root track and no other compression scheme. Key times run from 0 to `frames - 1`,
the rate is 30 in the inspected sets, and notify times are normalized 0..1. Eight notifies fall
outside 0..1, for example `MigA.TurnLPatrouille1Pistol` at 2.33; they are kept and reported.

### Conventions (measured)

- **Rotation storage:** the root bone's stored quaternion is used as is. Every other bone's
  stored quaternion is **conjugated**, for mesh bones and animation keys alike (animation
  frame-0 keys match the mesh bind quaternions under the same rule). Evidence: with this rule
  the XIIIM bind pose is an upright T-pose. Its feet are at z ≈ 0 to 10, the head joint at
  145 and the hands at x = ±60, z = 131, inside the reference point cloud (x ±77, z 0 to 160).
  The toes point +Y, matching the toe vertices (y up to +18). Unchanged quaternions fold the
  legs sideways. Conjugating the root as well turns the skeleton 180° against the mesh.
- **Sampling:** rotations use slerp along the shortest arc, positions use lerp. Looping clips
  interpolate from the last key (`frames - 1`) back to key 0 over the final frame. This is a
  hypothesis that has not been compared with the original. Track positions replace bind
  positions; there is no retargeting. Shared sets such as MigA (48 bones) drive meshes with
  27 to 38 bones by name.
- **Winding:** in a right-handed reading of the Unreal coordinates, faces are clockwise seen
  from outside. Computed normals use the reversed cross product; 74 to 94% of vertex normals
  point away from the centroid (XIIIM, AmosM, DogM, ratM).
- **Coordinates:** everything stays in source units (Z up). `MeshScale/MeshOrigin/RotOrigin`
  are kept, not baked. Conversion to runtime coordinates is the world module's policy
  (`docs/DESIGN.md`).

### Validation (`xiii-tool anim validate --game-dir XIII_Game --package xiiipersos --mesh XIIIM --seq ...`)

- XIIIM: 31 bones, 1 root, depth 10, joint height 150.5, mesh bounds z −0.1 to 160.2. 868
  vertices, 1,012 triangles, no degenerate triangles, up to 3 influences, weight sum
  1.0000/1.0000, bind skinning error 6e-5. All 31 bones have MigA tracks; all 175 MigA clips
  pass the structure checks.
- WaitNeutre (idle, 61 frames): bounds stay within ±0.3 units; the left foot does not move.
- Walk (31 frames, notifies `PlayFootStep` at 0.168/0.668): every frame finite. The largest
  bounds diagonal is 0.85 of the bind diagonal, and the lowest point stays at z −0.9 to 0.1.
  The left ankle moves +26.7 → −34.4 → +26.7 in Y. It is planted (z ≈ 10.5 to 13) while moving
  back and lifted (z up to 24.4) while swinging forward: one stride per cycle with an in-place
  root. The last key differs from frame 0 by at most 4.3 units, so the loop closes.
- Run (21 frames): the foot lifts to z 56.7 during the back kick and the lean is visible;
  bounds ratio 0.88.

Wireframe sheets (`xiii-tool anim render`, not committed) were reviewed: XIIIM bind/idle/walk/
run, DogM walk and fpsberrettaM fire. The human and the dog have connected limbs, no stray
triangles and believable gait phases. The first-person Beretta rig is flat (depth 2, every bone
parented to a distant camera-space root). Its mesh pieces stay intact, but the motion is too
small at that scale to judge.

### Not established yet

- Comparison against the original game's frames. M2b's exit criterion needs reference captures.
- How UE2 blends the last key back to the first on looping. `TrackTime` equals the frame
  count, while the last key sits at `frames - 1`.
- Root motion: RootSpeed3D is always 0 and walk/run are in place. Movement probably comes
  from script or physics; unverified.
- `MeshScale`/`RotOrigin` application order for the actor transform; `Engine.VertMesh` (3
  instances).
- Meaning of `FMeshBone.flags` (0/1, set on face bones), of the XIII UPrimitive bytes and of the
  `FLODMeshSection` fields.

### References

Field order was cross-checked against UModel/UEViewer (MIT) at
`gildor2/UEViewer@a0bfb468d42be831b126632fd8a0ae6b3614f981`: `Unreal/UnrealMesh/UnMesh2.cpp`
(sha256 `c1c2778392b39aad168fe7a31893ea9f64b90e34945868d00b7bd948b373df46`) and `UnMesh2.h`
(sha256 `8557058200be31a6bd133e0a9bbd912b06360475dd10f7fcf085f311eb38dfcb`). The local copies
are in the ignored `.research/`. `FMeshBone`/`VJointPos` come from UModel's `UnMesh.h`, which
was not downloaded; their field list here was established from the bytes. No code was copied.
The Rust implementation and the XIII `MotionChunk` layout are this project's own reading of the
data. No clean-room claim is made.

Candidates to share with `common` later: the bounded `Reader` (counts checked against the
remaining bytes, name/object resolution, exact-end check) and the `Vec3/Quat/Transform` math.

## World and collision (M2a)

Modules `common`, `texture`, `static_mesh`, `model` (BSP, `Polys`, `model::level` actor
placements) and `terrain`. Every decoder reads through `common::PayloadReader`, which cannot
read past the export's serialized size. It returns either an exact-consumption result or an
error with class, export, field and absolute/payload-relative offset. Classes whose remaining
data is deliberately not decoded return `PayloadReport::unsupported_tail` (label + byte range)
instead of dropping bytes. Bytes with a verified size but unknown meaning are listed in
`PayloadReport::unknown`. Errors are boxed (`DecodeError(Box<DecodeErrorData>)`).
Tool: `xiii-tool world-coverage|texture|mesh|bsp|terrain`. Viewer: `xiii-app --map`.

References: UModel (`gildor2/UEViewer@a0bfb468d42be831b126632fd8a0ae6b3614f981`, MIT) for
`UTexture`/`FMipmap`/`TLazyArray` (`UnTexture2.cpp`, `UnMaterial2.h`, `UnCoreSerialize.cpp`),
the XIII UPrimitive extension (`UnMesh2.h`) and the `FStaticMeshTriangle` layout
(`UnMesh2.cpp`). UModel rejects static meshes below version 112 and has no XIII BSP or terrain
reader. Those layouts were established from the GOG bytes described below. No reference code
was copied. The rotator matrix is the standard Unreal formula (roll about X, pitch about Y, yaw
about Z).

### Coverage (GOG, measured 2026-10-05, `xiii-tool world-coverage XIII_Game`)

| Class | Exports | Decoded | Exact to payload end | Notes |
|---|---:|---:|---:|---|
| Engine.Texture | 3,507 | 3,507 | 3,507 | every mip of every format decodes to RGBA8; 3 trailing bytes per texture of unknown meaning |
| Engine.Palette | 14 | 14 | 14 | |
| Engine.StaticMesh | 6,128 | 6,128 | 6,128 | 61,280 B of unknown-meaning bytes (XIII UPrimitive extension + 2-byte block) |
| Engine.Polys | 7,194 | 7,194 | 7,194 | 34,395 polygons |
| Engine.TerrainSector | 2,208 | 2,208 | 2,208 | |
| Engine.StaticMeshInstance | 14,542 | 14,542 | 14,542 | per placed static-mesh actor; colours + per-light vertex visibility |
| Engine.Model | 7,194 | 7,194 | 6,396 | full payload decoded on 6,396; 1,343,450 B unsupported tail (`model.after_linked`) on the 798 build-variant Models; typed `FBspVertexStream` (+ per-vertex `FColor`) decoded on Plage00/Plage01/Banque01 |
| Engine.TerrainInfo | 18 | 18 | 0 | sectors, vertices, sector grid; 2,052 B unsupported tail |

Texture formats are taken from the game's own `Engine.ETextureFormat` enum in `engine.u`: P8,
RGBA7, NoUsed1, DXT1, RGB8, RGBA8, NODATA, DXT3, DXT5, G8, G16, RRRGGGBBB, RGB565, P4.
Observed counts: DXT1 2,791; DXT3 427; DXT5 214; RGBA8 46 (stored BGRA); P8 13; G16 12
(terrain heightmaps); RGB8 3 (stored BGR); G8 1. P4 and RGB565 do not occur. Nine textures store
an empty 1x1 last mip. The 13 P8 textures without a `Format` property use class default 0 (P8).

Static meshes total 1,927,416 vertices, 1,222,788 render triangles, 679,565 collision triangles
(set 0), 1,824 meshes with a simplified collision set and 1,224,202 raw source triangles.

### Layouts (after the tagged-property block)

**Texture.** Compact mip count, then per mip: i32 lazy-array skip offset (absolute file offset
of the end of the data; verified on every mip), compact byte count, data, i32 USize, i32 VSize,
u8 UBits, u8 VBits. For P8/P4, an inline palette `TArray<{R,G,B,A}>` follows (licensee >= 42,
per UModel). For licensee >= 55, 3 bytes follow (71 distinct values; meaning unknown).
`UWindowFonts.utx` (licensee 50) has none.

**StaticMesh** (licensee 58; also one licensee-57 mesh). Fields in order:

- UPrimitive: FBox, FSphere, then 4 bytes + f32.
- Sections: `TArray<{i32 f4=0, u16 FirstIndex, FirstVertex, LastVertex, fE(=NumFaces), NumFaces}>`.
- A second FBox.
- VertexStream: `TArray<{FVector pos, FVector normal}>` + i32 revision.
- UVStreams: `TArray<{TArray<f32 u,v>, i32 channel, i32 1}>`. There are **no colour/alpha
  streams**.
- IndexBuffer and WireframeIndexBuffer: each `TArray<u16>` + i32.
- Two collision sets, each `{TArray<FVector>, i32, TArray<{u16 v0,v1,v2; i16 material}>,
  TArray<{u16 tri; i16 coplanar, front, back; 4 bytes}>}`. Set 0 is per triangle, with one BSP
  node per triangle and coplanar chains. Set 1 is the simplified collision; it is empty unless
  `SimplifiedColMaterial` is set.
- `TLazyArray<FStaticMeshTriangle>`: the simplified source triangles (material -1, no UVs).
- `TArray<u16>` + i32: the simplified collision's wireframe.
- A compact reference that matches `SimplifiedColMaterial`.
- 5 x i32 (4 for licensee 57) + 2 bytes. The second-to-last i32 equals the total face count in
  6,060/6,128 meshes.
- `TLazyArray<FStaticMeshTriangle>` raw triangles, in UModel's v>=112 layout: 3 FVector, i32 UV
  count, 3 UVs per channel, 3 FColor, i32 material, u32 smoothing.
- i32 InternalVersion: 13, or 12 in 68 meshes.
- `TArray<{compact material, u8 EnableCollision, u8 (0/1)}>`.

The `Materials` property elements are stored as `{u8 EnableCollision, compact Material}`. In
some meshes the 4-byte node field and the last 6 bytes of the unknown block look like
uninitialized memory.

**Model (BSP).** Fields in order:

- UPrimitive.
- Vectors and Points: `TArray<FVector>` each.
- Nodes, 70 bytes each: FPlane, u64 zone mask, u8 flags, i32 iVertPool, 7 x i16 (iSurf, iBack,
  iFront, iPlane/coplanar, two probable bound indices, unknown), FSphere, u8 iZone[2], u8
  NumVertices, i16 iLeaf[2], i16, and a u16 that increments by NumVertices (likely the first
  render vertex).
- Surfs: compact Material, u32 PolyFlags, i16 pBase, vNormal, vTextureU, vTextureV, iLightMap,
  iBrushPoly, u8 (0/255), compact Actor (always a Brush in Plage00), FPlane.
- Verts: `{u16 pVertex, i16 iSide}`. Only the ranges that nodes reference hold valid point
  indices.
- i32 NumSharedSides, then i32 NumZones.
- u32 `reserved` (always 0 in the corpus; meaning unknown).
- Zones: `NumZones` records of `{ compact ZoneActor; u64 Connectivity; u64 Visibility;
  f32 LastRenderTime }`. The record is **variable length** because `ZoneActor` is a compact
  object index (1 to 5 bytes); the remaining 20 bytes are fixed. Zone 0's actor is null in all
  64 zoned maps; the other actors are `Engine.ZoneInfo` or `Engine.SkyZoneInfo` exports.
  `Connectivity` is a zone bitmask: measured over all 1,350 zone records of the 64 zoned GOG
  maps, bit `z` is set for zone `z` and no bit at or above `NumZones` is set. `Visibility`
  and `LastRenderTime` positions/sizes are forced by the record length, but their meaning is
  not established (`Visibility` is not itself a `NumZones`-bit mask).
`Polys`: a compact object reference. It resolves to an `Engine.Polys` export in every one
of the 7,194 GOG Models (null never occurs).

What follows the `Polys` reference was decoded from `UModel::Serialize` in `Engine.dll` (RVA
0x9C240) and verified against the GOG bytes. Field order:

- `LightMap`: `TArray<FLightMapIndex>`. Each record is 178 bytes in this build (licensee 58,
  version 100): i32, i32, two 16-f32 matrices, nine i32, two bytes, then four compact indices.
  The array is empty in 7,183 of the 7,194 Models.
- A second `TArray` written by `0x103997a0`: each element is a `u16`, a nested `TArray<u8>`
  (compact count + bytes), then three i32 (engine version > 0x5b). Empty on the 6,396 fully
  consumed Models; `world-coverage` decodes 1,091 elements (91,905 nested bytes) corpus-wide
  (the `Banque01` level model is the largest). Field meanings are not established
  (`Model::light_bits`, `LightMapBits`).
- `Bounds`: `TArray<FBox>` (25 bytes: min, max, IsValid byte).
- `LeafHulls`: `TArray<i32>`.
- `Leaves`: `TArray<FConvexVolumeLeaf>` (compact Zone/Permeating/Volumetric + u64
  VisibleZones). The count equals the maximum node `iLeaf` + 1 on the maps where the full
  payload is consumed.
- `Lights`: `TArray<compact actor reference>`.
- `RootOutside` i32, `Linked` i32, `MoverLink` i32.
- `FBspVertexStream`: compact count then `count` records of 32 bytes (one `FBspVertex`), then an
  i32 revision. `FBspVertex` is `position` (3 f32), `color` (4 bytes, `FColor` memory order
  `B,G,R,A`), `uv0` (2 f32) and `uv1` (2 f32) — established from the element writer
  `0x10398160`, the array writers `0x1039bb30`/`0x10399830`, and `GetStride` 0x20 /
  `GetComponents` (position, colour, two texcoords) in `Engine.dll`; see
  `local/re/item5e_fbspvertex_disasm.txt`. The stream holds the node polygons' vertices in
  **reverse** node order: for node-vertex `k` of node `n`,
  `vertex_stream[n.first_vertex + (n.num_vertices - 1 - k)].position` equals
  `points[verts[n.vert_pool + k].point]` (measured exact on Plage00 1,704/1,704, Plage01
  2,627/2,627 within 0.2 UU, Banque01 8,782/8,784). The colour is a per-vertex `FColor` that is
  almost always `FF FF FF FF` (white) or `00 00 00 00` (transparent black), with rare greys
  (`FE FE FE FE` on `DM_LostTemple`); black entries occur at collinear (non-corner) vertices.

For **6,396** of the 7,194 Models every payload byte is consumed exactly (`xiii-tool bsp`
shows no unsupported tail). The remaining 798 exports use a build variant whose post-`Polys`
layout differs (all seven `Engine.Model` brushes with a non-empty `LightMap` array on the maps,
plus a few others); the decoder keeps the decoded prefix and reports the remainder as the
explicit label `model.after_linked` (1,343,450 B corpus-wide, down from 9,496,581 B). A decoded
tail is trusted only when it consumes the payload exactly, or — for the `Banque01`-style variant
— when its `FBspVertexStream` positions match `Points` for every referenced node vertex; a
misaligned variant stream is rejected (`Model::vertex_stream_matches_points`).

The lightmap **texels** are not decoded: the `LightMap` array is empty in 7,192 of 7,194
Models and the meaning of `FLightMapIndex.DataOffset` relative to the texture data is not
established, so `Model::lightmap_texels` is not implemented. Leaf -> zone assignments are
derived exactly from the nodes' `iLeaf`/`iZone` pairs (0 conflicts over all 64 zoned maps);
this is what `xiii-tool zones` uses. The level BSP is the only `Model` that no `Brush`
property references.

**Polys.** i32 Num, i32 Max, then per polygon: compact vertex count, Base, Normal, TextureU,
TextureV, vertices, u32 PolyFlags, compact Actor, compact Material, compact ItemName, compact
iLink, compact iBrushPoly. There are no PanU/PanV fields.

**TerrainInfo.** Properties:

- `TerrainMap`: a G16 texture.
- `TerrainScale`.
- `Layers[i]`: struct `TerrainLayer`, 37 raw bytes (compact Texture, compact AlphaMap, f32
  UScale, f32 VScale, then 25 bytes that are all zero in the inspected maps).
- `QuadVisibilityBitmap` and `EdgeTurnBitmap`: u32 words.

Native data: `TArray<compact TerrainSector>`, a `TArray<FVector>` of world-space vertices
(HeightmapX x HeightmapY, X fastest, rows along +Y), i32 SectorsX and i32 SectorsY. The
remaining 113-116 bytes are an unsupported tail; they contain the scale 550/550/0.39, a
translation and HeightmapX/Y. Plage00 and Plage01 both use a 32x16 heightmap, scale
550/550/100 and 4x2 sectors.

**TerrainSector.** Compact TerrainInfo, i32 QuadsX, QuadsY, OffsetX, OffsetY, 8 x FVector box
corners, `TArray<{i16 light index; TArray<u8> visibility bits}>`, then `TArray<FColor>` vertex
colours ((QuadsX+1)(QuadsY+1)). The light index is not an object reference (`42 00` occurs).

### Baked vertex lighting (`StaticMeshInstance`, terrain colours)

`xiii-tool world-coverage` counts 14,542 `Engine.StaticMeshInstance` exports corpus-wide; all
decode exactly (0 failures) and the per-light visibility cross-check below holds for every one.
A `StaticMeshActor` references its instance through the `StaticMeshInstance` object property,
so the lighting is **per placed actor**, not per mesh asset. The native payload after the
tagged-property block:

```text
TArray<FColor> Colors                 // one per render vertex of the actor's StaticMesh
compact NumLights
NumLights x {
  i16     LightIndex                  // index into the level light list; -1 (0xffff) occurs
  compact VisibilityByteCount         // == ceil(Colors.len() / 8): one bit per vertex
  u8[VisibilityByteCount] Visibility
  i32     Unknown                     // measured 0 or 1; meaning not established
}
```

Evidence (measured 2026-10-05, GOG corpus): every instance consumes its payload exactly; the
`VisibilityByteCount == ceil(Colors/8)` relation holds for all 867 instances on
Plage00/Plage01/Banque01; the colour count equals the referenced mesh's vertex count for every
placed actor on those three maps (0 mismatches, `opt_in_baked_lighting_invariants`). Instances
whose whole colour array is `[0,0,0,0]` (167 of 867) have no static lighting and are treated as
unlit rather than modulated to black. The 4th byte is 255 on every baked vertex and 0 on the
all-zero arrays.

**BSP** surfaces are **not** lit by `Model::vertex_stream`: the stream's 4-byte component is
decoded as `BspVertex::flags_or_color` and position-validated (`Model::vertex_stream_matches_points`),
but it is white at polygon corners and transparent-black (A=0) only at collinear (T-junction)
vertices, so modulating by it blackens those vertices — it is a flag/initialisation artefact,
not a light term (rejected on review; see the item5e report). `xiii-world` counts the split
(`lighting.bsp.color_white/black/other`) and leaves BSP `SceneObject::colors` as `None`. BSP
lighting has to come from the lightmap **texels**, which are not decoded (next task).

**Channel order and scale.** `FColor` on disk is the UE2/UE3 little-endian `G,B,R,A` memory
layout (the `FColor` union is `struct { uint8 B, G, R, A; }` on little-endian platforms in the
UE3 `Color.h`; UE2 uses the same D3D-colour layout). Independent local evidence: the Plage
terrain colours are warm sand (`R` mean 155, `G` 97, `B` 80) only when read as BGRA; read as
RGBA they would be blue. The renderer swaps B and R before upload. **No half-intensity (`x2`)
scale is applied**: the stored values span a continuous 0..255 (p50 R/G/B 62/76/93, 19-31%
above 128, no pile-up at 128), and the same `FColor` terrain path reaches 255 on sunlit sand,
so doubling would clip most of the terrain. This is the closest available evidence; no
original-engine capture exists to settle it, so the scale is recorded as a **hypothesis** in
`local/reports/item5b-baked-lighting.md`.

Terrain sector colours are assembled into one `width x height` grid per `TerrainInfo`
(`terrain::color_grid`), placing each sector's `(QuadsX+1) x (QuadsY+1)` colours at its vertex
`offset`. Adjacent sectors share a border row/column; a repeated cell is only a conflict when
the colours disagree (0 on Plage00/Plage01). The BSP `LightMaps`/`LightBits` tail is still not
decoded (see below).

### Coordinates and winding (`common`)

There is a single conversion point: `(x, y, z) -> (y, z, -x) / UNREAL_UNITS_PER_METER`. The
scale (50 UU/m) is provisional and **not calibrated**. Directions use the same mapping without
the scale; scales are permuted `(y, z, x)`. Rotators use 65536 units per turn and apply roll
about X, then pitch about Y, then yaw about Z, so forward = `(cos P cos Y, cos P sin Y, sin P)`.
The Bevy rotation is `C R C^T` (det +1); this is unit-tested. The axis change has det -1, so it
flips the numeric orientation of triangles.

- **Static meshes:** in source space the numeric normal `(b-a)x(c-a)` points against the stored
  vertex normal for 1,212,092 triangles and along it for 758 (9,938 degenerate). After
  conversion the source index order is counter-clockwise in Bevy, so no reordering is needed.
- **BSP node polygons** point numerically along the node plane (Plage00 316/0, Plage01 477/19),
  so the viewer reverses BSP fan triangles.
- **Terrain** triangles are emitted in the static-mesh convention. A unit test checks that the
  converted normals face +Y.
- **Visual check:** the "CLOSED FOR WINTER" sign (`StaticPlage2.pano08`, texture
  `XIIIPlage.PLclosed`) in Plage01 reads correctly (not mirrored) and is front-facing with
  back-face culling on.

### Collision data decoded (not yet validated against original movement)

- **Static meshes:** collision set 0 (per-triangle, collision-enabled materials only: 679,565
  triangles vs 1,222,788 render triangles) with a triangle BSP. The simplified set 1 (1,824
  meshes) has its own BSP and source triangles. `EnableCollision` is stored per slot.
- **Actor flags:** `bCollideActors`, `bBlockActors` and `bBlockPlayers` when present. Class
  defaults are not applied.
- **BSP:** node planes and children; PolyFlags `PF_NotSolid` 0x8, `PF_Semisolid` 0x20,
  `PF_Invisible` 0x1 and portal 0x04000000; the node collision-bound index (meaning inferred).
  Zones (actor + connectivity/visibility) are decoded; leaf zones are derived from the nodes.
  `Bounds`, `LeafHulls` and the `Leaves` convex-volume array are decoded; the node
  `iCollisionBound` indexes `LeafHulls` and `iRenderBound` indexes `Bounds` (both hold on the
  fully-consumed Maps after a bug in the old node-field reading was ruled out). The lightmap
  **texels** are not decoded (see the Model section).
- **Terrain:** world-space vertices, quad visibility (holes; Plage01 has 33 hidden quads) and
  edge turns. The bit conventions follow UE2 naming and are not verified in play.
- **Ray probes:** `xiii-app --dump` casts rays (not capsule sweeps) from the PlayerStart
  against this data. In Plage00 the ground (terrain) is 1.61 m below the start. In Plage01 the
  BSP floor is 0.24 m below, a door (`Porte6`) is 12.2 m ahead and walls are about 1.1-1.3 m to
  either side.

### Unknown / next

- The build variant of the 813 Models whose tail does not decode end-to-end (`model.after_linked`):
  the `LightMap` record layout for the seven map `Engine.Model` brushes with a non-empty
  `LightMap` array (and a few other exports) differs from the 178-byte form used elsewhere.
- The lightmap texels (`FLightMapIndex.DataOffset` relative to the texture data) and the
  TerrainInfo tail.
- The meaning of the `reserved` zone-field u32, of `Zone.Visibility` and of
  `Zone.LastRenderTime`, and of the texture trailing bytes, the static-mesh unknown block and
  the node padding.
- Scale calibration against the player cylinder; PrePivot.
- Material semantics: Shader, FinalBlend and modifiers are only followed to a texture.
- Sky-zone selection order and the meaning of the per-zone sky settings (`LinkToSkybox`).
- Vertex lighting: static-mesh instance colours and terrain sector colours are decoded and
  rendered; the BSP `LightMaps`/`LightBits` **arrays** are decoded but the lightmap
  **texels** are not (empty on all but 2 Maps).
- A capsule sweep with the original movement rules.
