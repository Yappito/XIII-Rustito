# Investigation

Date: 2026-10-04. Scope: XIII Classic (2003), local GOG installation and the user's installed Steam copy, Windows delivery with WSL2 available for development.

## Conclusions

**The requested import-from-an-owned-installation design is a credible direction.** We can already read the package tables across both local installations. Existing open-source tools document several required formats. A complete campaign-compatible runtime remains a substantial engine-reimplementation project; asset import alone will not make the game playable.

Evidence labels used throughout these documents:

- **Measured:** inspected in local files or produced by the supplied probe.
- **Upstream:** stated by a referenced project's documentation or code; not necessarily tested locally.
- **Proposed:** an implementation decision or experiment, not a working capability.
- **Unverified:** a question still requiring an experiment.

## What iw4L actually does

Reviewed revision: `d352dbdfd8778f12b5f93908bb5ccbc55561acca`.

Its README describes an independent Rust/Bevy runtime that uses owned game installations. It supports exploring multiplayer maps, bots and some combat/replay behavior, and explicitly calls gameplay incomplete. It is not evidence that a full MW2 campaign has been ported. [README](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/README.md)

The importer separates filesystem access, game-format parsing and normalized asset products. A load builds a prepared match before installing it, and caches derived products with versioned content keys. This is a useful architecture for XIII's package dependencies and long imports. [Map-loading design](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/docs/MAP-LOAD.md), [cache implementation](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/crates/asset_transport/src/artifact_cache.rs)

Its Windows discovery code searches Steam libraries. The player experience is effectively “select/find an installed game, then load its data”; manual bulk export is not the product's prerequisite. [Steam discovery](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/crates/asset_transport/src/steam.rs)

The reference also implements a language runtime: loaded GSC governs gameplay through Rust-provided engine operations. That distinction is highly relevant to XIII, but GSC is not UnrealScript and cannot be reused as XIII's VM. [GSC runtime](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/docs/GSC-RUNTIME.md)

Its custom renderer translates retail D3D9 shader bytecode and owns a substantial wgpu pipeline. That is a game-specific solution, not a requirement for all Bevy reimplementations. Start XIII with Bevy meshes and custom materials; investigate its actual material and vertex-shader semantics before considering a separate renderer. [Rendering](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/docs/RENDER.md)

## Local installation evidence

Canonical baseline supplied by the user:

- Windows: `P:\AI\XIII\XIII_Game`
- WSL: `/mnt/p/AI/XIII/XIII_Game`
- Measured size excluding save/profile/cache/log directories: **2,656,450,126 bytes across 611 files**.
- `XIII.exe` is PE32 / i386. Runtime is split across engine, rendering, audio, UI and game DLLs. `UCC.exe` and `XIIIEd.exe` are included.
- Package magic: `0x9E2A83C1`, little endian.

| Family | GOG files | Bytes | Role and qualification |
|---|---:|---:|---|
| `.unr` | 64 | 110,245,973 | Levels, including multiplayer, menu and utility maps; not 64 campaign missions |
| `.usx` | 35 | 187,866,956 | Static meshes |
| `.utx` | 41 | 39,882,078 | Texture/material packages |
| `.u` | 19 | 42,754,918 | Classes, compiled scripts, and some meshes/animations/textures |
| `.uax` | 37 | 423,854 | Sound objects; far too small to hold the complete audio corpus |
| `.hxc` | 59 | 849,601,186 | Ubisoft HXAudio banks |
| `.hsc` | 40 | 1,011,688,308 | Associated streamed audio data; exact per-sound linkage still to verify |
| `.bik` | 19 | 394,784,564 | Bink videos, including logos and story cinematics |

All **196** recognized package files passed the research probe's table/reference/range checks:

| Package version / licensee revision | Packages |
|---|---:|
| 100 / 58 | 176 |
| 100 / 57 | 18 |
| 100 / 56 | 1 |
| 100 / 50 | 1 |

These are package-format versions, not marketing game versions. Do not assume every file is 100/58, or that generic UT2004/modern Unreal readers apply. UELib associates XIII with engine build 829 and package 100/58; the existing Steam runtime log reports `829-145`. Only the latter installation has that measured runtime-log evidence. [UELib support table](https://github.com/EliotVU/Unreal-Library#supported-games)

The probe implements compact indices, length-prefixed names, import/export tables, local reference validation and export byte-range checks. It does **not** deserialize object properties, interpret bytecode, decode geometry, verify every payload, or demonstrate rendered output. Passing it is not an importer-completion claim.

### Paths matter

GOG has `system/PC/*.u` and `TexturesPC/*.utx`; its configuration names `SpecificPackage` entries. The Steam copy has flat `System/*.u`, `Textures/*.utx`, and `Maps/BaseSP` / `Maps/BaseMP`. Windows ignores many case differences that Linux does not. Import resolution must preserve original spelling while indexing identifiers case-insensitively and must explicitly handle platform-specific packages and duplicates.

All top-level imported package names in the GOG inventory have a matching installed package stem. This checks only package-name presence: class/object resolution, correct precedence, imports into a package, and payload compatibility remain unverified.

### Steam is a patched comparison corpus

Located at `P:\SteamLibrary\steamapps\common\XIII - Classic` (Steam app 1170760; installed manifest build ID 5110347). There are `XIIIPlus`, `EnginePlus`, `XIDInterfPlus`, other additional packages and a credit file identifying unofficial patch 1.4. The manifest does not identify subsequent local modifications, so do not attribute all observed differences to Valve's distributed build.

Measured **213** package files, all passing the same table probe. Matching unique case-insensitive asset basenames:

- 310 shared asset files have identical SHA-256 hashes.
- Four shared assets differ: `XIDInterf.u`, `alien.bik`, `nvidia.bik`, `ubi.bik`.
- Seventeen asset files occur only in the Steam copy; no GOG-only assets under this matching rule.
- All 64 maps and all 99 `.hxc`/`.hsc` files match by hash despite the folder changes.

Binary/configuration differences are recorded separately. Prefer the supplied GOG corpus for the first compatibility profile; use Steam as a second layout and patch-variation test. Do not merge the two installations into one implicit search path. [Asset comparison](evidence/asset-comparison.json), [path comparison](evidence/install-comparison.json)

## Evidence for the first campaign segment

`Plage00.unr` and `Plage01.unr` are candidate opening-sequence maps. Their object names fit the beach sequence; map progression still needs confirmation against original play and serialized properties.

| Observation | Plage00 | Plage01 |
|---|---:|---:|
| Names / imports / exports | 1,166 / 173 / 659 | 2,138 / 371 / 1,138 |
| StaticMeshActor exports | 156 | 133 |
| Model exports | 51 | 77 |
| TerrainInfo / TerrainSector exports | 1 / 8 | 1 / 8 |
| PlayerStart exports | 1 | 1 |

These are export counts, not the number of currently active entities or collision surfaces. Map tables also reveal doors/movers, camera/dialogue actors, enemy generation, checkpoints and mission-specific `XIDMaps` classes. `Plage01` includes comic-window classes (`CWnd…`), a Beretta pickup and ladder/volume classes. A beach viewer needs BSP, static meshes **and terrain**. A playable sequence additionally needs stateful game behavior. [Focus-package evidence](evidence/focus-packages.json)

`StaticPlage2.usx` contains 154 `Engine.StaticMesh` exports. `system/PC/xiiipersos.u` contains 74 `Engine.SkeletalMesh`, 41 `Engine.MeshAnimation` and 161 `Engine.Texture` exports. The absence of a separate Animations folder is not evidence that animation data is missing.

## Gameplay source and native behavior

In the GOG copy, all 535 `Core.TextBuffer` exports in `xiii.u`, all 133 in `xidmaps.u`, all 56 in `xidpawn.u`, and all 97 in `xidcine.u` have 13-byte payloads. Inspected samples have a one-space text payload. This is evidence of stripped gameplay source text. By contrast, `engine.u` has varied, substantial text buffers.

The gameplay packages still expose class/function/state metadata: `xiii.u` alone has 535 class, 1,558 function and 128 state exports. Actual function-bytecode deserialization was not implemented in this pass. The next spike must determine its layout, usable script coverage and native-call inventory before promising campaign compatibility.

DLL export names expose useful research entry points such as `FindBestPathTo` in `XIDPawn.dll` and `AutoPosition` in `XIDCine.dll`. Native symbol names do not provide implementations, nor do export counts equal native-operation counts. The new x64 runtime cannot directly link the original 32-bit DLLs as ordinary in-process dependencies. [DLL metadata](evidence/native-dll-summary.json)

An archived [XIII UnrealScript repository](https://github.com/pingwindev/xiii-unrealscript) describes its contents as likely Xbox-branch code and acknowledges missing PC classes. Its root tree had no license file. Treat it as a provenance-sensitive, potentially mismatched research lead; it is not a freely licensed, verified PC codebase to translate wholesale. No gameplay source from it was copied into this project.

## Existing tools: useful but incomplete

| Tool | Upstream evidence | Use here |
|---|---|---|
| [UE Viewer / UModel](https://www.gildor.org/projects/umodel/compat) | XIII row: skeletal mesh and textures supported; animation and static mesh unsupported | Texture/mesh cross-checks and permissively licensed parser reference; not an all-assets conversion solution |
| [Unreal-Library / UE Explorer](https://github.com/EliotVU/Unreal-Library) | XIII 100/58 listed; C# package/decompiler implementation | Independent table/property/bytecode inspection candidate; XIII decompilation still needs local validation |
| [vgmstream HX parser](https://github.com/vgmstream/vgmstream/blob/7dc938fa2f210943b37c7b6511852b516ef432ab/src/meta/ubi_hx.c) | Explicit XIII PC `.hxc` support and XIII-specific handling | Audio decoder spike; preserve bank IDs, stream associations and cue/loop behavior |
| Bundled UCC / XIIIEd | Present in this GOG installation | Local reference/export experiment on a disposable working copy; not required for players |
| [SurrealEngine](https://github.com/dpjudas/SurrealEngine) | UE1 reimplementation, focused on Unreal/UT99 | Architectural reference for object/VM/native separation; not XIII/UE2 support |

No third-party extractor or decompiler was executed against the game during this pass. Compatibility-table claims and source inspection are not successful extraction tests.

## Bevy and host feasibility

Verified upstream release at investigation time: **Bevy 0.19.1**, released 2026-08-13. Its tagged manifest specifies **Rust 1.95.0** as its minimum. iw4L's reviewed manifest uses Bevy 0.19. Use a pinned release and lockfile rather than `main` or unversioned examples. [Release](https://github.com/bevyengine/bevy/releases/tag/v0.19.1), [tagged manifest](https://github.com/bevyengine/bevy/blob/v0.19.1/Cargo.toml)

Native Windows should be the main runtime and visual-validation target. WSL2 is suitable for parsing, hashing, scripting and headless tests. The host reports an NVIDIA RTX 4090, but no Bevy GPU/window/audio test was run. Rust/Cargo were not discoverable on the inspected WSL or native Windows PATH; the standard Visual Studio installer locator was absent. This does not establish that no toolchain exists elsewhere.

Bevy's custom assets/materials and fixed-step scheduling are appropriate building blocks. Physics is still a project decision: Avian 0.7 upstream targets Bevy 0.19, but its generic controller behavior is not proof of XIII movement fidelity. [Custom asset example](https://github.com/bevyengine/bevy/blob/v0.19.1/examples/asset/custom_asset.rs), [material example](https://github.com/bevyengine/bevy/blob/v0.19.1/examples/shader/shader_material.rs), [Avian release](https://github.com/avianphysics/avian/releases/tag/v0.7.0)

## Uncertainties and effort

| Risk | Next resolving experiment |
|---|---|
| Static meshes, BSP, terrain and collision use custom layouts | Decode representative objects with exact export boundaries; compare geometry and collision to original game/editor |
| MeshAnimation is unsupported by the convenient viewer | Recover one real character clip and verify bind pose, timing and transforms |
| Compiled scripts depend on extensive native engine services | Decode/disassemble mission functions, enumerate natives, run a minimal state/timer/trigger path |
| Audio object IDs do not directly identify decoded bank tracks | Trace one weapon sound and one dialogue event through package metadata to HX bank/stream |
| Comic presentation is more than a generic outline effect | Capture original scene behavior; inspect materials, comic cameras and triggers |
| Existing tools give plausible but incorrect results | Cross-check with a second parser and actual in-game observations |

Engineering judgement: expect multiple milestones and potentially many months for faithful campaign completion, even with capable coding agents. No calendar commitment is justified before the world, animation and script spikes. A map viewer can arrive much earlier and must be labeled accordingly. Existing patches may help establish an original-game reference, but applying patches was outside this investigation and was not performed.
