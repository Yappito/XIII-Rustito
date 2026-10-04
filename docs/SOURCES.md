# Sources and provenance

Checked on **2026-10-04**. Public upstream sources were inspected alongside the user's local installations. This document is a reference catalog, not a list of bundled dependencies. No original game content or third-party implementation was incorporated into a shipping runtime in this investigation.

## Reference project

**iw4L**, revision `d352dbdfd8778f12b5f93908bb5ccbc55561acca`:

- [README](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/README.md): purpose, owned-data model and incomplete gameplay status.
- [Cargo manifest](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/Cargo.toml): Bevy 0.19 dependency and workspace structure.
- [Map loading](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/docs/MAP-LOAD.md): parsing/product boundary and load/cache architecture.
- [Steam discovery code](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/crates/asset_transport/src/steam.rs): actual installation discovery.
- [Artifact cache code](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/crates/asset_transport/src/artifact_cache.rs): derived-product persistence.
- [Script runtime](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/docs/GSC-RUNTIME.md), [simulation](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/docs/SIM-STEP.md), [rendering](https://github.com/vladtrc/iw4L/blob/d352dbdfd8778f12b5f93908bb5ccbc55561acca/docs/RENDER.md): relevant subsystem separation, not directly reusable XIII semantics.

Its README states Apache-2.0 for project source and identifies attribution requirements. Check the exact file/dependency notices if reusing code. This investigation borrowed architectural ideas, not its Rust implementation.

## XIII formats and engine behavior

| Reference | Revision / source | Why it matters | Limit |
|---|---|---|---|
| UE Viewer / UModel | `a0bfb468d42be831b126632fd8a0ae6b3614f981`; [compatibility table](https://www.gildor.org/projects/umodel/compat) | XIII 100/58 support claims | Table explicitly does not support XIII animation/static meshes |
| UModel texture implementation | [UnTexture2.cpp](https://github.com/gildor2/UEViewer/blob/a0bfb468d42be831b126632fd8a0ae6b3614f981/Unreal/UnrealMaterial/UnTexture2.cpp) | XIII palette/revision branches | Source inspection, no local decode test |
| UModel mesh implementation | [UnMesh2.cpp](https://github.com/gildor2/UEViewer/blob/a0bfb468d42be831b126632fd8a0ae6b3614f981/Unreal/UnrealMesh/UnMesh2.cpp), [header](https://github.com/gildor2/UEViewer/blob/a0bfb468d42be831b126632fd8a0ae6b3614f981/Unreal/UnrealMesh/UnMesh2.h) | Old-engine and XIII mesh layout research | Does not establish complete campaign geometry support |
| UModel package implementation | [UnPackage.cpp](https://github.com/gildor2/UEViewer/blob/a0bfb468d42be831b126632fd8a0ae6b3614f981/Unreal/UnrealPackage/UnPackage.cpp) | Independent container implementation | Our Python probe remains a narrow research tool |
| Unreal-Library | `3207a17e9b294be3d1bf26b18e07ccff7e1d4b0c`; [repository](https://github.com/EliotVU/Unreal-Library/tree/3207a17e9b294be3d1bf26b18e07ccff7e1d4b0c) | XIII engine/package identifiers and decompiler investigation | Table support is not a local successful bytecode test |
| UE Explorer | [repository](https://github.com/UE-Explorer/UE-Explorer) | UI for package/decompiler work | Candidate tool, not installed/run |
| vgmstream | `7dc938fa2f210943b37c7b6511852b516ef432ab`; [ubi_hx.c](https://github.com/vgmstream/vgmstream/blob/7dc938fa2f210943b37c7b6511852b516ef432ab/src/meta/ubi_hx.c) | Explicit XIII PC HXAudio handling, codec and stream clues | Actual bank decoding/event linkage untested here |
| Archived XIII scripts | `9c2568a6e0fa3c5fbf753dbf6e457876f5e4593e`; [README](https://github.com/pingwindev/xiii-unrealscript/blob/9c2568a6e0fa3c5fbf753dbf6e457876f5e4593e/README.md) | Explains likely Xbox origin and omitted PC functionality | Not verified PC source; no root license file observed; no gameplay code copied |
| SurrealEngine | `cd0d0a229be22ce167c45a3ddb48e48e15201691`; [README](https://github.com/dpjudas/SurrealEngine/blob/cd0d0a229be22ce167c45a3ddb48e48e15201691/README.md) | Independent Unreal-engine reimplementation pattern | UE1 focus, not an existing XIII port |
| FFmpeg | [Bink demuxer](https://github.com/FFmpeg/FFmpeg/blob/master/libavformat/bink.c) | Candidate video validation path | No local playback or packaging decision |
| Ghidra | [official repository](https://github.com/NationalSecurityAgency/ghidra) | Candidate native-binary analysis tool | Not installed/run in this pass |

The checked [UModel license](https://github.com/gildor2/UEViewer/blob/a0bfb468d42be831b126632fd8a0ae6b3614f981/LICENSE.txt) and [Unreal-Library license](https://github.com/EliotVU/Unreal-Library/blob/3207a17e9b294be3d1bf26b18e07ccff7e1d4b0c/LICENSE) are MIT. [vgmstream's COPYING](https://github.com/vgmstream/vgmstream/blob/7dc938fa2f210943b37c7b6511852b516ef432ab/COPYING) grants use with retained notices. Those root notices do not substitute for checking optional codec/library dependencies in an actual build. No project-wide source license has been selected for this new runtime yet.

## Bevy and development stack

- [Bevy 0.19.1 release](https://github.com/bevyengine/bevy/releases/tag/v0.19.1), published 2026-08-13; latest release returned by upstream during this investigation.
- [Tagged Cargo manifest](https://github.com/bevyengine/bevy/blob/v0.19.1/Cargo.toml), declaring Rust 1.95.0 and MIT OR Apache-2.0.
- [Windows setup](https://bevy.org/learn/quick-start/getting-started/setup/): native build prerequisites.
- [Custom assets](https://github.com/bevyengine/bevy/blob/v0.19.1/examples/asset/custom_asset.rs), [custom material](https://github.com/bevyengine/bevy/blob/v0.19.1/examples/shader/shader_material.rs), [fixed timestep example](https://github.com/bevyengine/bevy/blob/v0.19.1/examples/movement/physics_in_fixed_timestep.rs): tagged implementation references.
- [Avian 0.7 release](https://github.com/avianphysics/avian/releases/tag/v0.7.0): upstream Bevy 0.19 compatibility. This is an optional candidate, not a tested local dependency.

## Local evidence index

| File | Contents |
|---|---|
| [gog-inventory.json](evidence/gog-inventory.json) | File sizes/hashes, extension totals, per-package table spans/counts/class summaries and top-level imports |
| [steam-inventory.json](evidence/steam-inventory.json) | Equivalent measurements for the locally patched Steam copy |
| [asset-comparison.json](evidence/asset-comparison.json) | Content comparison by unique case-insensitive asset basename |
| [install-comparison.json](evidence/install-comparison.json) | Normalized-path comparison including binaries/INI files; leaves nested-map path changes visible |
| [focus-packages.json](evidence/focus-packages.json) | Opening-map and gameplay/animation package metadata; TextBuffer-size histograms |
| [unresolved-package-stems.json](evidence/unresolved-package-stems.json) | Missing top-level package-stem check; empty result, not full object resolution |
| [native-dll-summary.json](evidence/native-dll-summary.json) | Named PE exports and example decorated native method symbols, without disassembly or implementations |
| [reference-revisions.json](evidence/reference-revisions.json) | Upstream commit IDs sampled during research |
| [reference-files.json](evidence/reference-files.json) | Hashes and source links for downloaded reference files |
| [verification.json](evidence/verification.json) | Final research check results and explicit untested scope |

Additional observations came from the supplied `system/Default.ini`, `MapList.ini`, PE headers, the local Steam app manifest, its existing `XIII.log` and unofficial patch credit file. These files were read in place. User-account fields and full logs were not copied into the evidence bundle. The bundled manual and readme are available for subsequent reference but were not analyzed in this pass.

The application is intended to consume installed data, not distribute it. Keep `.research/`, extracted payloads, original screenshots and recordings private/local unless their distribution rights are separately established. No formal clean-room claim is made for the development process.
