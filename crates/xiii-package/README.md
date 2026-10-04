# xiii-package

Bounded, filesystem-free reader for XIII Classic package files (Unreal Engine 2, file
version 100). Input is `&[u8]` plus an explicit `Limits`; no dependencies; `unsafe` forbidden.

Scope: summary, GUID/generations, name/import/export tables, typed object references,
outer paths with cycle/depth checks, class histograms, payload byte spans. Export payloads
(properties, meshes, textures, bytecode) are **not** decoded.

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

## Tests

- `cargo test -p xiii-package` uses synthetic packages only (builder in `src/tests.rs`).
- Real corpora (opt-in, read-only): see `crates/xiii-tool/tests/local_corpus.rs`
  (`XIII_GOG_DIR`, `XIII_STEAM_DIR`).
