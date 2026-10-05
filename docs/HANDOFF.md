# Handoff for the next implementation agent

Updated: **2026-10-05 (second session)**. Research phase (2026-10-04), M0, M1 and the M2 first passes are done; the second 2026-10-05 session added collision, placement, BSP zones and the script VM's spawn/lifecycle/physics layer. Start with **"Next session: start here"** below.

## Next session: start here

State: all verified work is pushed to `origin/main` (`git@github.com:Yappito/XIII-Rustito.git`, `148a4bb` or later). Tasks that were still running when this was written are listed under "In flight"; check `git worktree list`, `local/logs/` and `local/reports/` for their output before starting anything new. Results are in "Implementation status", the priority list in "Exact next work".

### How the user wants work organized

- The coordinating Claude session **oversees and reviews**; it keeps its own context small and delegates implementation.
- Delegation uses the **opencode CLI**, currently **`opencode-go/deepseek-v4.1-flash`** (OpenCode Go provider, added by the user after the Ollama Cloud Pro 5-hour limit was hit; `ollama-cloud/*` models also work when that quota is available, and the user briefly preferred `glm-5.3-flash`, which exists on both providers). **On a usage-limit error, ask the user before switching provider/subscription.** If opencode is unavailable, fall back to Claude sub-agents.
- Claude writes one spec per task under git-ignored `local/tasks/`, runs opencode headless, reviews (diff, fmt/clippy/full tests incl. opt-in corpus, screenshots, semantics against UE2), sends corrections in the same session, then commits and pushes verified work to `origin/main` (commit messages end with the session's Co-Authored-By line). Ask before other outward-facing actions.
- Report honestly: a map viewer is not a playable mission, a headless trace is not gameplay.
- Credentials: the user enters API keys themselves (`opencode auth login`). Never enter a key yourself, even if pasted in chat; give the user the command instead.

### opencode workflow (verified 2026-10-05)

- Executable: `%LOCALAPPDATA%\Microsoft\WinGet\Packages\SST.opencode_Microsoft.Winget.Source_8wekyb3d8bbwe\opencode.exe` (v1.18.33; may not be on PATH). Credentials: OpenAI, Ollama Cloud, Z.AI Coding Plan, OpenCode Go.
- Command: `opencode run "<message>" -m opencode-go/deepseek-v4.1-flash --auto --title <t> [--dir <worktree>] --file local/tasks/<spec>.md`. The **message must come before `--file`** (array option swallows it). `--auto` is required headless; deny rules still apply. Corrections: `--session <id>` (`opencode session list`). Run through the Bash tool with `run_in_background` (not nohup) to get completion notices.
- `opencode.json` (committed): permission guardrails (deny edits to `XIII_Game`, Steam, `.git`, coordinator files; deny commit/push/reset/checkout/stash, installers, `cargo add/update/install`, and any bash command mentioning the protected files) and `"instructions": ["docs/AGENT_TASK_RULES.md"]`, which loads the shared delegate rules into every session.
- `docs/AGENT_TASK_RULES.md`: acceptance cases are a contract (no swapped cases), no tuning to pass, claim labels (measured / upstream / hypothesis), invariant checks (units, scale invariance), adversarial tests, a Deviations section, self-review, "do not stop early", never touch unowned files. Each rule exists because a delegate broke it once.
- Parallel tasks run in git worktrees under `local/wt/<name>` (branch per task) with a directory junction `XIII_Game -> P:\AI\XIII\XIII_Game`; use an absolute `XIII_GOG_DIR` inside the worktree. Merge `origin/main` into the task branch, verify there, push `HEAD:main`.
- Observed failure modes: models end their turn after investigating (resume with "do not end your turn until ..."); swapping the requested acceptance case; scale-invariance reasoning errors; silent no-op natives; inverted operator semantics; a delegate reverting a coordinator edit through a shell script (now denied). Review semantics, not just green tests.

### Environment notes

- cargo/rustc in `C:\Users\ZoliBen\.cargo\bin` (Git Bash: `export PATH="$HOME/.cargo/bin:$PATH"`). Rust 1.99.0 pinned, MSVC 14.44. No `gh` CLI.
- Full verification (about 2-4 minutes warm): `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && XIII_GOG_DIR=P:/AI/XIII/XIII_Game XIII_STEAM_DIR="P:/SteamLibrary/steamapps/common/XIII - Classic" cargo test --workspace`. Last result (`148a4bb`): **205 passed, 0 failed**. Use absolute paths: some older opt-in tests resolve relative `XIII_GOG_DIR` against the crate directory.
- Viewer: `cargo run -p xiii-app --release -- --map Plage01 --game-dir XIII_Game` (`--exit-after-secs N --screenshot <png>`, `--dump`, `--collision-test`).
- Reports and screenshots of each task: `local/reports/` (git-ignored; proprietary-derived outputs never committed).

### In flight when this was written

- `item1d-world-walker` (main tree): extract the Bevy-free `xiii-world` crate from `xiii-app/src/viewer/load.rs`, a `WorldPhysics` adapter (Unreal space) on `xiii-collision`, and UE2-style `walk_move` (step-up at `MAXSTEPHEIGHT` 35 UU, floor-follow, `MINFLOORZ` 0.7) so the PlayerStart doorway case can pass. Spec `local/tasks/item1d-world-crate-walker.md`.
- `item3c-new-anim` (worktree `local/wt/item3`): `New` opcode, animation natives through an `AnimationData` provider trait, `SetViewTarget`, next survey ranking. Spec in that worktree's `local/tasks/`.
- `item6-audio` (worktree `local/wt/audio`): `xiii-audio` crate, HX `.hxc`/`.hsc` parsing, one codec decode, `.uax` linkage. Spec `local/tasks/item6-audio-spike.md`.

## User intent

Re-engineer XIII Classic (2003) into Rust using Bevy so it remains playable on modern systems. Use [iw4L](https://github.com/vladtrc/iw4L) as the reference approach. The user's clarification was to import assets from an existing owned game installation, as that project does. The first requested phase was investigation, design documentation and guidelines; this handoff closes that phase.

The environment is primarily Windows with WSL2. A GOG installation lives inside this project, and an additional Steam installation is available. Preserve both. Campaign fidelity is the investigator's default milestone sequence; the user did not explicitly choose it over multiplayer. Direct installation import **was** explicitly preferred.

## Read in this order

1. [`../AGENTS.md`](../AGENTS.md): persistent project guidelines.
2. [`INVESTIGATION.md`](INVESTIGATION.md): facts, limitations and evidence.
3. [`DESIGN.md`](DESIGN.md): proposed importer/runtime architecture.
4. [`ROADMAP.md`](ROADMAP.md): gates defining meaningful progress.
5. [`DEVELOPMENT.md`](DEVELOPMENT.md): platform setup and verification.
6. [`SOURCES.md`](SOURCES.md): upstream references and provenance.

## Implementation status (updated 2026-10-05)

Implementation started on 2026-10-04. M0 and M1 are complete; first passes of all three M2 spikes (M2a world, M2b animation, M2c script) landed on 2026-10-05. Work log lives in this section; keep it current.

### Toolchain (installed 2026-10-04, native Windows)

- rustup 1.29.0 with Rust **1.99.0** (`x86_64-pc-windows-msvc`), pinned in `rust-toolchain.toml` (Bevy 0.19.1 MSRV is 1.95).
- Visual Studio 2022 Build Tools (winget `Microsoft.VisualStudio.2022.BuildTools`, VCTools workload): MSVC **14.44.35207**, Windows SDK 10.0.22621 + 10.0.26100.
- Git repository: `main` tracks `git@github.com:Yappito/XIII-Rustito.git` (repo-local author `Yappito`). `.gitattributes` forces LF.

### Cargo workspace

| Crate | Role | State |
|---|---|---|
| `crates/xiii-package` | Bounded, filesystem-free UE2 v100 package reader (no deps) | Summary/GUID/generations, names, imports, exports, object paths, contextual errors, state frames, tagged property blocks (`read_object_properties`, `read_property_block`) with native-tail reporting |
| `crates/xiii-install` | Read-only installation open/profile detection/case-insensitive logical index (no deps) | Done for GOG + patched Steam; no precedence rule needed (no duplicates found) |
| `crates/xiii-decode` | Payload decoders to normalized assets (textures, palettes, static meshes, BSP Model/Polys, terrain, skeletal meshes, animation) and the single Unreal-to-Bevy coordinate policy (`common.rs`) | M2a/M2b first pass; see below |
| `crates/xiii-script` | UStruct/UClass/UFunction/UState/UProperty reflection, bytecode decoder/disassembler, native catalog, `class_defaults()`, minimal VM + native registry | M2c first pass; see below |
| `crates/xiii-collision` | Dependency-free collision: BVH, SAT swept AABB (two-sided; start contact skipped only when not moving into it), ray, overlap, `move_slide` (approximation) | Added 2026-10-05 (second session) |
| `crates/xiii-tool` | CLI: `inspect`, `corpus`, `props`, `coverage`, `deps`, `world-coverage` (+ world dump commands), `anim coverage/list/validate/render/export`, `script classes/functions/disasm/natives/coverage/run` | Working |
| `crates/xiii-app` | Bevy 0.19.1 runtime: M0 smoke test (default) and diagnostic map viewer (`--map Plage00 --game-dir <root>`) | Working natively |

Each crate has a README with verified layouts, evidence and rationale.

### M2 spike results (2026-10-05) — measured, not playable

**M2a world** (`xiii-tool world-coverage XIII_Game`; `cargo run -p xiii-app --release -- --map Plage01 --game-dir P:/AI/XIII/XIII_Game`)

| Class | Decoded (GOG) | Payload |
|---|---|---|
| Texture | 3507/3507 (DXT1 2791, DXT3 427, DXT5 214, RGBA8 46, P8 13, G16 12, RGB8 3, G8 1); all mips to RGBA8 | exact |
| Palette | 14/14 | exact |
| StaticMesh | 6128/6128 (verts, normals, UVs, sections, indices, wire indices, per-triangle collision + tree nodes, simplified collision, materials) | exact; consumed-but-unknown ranges listed in README |
| Polys / TerrainSector | 7194/7194, 2208/2208 | exact |
| Model (BSP) | 7194/7194 through NumZones (vectors, points, nodes, surfs, verts) | zones/lightmaps/leaves = labelled unsupported tail (9.57 MB total) |
| TerrainInfo | 18/18 sectors, world verts, sector grid | ~115 B unsupported tail |

- Coordinates: `(x,y,z) -> (y,z,-x) / 50` in one place; rotators via basis matrices (65536/turn; roll X, pitch Y, yaw Z). Static-mesh triangles keep source order (the axis swap mirrors winding; 1,212,092 vs 758 triangles agree with stored normals); BSP polygons are reversed. Asymmetric check: the Plage01 "CLOSED FOR WINTER" sign reads unmirrored and front-facing with back-face culling. **50 units/m is uncalibrated.**
- Viewer: Plage00 (beach, cliffs, trees, jeep) and Plage01 (lifeguard hut on stilts, boardwalk, rocks, sea) are recognizable and correctly placed. Unlit diagnostic materials, CPU-blended terrain layers, no skybox (sky polygons skipped and counted), SinusModifier sea renders black, no vertex lighting, PrePivot ignored, class defaults not applied to actors. On-screen `skip./fail./note.` counters and crosshair object path. Opt-in test asserts both maps import with 0 `fail.*` counters.
- Collision: decoded but only ray-probed (Plage00 ground 1.61 m below start; Plage01 floor 0.24 m below, door `Porte6` 12.2 m ahead, walls ~1.1-1.3 m either side). **No capsule sweep / doorway test yet, so the M2a exit is not fully met.**

**M2b skeleton/animation** (`xiii-tool anim coverage XIII_Game`)

- SkeletalMesh 129/129 and MeshAnimation 96/96 decode byte-exactly (xiiipersos, xiiiarmes, xiiideco, xiiipersosG, xiiivehicule); 1,085 sequences, 294,533 rotation keys. VertMesh (3, xiiideco) not attempted.
- SkeletalMesh = UModel's legacy (v1) path plus XIII extras: 53-byte bones without NumChildren; trailing XIII hit-box array (box + u16 bone). MeshAnimation motion data is XIII-specific (UModel unsupported) and was reverse-engineered: per-bone start/count tables into shared arrays, rotation = 4 x i16/32767, plain position vectors, key times in frames; ranges tile the arrays exactly. No other compression; root speed always 0 (walk/run play in place).
- Convention: root quaternion as stored, all other bones conjugated (the only rule giving an upright T-pose with toes on the toe vertices).
- XIIIM + MigA: weights sum 1.0000 (max 3 influences), bind skinning error 6e-5; walk/run/idle stable and periodic (wireframe renders checked visually). SlaterM has 10 weights to bone 0xFFFF (dropped and renormalized); 8 notify times outside 0..1. **Not yet compared against original-game frames**; loop interpolation, root-motion source and MeshScale/RotOrigin order are unknown.

**M2c script** (`xiii-tool script coverage XIII_Game`; `xiii-tool script run --game-dir XIII_Game --trace`)

- All script lives in the 19 `.u` packages (licensee 58). 31,960 script objects (6,903 functions, 532 states, 1,444 classes, 92 structs, 22,632 properties), 445,474 tokens, each ending exactly at its declared script size; 0 failures, **no XIII-specific opcodes**. Patched Steam: 33 packages, 584,712 tokens, clean.
- XIII deltas from stock v100 (cross-checked with UELib@3207a17e): 24-bit function flags with a different bit order (verified against the unstripped `engine.u` source on 1,765 declarations; XIII-only `debugonly` 0x20000), u16 state flags, 18-byte class flags+GUID (split inferred), i16 ArrayDim.
- `class_defaults()`: 1,444/1,444 Core.Class defaults decode to export end, so property coverage is now 141,939/141,939 (GOG) and 149,847/149,847 (Steam). Two-byte array indices occur only in class defaults (130, max 254).
- Native catalog: 894 natives (418 by index, 476 by name; 8 latent, 15 iterators); 880/894 match `?exec` DLL symbols. Duplicate native numbers (203, 472-476) exist and are resolved by argument count (`AmbiguousNative` error otherwise). Evidence: `docs/evidence/script-coverage-gog.json`.
- Minimal VM (no Bevy/filesystem): loads all 371 Plage00 actors with class defaults + map properties (0 failures); state-aware virtual dispatch, labels, latent `Sleep`, iterators, out params, Accessed-None semantics, per-tick budget, explicit errors with script stack traces. 47-entry native registry with evidence/status.
- A real authored chain runs unmodified: `TouchTrigger2.Touch` -> `DynamicActors` -> `XIIIDispatcher0` enters `Dispatch`, `Sleep`s, resumes, triggers BaseSoldier15/14 (logged **DEFERRED**, out of scope) and ends in `Fin`. Widening the scope to soldiers fails explicitly at `UnimplementedNative Actor.Spawn (#278)`. Touch is delivered by the harness (no collision yet).
- **Go/no-go: GO** on retaining compiled campaign logic. Remaining cost is the native layer (~200 distinct native numbers called by game packages), not the VM. Native semantics have not been checked against original-engine behavior.

### Second 2026-10-05 session results — measured, not playable

Per-task reports with all numbers and commands: `local/reports/item1-collision.md`, `item1c-placement.md`, `item1b-bsp-tail.md` (in `local/wt/bsp`), `item3-spawn-lifecycle.md`, `item3b-physics-bridge.md` (in `local/wt/item3`).

**Collision / doorway / scale (M2a exit, partial)** — `cargo run -p xiii-app --release -- --map Plage01 --game-dir XIII_Game --collision-test`
- Player: `XIII.XIIIPlayerPawn` (via `Default.ini DefaultGame=XIII.XIIIGameInfo` -> `DefaultPlayerClassName`); inherited defaults CollisionRadius 34, CollisionHeight 75 (half), BaseEyeHeight 60, GroundSpeed 472, JumpZ 420. `xiii-tool script defaults <pkg> <Class>` prints resolved inherited defaults.
- Extent-box primitive (UE2 world collision uses an axis-aligned extent box; upstream claim, unverified for XIII). 24,369 Plage01 collision triangles, BVH build ~9 ms.
- `Porte6` (`XIDCine.Porte`, mesh `StaticPlage2.Pl_porte01T`): **aligned door case PASS** (closed: blocked by Porte6; open: passes 27.7 m beyond). Clear opening ~2.56 m x 4.16 m at 50 u/m (128 x 208 UU). **PlayerStart case FAIL**: walker stuck on a 1.8 UU plank edge of `GR_interieur01` (no floor-following/step-up in `move_slide`); being fixed by `walk_move` (in flight).
- Scale (fit is scale-invariant; only metres change): pawn 150 UU tall, visible mesh `xiiipersos.XIIIM` bind pose 160.3 UU, door 208 UU. Human-size assumptions give **~85-105 UU/m**, i.e. the 50 u/m constant is ~2x too small. **Not changed yet** — decide with original-engine captures or accept the estimate (a coordinator/user decision).
- Placement: map property -> inherited class default -> engine default, counted per field (`placement.*` counters). PrePivot applied (`T(L) R S T(-PrePivot)`) but 0 placed mesh actors on Plage00/01 have one, so sign/order is not empirically verified. PlayerStart rests on the floor per the UE2 rule (map CollisionHeight 10, residual +2 UU). Static-mesh collision set 0 equals render geometry for `GR_interieur01`.

**BSP zones** — `xiii-tool zones XIII_Game/Maps/Plage01.unr`
- After NumZones: u32 (always 0), NumZones variable-length zone records `{compact ZoneActor, u64 Connectivity, u64 (labelled Visibility; values look like uninitialised floats, semantics unverified), f32 LastRenderTime}`, then the Polys reference. 7,194/7,194 Models; zone actors ZoneInfo 334 / SkyZoneInfo 35; Connectivity invariant 1,350/1,350; leaf->zone from nodes 0 conflicts. Sky zones: Plage00 zone 3, Plage01 zone 4. Remaining tail `model.lightmaps_and_after` 9,496,581 B (arrays after Polys fail on the 13 maps with non-empty LightMap; not guessed).

**Script VM growth (M2c -> M3)** — `xiii-tool script run --game-dir XIII_Game --map Plage00 [--begin-play] [--survey] [--physics flat:0]`
- `Actor.Spawn`/`Destroy`, lifecycle tables (level start: PreBeginPlay, BeginPlay, PostBeginPlay, PostNetBeginPlay, SetInitialState; runtime spawn adds Spawned first; cross-actor grouping is a hypothesis), GameInfo from `Default.ini` + `InitGame` (spawns XIIISoloMutator etc.).
- Exact omitted-optional-argument presence (EX_Nothing) for natives; ~45 more natives (strings with FString clamping, `~=` case-insensitive equality, case-sensitive `InStr`, vectors/rotators, AllActors, pawn/controller lists, seeded deterministic FRand/Rand, DynamicLoadObject with class check, native-class identity).
- `WorldPhysics` provider trait (Unreal space) + `Move`/`SetLocation`/`Trace`/`FastTrace`/`SetCollision`/`SetCollisionSize`/`TouchingActors`; actor touching from XIII's decoded `Actor.TouchingActor` cylinder test with symmetric Touch/UnTouch. Without a provider these fail explicitly. `--physics flat:<z>` is a diagnostic floor only.
- Plage00 all-classes survey: 3 missing natives left (`LinkSkelAnim`, `LoopAnim`, `SetViewTarget`), then the `New` opcode (in flight: item3c). The Plage00 dispatcher chain with soldiers active runs to `Fin` past `Actor.Spawn`.
- Semantics references: UE2 conventions; `dpjudas/SurrealEngine@380f525` (UE1 reimplementation) consulted for Move/Trace return conventions, no code copied.

### Verified results

| Check | Result |
|---|---|
| `xiii-tool corpus XIII_Game --compare docs/evidence/gog-inventory.json` | 196/196 packages match (version/licensee, name/import/export counts, per-class counts, sizes); 0 errors |
| Same against Steam + `steam-inventory.json` | 213/213 match; 0 errors |
| Header layout | 36 fixed bytes + 16-byte GUID + i32 generation count G + G×(exports, names); header ends at 56+8G == first table offset in all 409 files; latest generation == table counts in all files; header+tables+export spans cover every byte, no gaps/overlaps |
| Installation profiles | GOG (`system/PC`, `TexturesPC`, flat `Maps`) and SteamPatched (`Textures`, `Maps/BaseSP`/`BaseMP`, 8 `*Plus` packages, "unofficial patch 1.4" credits) detected by content; 0 duplicate logical names in either install; Plage00/01, StaticPlage2, XIIIPersos, XIDMaps, XIII, XIDCine, Engine, Core resolve within their own root |
| Native Bevy smoke (`cargo run -p xiii-app -- --exit-after-secs ...`) | RTX 4090, Vulkan default (DX12 works with `WGPU_BACKEND=dx12`); 1280×720; 8.3 ms vsync / 2.0 ms uncapped dev build; runtime-generated WAV tone plays; screenshot path works; mouse motion captured live. Keyboard/mouse-button events not yet exercised live |
| Property coverage (`xiii-tool coverage XIII_Game`) | 141,939 exports; 140,498 decode without anomalies; 1,441 failures, all `Core.Class` (native UClass data precedes defaults — expected). 301 classes decode exactly to payload end; 62 classes leave native tails (StaticMesh, Texture, Model, Polys, TerrainSector, Function/State/TextBuffer, `*Property`, NavigationPoint subclasses 20–55 B). Report: `docs/evidence/property-coverage-gog.json` (metadata only) |
| Property layout facts | State frame = compact node, compact state node, u64 probe mask, u32 latent action, compact offset if node≠0 (all 58,479 observed: 15/17 B, offset −1). Bools: size code 5 with explicit size byte 0. Structs are raw member data (not tagged; UELib: tagged from v118). Multiple identical `None` name entries exist → terminator matched by text. Sources: UModel `UnObject.cpp` @a0bfb468, UELib @3207a17e |
| Object-level deps (`xiii-tool deps Maps/Plage0X.unr --game-dir XIII_Game`) | Plage00: 17/17 packages, 151/156 imports resolve to exports; Plage01: 20/20, 346/351. The 5 unresolved in each are `Engine.Level/Model/Polys/StaticMeshInstance/TerrainSector` classes with no export in `engine.u` (probably native-only, unproven). Every StaticMeshActor has decoded Location and a StaticMesh import resolving to e.g. `StaticPlage2.rocher01` |
| Unit/integration tests | Workspace: 153 tests passing with opt-in corpus vars set (2026-10-05). Earlier breakdown: xiii-package 50 unit + doc; xiii-tool synthetic corpus/deps + opt-in `local_corpus`/`local_properties`; xiii-install 20 unit + 4 opt-in integration (pass with `XIII_GOG_DIR`/`XIII_STEAM_DIR`); xiii-app 4 |
| Build times (cold) | `cargo build -p xiii-app` 7m14s dev, 4m37s release; ~410 deps; release binary 70 MB |

Opt-in corpus tests: set `XIII_GOG_DIR=P:/AI/XIII/XIII_Game` and `XIII_STEAM_DIR="P:/SteamLibrary/steamapps/common/XIII - Classic"` before `cargo test --workspace`; without them they print `SKIPPED`.

### Open questions from this phase

- How the original engine finds GOG `system/PC`/`TexturesPC` when `Default.ini` `Paths=` omits them (likely `PlateForm=0` + `SpecificPackage=`) is unverified.
- Corpus comparison checks sizes and all probe fields, not SHA-256.
- Reader accepts non-minimal/negative-zero compact indices (matches probe).
- Class defaults inside UClass payloads not yet located (needs UStruct/UClass serializer → M2c). Array elements/unknown structs kept as raw spans. `Color` channel order unverified. `deps` uses file-name lookup, not `xiii-install`.

## Research-phase artifacts (still present)

- `tools/probe_install.py`, `tools/build_evidence.py`, `tools/test_probe_install.py`: Python research probes (standard library only).
- `docs/evidence/`: inventory hashes, class counts, comparisons, focus-package summaries and source-revision metadata. No extracted payloads.
- Ignored `.research/`: downloaded public reference files used in the investigation; not a runtime dependency.
- No original game, editor, UCC or external extractor has been run. Both installations were only read.

## Findings that should shape implementation

1. **Direct package access works at the table level.** All 196 GOG packages and 213 local Steam packages parsed without table/reference/range errors. Versions are 100/50, 100/56, 100/57 and 100/58. Object payloads are still mostly unknown.
2. **The Steam copy is patched.** It contains unofficial patch 1.4 credits and additional `*Plus` packages. Its flattened package layout and `Maps/BaseSP` / `BaseMP` structure differ from GOG. All 64 maps and 99 HX bank/stream files nevertheless match by SHA-256. Do not call this a pristine Steam release or merge it into GOG implicitly.
3. **Opening maps include terrain.** Plage00/Plage01 have BSP/model objects, many static meshes, and TerrainInfo/TerrainSector exports. A static-mesh-only importer is insufficient.
4. **Animation is inside `.u` packages.** `system/PC/xiiipersos.u` contains skeletal meshes and MeshAnimation exports. The convenient UModel compatibility table does not promise XIII animation or static-mesh support.
5. **Gameplay source text is stripped.** The inspected game-specific TextBuffers are tiny placeholders, while class/function/state metadata remains. Plan a bytecode/native-API compatibility spike, not automatic source translation. Actual function-bytecode deserialization is unverified.
6. **Audio is a separate subsystem.** The `.uax` files total only about 424 KB; the HX bank/stream files total about 1.86 GB. vgmstream has explicit XIII support upstream, but local decode and sound-ID mapping still need testing.
7. **The runtime must replace engine behavior.** Existing 32-bit DLLs contain native engine/game operations. An x64 Bevy renderer does not make those operations available automatically.
8. **Bevy baseline is current to this research date.** Verified release 0.19.1; tagged MSRV 1.95.0; optional Avian 0.7 targets Bevy 0.19 upstream. No local native build has proved that stack yet.

## Locations

| Purpose | Windows | WSL |
|---|---|---|
| Project | `P:\AI\XIII` | `/mnt/p/AI/XIII` |
| GOG baseline | `P:\AI\XIII\XIII_Game` | `/mnt/p/AI/XIII/XIII_Game` |
| Patched Steam comparison | `P:\SteamLibrary\steamapps\common\XIII - Classic` | `/mnt/p/SteamLibrary/steamapps/common/XIII - Classic` |

Native Windows reports an RTX 4090. Rust/Cargo were not found on the inspected WSL or Windows PATH. Windows's standard Visual Studio installation locator was absent. Recheck before installing; absence from PATH is not proof of no installation.

## Exact next work

M0/M1 done; M2a/b/c first passes done; second 2026-10-05 session closed most of the M2a exit and grew the VM toward M3 (see above). Remaining work, in priority order:

1. **Finish the M2a exit:** review/land `item1d` (`xiii-world` crate, `WorldPhysics` adapter, `walk_move`); the PlayerStart doorway case must pass legitimately or be explained. Then wire the real map physics into `xiii-tool script run` (`--physics map`) so `Move`/`Trace` run against Plage00/01 collision. Decide the units-per-metre constant (estimate ~85-105 UU/m vs current 50; needs a user/coordinator decision or original-engine captures). Sky zone handling in the viewer using the decoded zones.
2. **Animation in the runtime:** land `item3c` (`New`, animation natives via `AnimationData`, `SetViewTarget`); then implement `AnimationData` from decoded MeshAnimation (`xiii-decode`), upload a skinned mesh + clip into Bevy in the viewer, place map Pawns with their meshes; compare frames against original-game captures.
3. **Native layer growth (M3):** continue the survey ranking after item3c in campaign-dependency order (AI/pathing next); timers; integrate the VM with the Bevy fixed-step schedule, one authoritative owner per field (VM owns script state; physics owns positions via the provider).
4. **Original-engine reference captures** (needs the user / an isolated writable copy): opening map order, movement speed/scale, event timing for the Plage00 dispatcher chain. This would also settle the scale constant.
5. **Materials:** skybox from the sky zone, translucency/modulation, SinusModifier, vertex lighting (BSP lightmap arrays still undecoded), then the comic outline treatment.
6. **Audio:** land `item6` (HX bank parsing/decoding spike), then Bevy playback and `.uax` sound-object mapping.

Do not stop at a generic Bevy FPS sample. Do not report playable progress from the map viewer or the headless VM trace.

## Validation completed

| Check | Result |
|---|---|
| GOG inventory | 344 asset/binary/INI hashes; 196 package-table parses; 0 errors |
| Steam local inventory | 376 asset/binary/INI hashes; 213 package-table parses; 0 errors |
| Unique asset-basename comparison | 310 identical, 4 different, 17 Steam-only, 0 GOG-only, 0 ambiguous |
| GOG top-level imported package stems | 0 missing stems; object-level resolution not tested |
| Focus metadata | 8 packages reread and matched to baseline hashes |
| Synthetic research-tool suite | 8 tests passed |
| Geometry / animation / scripts / native Windows runtime | See "Implementation status" (2026-10-05) |
| Audio | Not implemented or tested |

The script tests only verify the research reader's implemented boundaries. They are not a full fuzzing campaign, a complete package validator or evidence of decoded assets. See `DEVELOPMENT.md` for reproducible commands.

## Update this file when continuing

Replace the current-state and next-work sections as implementation advances. Record exact commands/tests, successful maps/behaviors, unsupported cases, changed dependency versions and any architectural decision. Keep completed measurements distinct from proposals. Preserve the original-installation and source-provenance rules.
