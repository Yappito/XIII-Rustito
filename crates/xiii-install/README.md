# xiii-install

Read-only installation resolver. An `Installation` is one root. The crate validates it by content and walks it within fixed limits. It detects a measured content profile and builds a case-insensitive package index that keeps exact paths. Nothing is written into the game directory. The crate has no dependencies.

## API

- `Installation::open(root, &OpenOptions)` returns `Result<Installation, OpenError>`.
- `resolve_package(name)`, `resolve_map(name)` and `resolve_package_of_kind(name, kind)` return `Result<ResolvedPackage, ResolveError>`. A name may carry a known extension, for example `Plage00.unr`.
- `profile()` and `detection()` return the profile with its evidence and any `unknown_reasons`. The crate also exposes `inventory()`, `search_roots()`, `packages()`, `conflicts()` and `diagnostics()`.
- `ini_evidence()` and `specific_packages()` report `[Core.System]` `Paths=`, `SpecificPackage=` and `PlateForm=` from `Default.ini`/`XIII.ini`. These values are evidence only and never change resolution.
- `discover_candidates()` and `discover_candidates_in(&DiscoveryRoots)` are best-effort and have no side effects. They read Steam `libraryfolders.vdf` plus `appmanifest_1170760.acf`, and find `XIII*` folders under GOG parents taken from `ProgramFiles*`, `SystemDrive` and `HOME`. They do not read the registry, hardcode drive letters or open any candidate.

## Validation

A root must have a `system` directory, matched in any case, that directly holds `core.u` and `engine.u`, each starting with `C1 83 2A 9E`. After indexing, at least one `.unr` must be found in a map search root. The folder name is never checked.

The walk does not follow symlinks or junctions. Defaults are depth 6 and 50,000 entries, and hitting either limit produces a diagnostic. `Save`, `Saves`, `Cache` and `Logs` directories, plus `*.log` and `*.tmp` files, are never inventoried.

## Profiles (measured 2026-10-04)

| Signal | GOG `XIII_Game` | Steam `XIII - Classic` |
|---|---|---|
| `system/PC/*.u` | 9 | 0 |
| `TexturesPC/*.utx` | 41 | 0 |
| `Textures/*.utx` | 0 | 43 |
| `Maps/*.unr` | 64 | 0 |
| `Maps/BaseSP` / `BaseMP` `*.unr` | 0 / 0 | 38 / 26 |
| `<X>Plus` package next to `<X>` | none | EnginePlus, IpDrvPlus, MUL_CommonPlus.utx, Meshes_CommunsPlus.usx, XIDInterfPlus, XIIIMPPlus, XIIIPersosPlus, XIIIPlus |
| `Patch 1.4 Credits.txt` with "unofficial patch 1.4" | none | `Maps/` and `System/` |

Classification rules:

- **`Gog`** requires both platform signals, no flattened directories and no patch markers.
- **`SteamPatched`** requires a flattened layout (`Textures` or `Maps/BaseSP`/`BaseMP`) with at least one patch marker and no platform directories.
- **`Unknown`** covers everything else and lists the reasons. Examples are a mixed layout, a partial platform layout, a platform layout with patch markers, or a flat layout without patch markers.

`goggame.dll` is present in both installs, so it is not used as evidence.

## Search roots

Only direct children with the listed extension are indexed.

| Profile | Roots |
|---|---|
| Gog | `system` (.u), `system/PC` (.u), `Maps` (.unr), `TexturesPC` (.utx), `StaticMeshes` (.usx), `Sounds` (.uax), `Animations` (.ukx), `Music` (.umx) |
| SteamPatched | `System` (.u), `Maps`, `Maps/BaseSP`, `Maps/BaseMP` (.unr), `Textures` (.utx), `StaticMeshes` (.usx), `Sounds` (.uax), `Animations` (.ukx), `Music` (.umx) |
| Unknown | union of both |

The directories in the profile tables are matched case-insensitively. `Animations` and `Music` do not exist in either install.

Some directories are deliberately not indexed:

- `MapsUser` (GOG `Paths=`) and `Skins` (Steam `Paths=`) are user content. Both are empty locally, and each is reported as an `IniPathNotIndexed` info diagnostic.
- `Sounds/PC/*.hxc|*.hsc` are not Unreal packages.

### Comparison with the ini `Paths=` entries

- **GOG `Default.ini`:** `Paths=` lists `System`, `MapsUser`, `Maps`, `Sounds`, `Music` and `Animations`. `StaticMeshes` appears only commented out (`;;;Paths=`). `system/PC`, `TexturesPC` and `StaticMeshes` are not in `Paths=` at all, yet every `SpecificPackage=` entry is found only in `system/PC`, and all shipped textures and static meshes are in `TexturesPC`/`StaticMeshes`. How the original engine maps those platform directories (`PlateForm=0`) is still unverified.
- **Steam `Default.ini`/`XIII.ini`:** `Paths=` covers every indexed root. The same nine `SpecificPackage=` names are present, all flat in `System`.

## Duplicates and precedence

UE2 package names share one namespace. If two indexed files have the same lowercase stem, in any directories and with any extensions, the result is a `DuplicateLogicalPackage` warning. Every lookup of that name then fails with `ResolveError::Ambiguous`, even with a kind filter. No ordering rule picks a winner.

Measured result: **no duplicate stems exist in either install**, including across `system`/`system/PC` and across extensions. GOG has 196 packages with 196 logical names, and Steam has 213 with 213. So no precedence rule is needed or implemented. Do not read `SpecificPackage=` as "subdirectory overrides root".

## Tests

- `cargo test -p xiii-install` runs synthetic temporary trees covering both layouts, case-insensitivity, duplicates, bad magic, exclusions, rejection, Unknown profiles, ini comparison, walk limits and VDF discovery.
- The opt-in run against the real installs is `XIII_GOG_DIR=... XIII_STEAM_DIR=... cargo test -p xiii-install --test real_installs -- --nocapture`. With the variables unset it prints `SKIPPED`.
