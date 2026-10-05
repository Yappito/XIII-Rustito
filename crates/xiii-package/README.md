# xiii-package

Bounded, filesystem-free reader for XIII Classic package files (Unreal Engine 2, file
version 100). Input is `&[u8]` plus an explicit `Limits`; no dependencies; `unsafe` forbidden.

Scope: summary, GUID/generations, name/import/export tables, typed object references,
outer paths with cycle/depth checks, class histograms, payload byte spans, and (module
`object`) export payload slicing, the `RF_HasStack` state frame and UE1/UE2 tagged properties.
Class-native payload data after the properties (meshes, textures, BSP, bytecode, UClass
data) is **not** decoded; it is reported as a byte count (the "native tail").

## Verified layout (version 100)

| Offset | Type | Field |
|---|---|---|
| 0 | u32 | tag `0x9E2A83C1` |
| 4 | u16 / u16 | file version (100) / licensee (50, 56, 57, 58 observed) |
| 8 | u32 | package flags |
| 12..36 | 6 x i32 | name count/offset, export count/offset, import count/offset |
| 36 | 16 bytes | GUID |
| 52 | i32 | generation count `G` |
| 56 | G x (i32, i32) | generation (export count, name count) |
| 56 + 8G | | end of summary |

Tables, as in `tools/probe_install.py`:

- Name: FString (compact length; positive = Latin-1 + NUL, negative = UTF-16LE + NUL) + u32 flags.
- Import: compact class-package name, compact class name, i32 outer ref, compact object name.
- Export: compact class ref, compact super ref, i32 outer ref, compact object name, u32 flags,
  compact serial size, compact serial offset (present only when size != 0).
- Object refs: 0 null, `n > 0` export `n-1`, `n < 0` import `-n-1`. A null export class is reported as
  `Core.Class`. Export paths omit the containing package name; import paths start at the root package.
- Compact index: old Unreal encoding (sign bit 7 + continuation bit 6 + 6 bits, then up to three
  7-bit groups, then a fifth byte that may only use its low 5 bits). Not LEB128. Magnitudes above
  `i32::MAX` (positive) or `2^31` (negative) are rejected; negative zero and non-minimal encodings
  are accepted, matching the probe.

## Evidence (measured 2026-10-04, `xiii-tool corpus`)

| Corpus | Packages | Dialects (100/licensee) | Generations | Probe inventory match |
|---|---|---|---|---|
| GOG `XIII_Game` | 196 | 50: 1, 56: 1, 57: 18, 58: 176 | 1 gen: 177, 3 gen: 19 | 196/196 |
| Steam (community patch) | 213 | 50: 1, 56: 1, 57: 18, 58: 193 | 1 gen: 194, 3 gen: 19 | 213/213 |

- In every package the summary ends exactly at the first table (`56 + 8G`, so 64 or 80 bytes):
  no bytes go unaccounted between the summary and the tables.
- In every package the newest generation equals the current (export count, name count). The 19
  three-generation files are the original retail `.u` script packages (the 14 extra Steam `.u` files have one generation) (e.g. `core.u`: names 335, 336, 337; exports 694).
- Summary + three tables + all export payload spans cover every byte of every file, with no
  overlaps (`Package::unaccounted_ranges` / `overlapping_ranges` empty for all 409 files).
- Name/import/export counts, table spans, version/licensee, flags, per-class and zero-size
  export histograms, and imported root packages equal `docs/evidence/{gog,steam}-inventory.json`.

## Object payloads (version 100)

API: `Package::export_payload(data, i)`, `Package::read_object_properties(data, i, &limits)`
-> `ObjectProperties { state_frame, block: PropertyBlock { properties, span, terminator },
payload }` with `consumed()` / `tail()`, and `Package::read_property_block(data, start, end,
&limits)` for blocks that do not start the payload (e.g. class defaults; `crates/xiii-script`
locates them after the native UClass data and exposes `class_defaults()`).
`data` must be the parsed buffer (its length is checked). Structural failures are
`PackageError`s with `Table::Payload`, the export index, a field such as `property.size`
and the absolute offset. Value-level problems keep a bounded raw span:
`PropertyValue::Raw(RawReason)`; `RawReason::is_anomaly()` separates layout mismatches from
deliberately unsupported values (map/fixed array, unknown struct). `Limits::max_properties`
(default 65 536) caps tags per block; string lengths are capped by their value size.

| Part | Encoding |
|---|---|
| State frame (only if export flags & `0x02000000`, RF_HasStack) | compact node ref, compact state-node ref, u64 probe mask, u32 latent action, compact code offset if node != 0 |
| Tag name | compact name index; the block ends at a name whose text is `None` (case-insensitive) |
| Info byte | bits 0-3 type, bits 4-6 size code, bit 7 array flag (for bools: the value) |
| Struct name | compact name index, only for type 10 |
| Size | code 0..4 = 1, 2, 4, 12, 16; 5 = u8, 6 = u16, 7 = i32 (negative rejected) |
| Array index | only if bit 7 and not bool: `0xxxxxxx`; `10xxxxxx b` (14 bits); `11xxxxxx b b b` (30 bits), big-endian |
| Value | `size` bytes; bools have none |

Types: 1 Byte, 2 Int, 3 Bool, 4 Float, 5 Object (compact ref), 6 Name (compact), 7 Delegate
(compact ref + compact name, as UELib reads type 7 from version 100; absent from the corpus),
8 Class (compact ref), 9 Array (compact count + raw element span; the element layout needs
the class schema), 10 Struct, 11 Vector, 12 Rotator, 13 Str (FString), 14 Map and
15 FixedArray (raw). Struct values are raw member data in version 100 (UELib: tagged structs
start at version 118), so only `Vector` (12), `Rotator` (12; i32 pitch/yaw/roll), `Color`
(4 bytes in stored order; channel order unverified), `Scale` (17), `Plane`/`Sphere`/`Guid`
(16), `Box` (25), `Range` (8), `RangeVector` (24) and `PointRegion` (compact zone ref, i32
leaf, u8 zone; must fill the value exactly) are decoded, and only when name and size match.
Everything else keeps its span.

References for the tag layout (both MIT): UModel `Unreal/UnObject.cpp` (`FPropertyTag`,
UE1/UE2 branch; sha256 `9c8447f35a3fab058879e2547f0cf206a1242826cd727e30994d35b6783a44b2`) at
`gildor2/UEViewer@a0bfb468d42be831b126632fd8a0ae6b3614f981`; UELib `src/Core/UStateFrame.cs`,
`src/Core/Classes/UDefaultProperty.cs` and `src/Branch/PackageObjectLegacyVersion.cs` at
`EliotVU/Unreal-Library@3207a17e9b294be3d1bf26b18e07ccff7e1d4b0c`. The Rust code implements
the layout independently (no code was copied); the four-byte array-index form keeps 30 bits as
in UELib, where UModel masks 22 bits. UModel asserts that RF_HasStack is absent; the state
frame layout follows UELib for version 100 (u64 probe mask below 691, u32 latent action below
566, no state stack below 189) and was confirmed on the corpus.

### Evidence (measured 2026-10-04, `xiii-tool coverage XIII_Game`)

**Update 2026-10-05:** `xiii-tool coverage` now decodes `Core.Class` exports through
`xiii-script` (native UField/UStruct/UState/UClass data first, then the defaults block, which
must end at the payload end): GOG **141 939 / 141 939** exports decode (1 444 / 1 444 classes),
Steam 149 847 / 149 847, 0 anomalous values. Class defaults are the only place where two-byte
array indices occur (130, max index 254). `docs/evidence/property-coverage-gog.json` was
regenerated; the list below is the original 2026-10-04 measurement.

Report: `docs/evidence/property-coverage-gog.json` (metadata only: class/struct names, counts,
sizes, error kinds; no property values). The Steam copy (213 packages) shows the same pattern:
only `Core.Class` fails (1 620), no anomalous values.

- 196 packages, 141 959 exports (20 zero-sized), 141 939 attempted: **140 498 decode**
  (0 anomalous values) and **1 441 fail, all `Core.Class`** (`InvalidPropertyType` 897,
  `NameIndexOutOfRange` 544, at payload+0). UClass payloads begin with native
  UField/UStruct/UState/UClass data; the class defaults follow and are not located yet. The
  3 `Core.Class` "successes" are coincidences: the first compact index (SuperField) equals
  the index of a `None` name entry (`core.u` Object; `xiiipersosG.u` JonesMaj, Mig).
- 364 classes: 301 always end exactly at the export boundary (properties only, e.g.
  StaticMeshActor 14 576, Light 9 531, Brush 6 402, SpriteEmitter, Shader, Package and the
  XIDCine/XIDPawn/XIII actors); 62 always leave a native tail (mean: StaticMesh 37 758 B,
  Texture 23 072, Model 2 494, StaticMeshInstance 1 024, TextBuffer 597, Polys 514,
  TerrainSector 457, State 161, Function 158, Sound 24, plus TerrainInfo, Level, MeshAnimation,
  SkeletalMesh, Font, Palette, Struct, Enum, Const, the `Core.*Property` field objects
  (about 10-13 B) and NavigationPoint subclasses (about 20-55 B: PathNode, PatrolPoint,
  AttackPoint, PlayerStart, InventorySpot, Ladder, Teleporter, ...)). State frames and
  properties take 8 385 706 of 375 027 560 payload bytes.
- State frames: 58 479 (every RF_HasStack export). Node == state node in all, never null;
  code offset always -1; probe mask always `0xFFFFFFFFFFFFFFFF`; latent action holds
  arbitrary values; 15 or 17 bytes (one- or two-byte compact references).
- Bools: 309 816, always size code 5 with an explicit size byte of 0 and no value bytes.
  Writers use an implicit size code whenever the size is 1/2/4/12/16 (ArrayProperty appears
  with codes 0-6).
- Array indices: 10 115 non-zero, max 15, so only the one-byte form occurs.
- Types present: Bool 309 816, Struct 229 705, Object 204 104, Float 170 834, Byte 92 581,
  Name 87 207, Int 31 254, Array 12 061, Str 1 232; no Delegate/Class/Vector/Rotator/Map/
  FixedArray tags. Decoded structs: Vector 114 422, PointRegion 58 381, Rotator 31 007,
  Scale 14 208, RangeVector 4 181, Range 1 807, Color 985, Plane 87. Unknown structs (raw):
  4 627 values of 13 types (Propr, InventoryItem, InitialAllianceInfo, SImpact, TerrainLayer,
  Matrix, C_*Sprites, ...); none parses as a tagged block, consistent with raw member data.
- Terminators: 32 658 blocks end on a *later duplicate* `None` name entry. Packages contain
  identical `None` entries (`core.u` names 0, 335, 336), so the terminator is matched by text,
  not by a fixed index.
- Object references in decoded properties: 172 499 to local exports, 30 496 to imports,
  1 109 null.

### Map references (`xiii-tool deps <map> --game-dir XIII_Game`)

| Map | Imported packages | Found | Imports below roots | Resolved (path + class) | Absent top-level classes | Failures |
|---|---|---|---|---|---|---|
| Maps/Plage00.unr | 17 | 17 | 156 | 151 | 5 | 0 |
| Maps/Plage01.unr | 20 | 20 | 351 | 346 | 5 | 0 |

The 5 absent imports in both maps are `Engine.Level`, `Engine.Model`, `Engine.Polys`,
`Engine.StaticMeshInstance` and `Engine.TerrainSector`: classes with no export in
`system/engine.u` (candidates for native-only classes registered by the engine DLL; not
proven). Every StaticMeshActor (Plage00 156, Plage01 133) has a decoded `Location` and a
`StaticMesh` import that resolves to an `Engine.StaticMesh` export (e.g.
`StaticPlage2.rocher01` in `StaticMeshes/StaticPlage2.usx`); `Rotation`/`DrawScale` appear
only when they differ from the defaults (Plage00: 92 / 54). Object-property values pointing at
imports: Plage00 196, Plage01 403. Package lookup in `deps` is a simple case-insensitive file
stem match, not the installation resolver.

## Tests

- `cargo test -p xiii-package` uses synthetic packages only (builder in `src/tests.rs`;
  property blocks, encodings and malformed payloads in `src/object_tests.rs`).
- Real corpora (opt-in, read-only): see `crates/xiii-tool/tests/local_corpus.rs`
  (`XIII_GOG_DIR`, `XIII_STEAM_DIR`) and `crates/xiii-tool/tests/local_properties.rs`
  (`XIII_GOG_DIR`: the coverage numbers above and Plage00/Plage01 import resolution).
- `crates/xiii-tool/tests/synthetic_deps.rs`: generated install for `deps`/`coverage`.
