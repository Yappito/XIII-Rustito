# Handoff for the next implementation agent

Updated: **2026-10-05 (second session)**. Research phase (2026-10-04), M0, M1 and the M2 first passes are done; the second 2026-10-05 session added collision, placement, BSP zones and the script VM's spawn/lifecycle/physics layer. Start with **"Next session: start here"** below.

## Next session: start here

State: all verified work is pushed to `origin/main` (`git@github.com:Yappito/XIII-Rustito.git`, `9903901` or later). Tasks that were still running when this was written are listed under "In flight"; check `git worktree list`, `local/logs/` and `local/reports/` for their output before starting anything new. Results are in "Implementation status", the priority list in "Exact next work".

### How the user wants work organized

- The coordinating Claude session **oversees and reviews**; it keeps its own context small and delegates implementation.
- Delegation uses the **opencode CLI**, currently **`opencode-go/deepseek-v4.1-flash`** (OpenCode Go provider, added by the user after the Ollama Cloud Pro 5-hour limit was hit; `ollama-cloud/*` models also work when that quota is available, and the user briefly preferred `glm-5.3-flash`, which exists on both providers). Up to six sessions ran in parallel (three on `opencode-go/deepseek-v4.1-flash`, three on `ollama-cloud/deepseek-v4.1-flash` once that quota reset). **On a usage-limit error, ask the user before switching provider/subscription.** If opencode is unavailable, fall back to Claude sub-agents.
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
- Full verification (about 2-4 minutes warm): `cargo fmt --all -- --check && cargo clippy --workspace --all-targets -- -D warnings && XIII_GOG_DIR=P:/AI/XIII/XIII_Game XIII_STEAM_DIR="P:/SteamLibrary/steamapps/common/XIII - Classic" cargo test --workspace`. Last result (`9903901`): **747 passed, 0 failed**. Use `bash P:/AI/XIII/local/verify.sh` (git-ignored helper: fmt + clippy + full tests with real exit status) before every push; piping cargo into `tail`/`awk` once masked a compile failure and broke main. Use absolute paths: some older opt-in tests resolve relative `XIII_GOG_DIR` against the crate directory.
- Viewer: `cargo run -p xiii-app --release -- --map Plage01 --game-dir XIII_Game` (`--exit-after-secs N --screenshot <png>`, `--dump`, `--collision-test`, `--reach-test`, `--lighting off|baked`). Character: `--model xiiipersos.XIIIM --anim Walk`. Movement + VM prototype: `--play --map Plage01 [--play-script <file>]` (E = use). Headless VM: `xiii-tool script run --game-dir XIII_Game --map Plage00 --begin-play --physics map --anim map --nav map --events`.
- Disassembler (user-approved): `llvm-objdump` from `rustup component add llvm-tools`, at `C:/Users/ZoliBen/.rustup/toolchains/1.99.0-x86_64-pc-windows-msvc/lib/rustlib/x86_64-pc-windows-msvc/bin/llvm-objdump.exe`. DLLs are read only; disassembly output stays in git-ignored `local/re/`, never committed or pasted into code.
- Launch parallel opencode runs a few seconds apart: simultaneous starts fail with `database is locked`.
- Reports and screenshots of each task: `local/reports/` (git-ignored; proprietary-derived outputs never committed).

### In flight when this was written

- `item20b` save/load fidelity (worktree `local/wt/sweep`, uncommitted; opencode session `item20b-save-fidelity`): menu Load/Continue via `SaveSlotProvider` works; the checkpoint-load `StartSpotEvent = "LOAD"` correction was cut off by a provider limit - resume it, remove the t=0.20 s re-apply bridge, keep health 150 for 10 s and Plage00's normal-start 75.
- `item16c` menu completion (worktree `local/wt/codex1`, partial Codex CLI edit in `canvas.rs` only; the Codex workspace ran out of credits).
- Specs in `P:/AI/XIII/local/tasks/`; logs in `P:/AI/XIII/local/logs/`; reports in each worktree's `local/reports/`.

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
| `crates/xiii-collision` | Dependency-free collision: BVH, SAT swept AABB, ray, overlap, `walk_move` (UE2-style step-up/floor-follow), moving objects (movers) | Working |
| `crates/xiii-world` | Bevy-free world import (scene, placement from class defaults, box/line collision soups, zones/sky, navigation/ReachSpecs, baked vertex lighting) and runtime (script/map loading, physics/animation/navigation providers, begin-play) | Working |
| `crates/xiii-audio` | Dependency-free HX `.hxc`/`.hsc` parser, PCM16 + Ubisoft ADPCM decoders, WAV writer | Spike done |
| `crates/xiii-tool` | CLI: `inspect`, `corpus`, `props`, `coverage`, `deps`, `world-coverage` (+ world dump commands), `anim coverage/list/validate/render/export`, `script classes/functions/disasm/natives/coverage/run` | Working |
| `crates/xiii-app` | Bevy 0.19.1 runtime: smoke test, map viewer (sky camera, baked lighting), skinned character viewer, headless collision/reach tests, `--play` movement + VM prototype | Working natively; prototype, not gameplay |

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

### Later 2026-10-05 results (OpenCode Go + Ollama Cloud delegates) — measured, not playable

- **Scale:** `UNREAL_UNITS_PER_METER` = **90** (estimate approved by the user; evidence: pawn 150 UU, XIIIM 160.3 UU, door 208 UU). Harness distances are in UU so tests are scale-independent.
- **Collision validated against the map's own navigation:** ReachSpecs are a native `PathList` tail on NavigationPoints (`compact count + {Start, End, u16 R, u16 H, u32 reachFlags, u16 Distance}`; 993 edges exact). `--reach-test` over every eligible walk edge: Plage00 12/12, Plage01 294/302, Banque01 631/658. StaticMesh collision per UE2 defaults: box queries use simplified set 1 (`UseSimpleBoxCollision` default true; the corpus sets it false on 167 meshes, never true), line queries per-polygon unless `UseSimpleLineCollision` (set true on 418).
- **Rendering:** GPU-skinned decoded characters (`--model`), CPU/GPU error <= 1.3e-4 UU; UE2 skybox via a second camera at the `SkyZoneInfo`; baked per-vertex lighting from `Engine.StaticMeshInstance` (14,542/14,542 exact; BGRA) and terrain colours. BSP lightmaps still undecoded.
- **Script VM toward M3:** real map physics/animation/navigation providers; AI controller possession (soldiers get `IAController`, enter `Init`, request decoded `WaitNeutre`); presentation event queue (PlaySound/PlayMusic/texture swaps); UE2 pathing natives over ReachSpecs (Dijkstra, latent MoveTo/MoveToward); seven XIII AI natives from `XIDPawn.dll` disassembly (`Partial` where engine state is unnamed; the report has an inconsistent +0x3B0 Pawn/BaseS label in `HalteAuFeu`); movers (`PHYS_MovingBrush` interpolation, `FinishInterpolation`). Plage00 soldiers reach `Patrouille`.
- **`--play` prototype:** first-person movement at decoded `XIIIPlayerPawn` speeds (60 Hz fixed step, gravity from `PhysicsVolume`), the VM running all map actors (failing actors suspended, not crashing), ownership rule (host owns the player pawn's movement fields; the VM owns everything else). On Plage01 the player opens `Porte6` through the game's `Grab` -> `Locked.Trigger` -> `DoOpen` chain and leaves the hut. **Shortcuts still in place:** the player pawn is host-spawned (script login blocked by `GetLocalURL`; item3h), the door key is host-granted (pickup/inventory natives missing), object references into packages outside the script set read as `None` (loses sound paths; needs an external-reference value).
- **Audio:** all 59 HX banks parse with 0 unparsed bytes; 28,631/28,631 waves decode (PCM16 + 6-bit Ubisoft ADPCM); `.uax` Sound -> HX entry by name (4,027/5,892).
- Per-task reports with commands and numbers: `P:/AI/XIII/local/reports/item*.md` and the worktrees' `local/reports/`.

### Third 2026-10-05 block (up to `1b16fd4`) — measured, prototype, not a playable mission

- **Plage00 opening runs from the game's scripts in `--play`:** the VM now dispatches the per-frame `Tick` event (it never did); `XIIIBaseHud.DrawHUD` sets `MapInfo.EndCartoonEffect` (Plage00/01 `InitialCartoonEffect == 0`), `Cine2.CineInit` proceeds, Pam leans over XIII and the line *"I can't remember a thing..."* (`plage00_XIIIb_00`) plays with subtitles at 0.28 s, input frozen by the cutscene. The comic-panel cartoon effect is implemented (render-to-texture bridge) but not yet observed live (Base01 sets `HudCartoonSFX` without panel updates).
- **Plage01 hut exit by the game's own logic:** script login spawns the player pawn (one pawn; the 12 extra were `Engine.Camera` cutscene controllers restarted by `StartMatch` - fixed with the native `bOnlySpectator` default), the key is picked up by walking (pickup/inventory chain), `E`=Grab opens `Porte6`, the HUD shows "Checkpoint reached", lock/door sounds and the `Plage01__hSortCahutte` music cue play. Remaining host workaround: `Vm::settle_pickups` (labelled hypothesis).
- **Combat core:** `XIIIWeapon.Fire` -> `RealTraceFire` -> `ProcessTraceHit` -> `TakeDamage` -> `Died` kills `BaseSoldier6` on Plage01 (75 hp/shot); first-person Beretta view. Limits: diagnostic weapon grant, hit zones from the collision cylinder (every hit reads as head), `PlayFiringSound` and `RefreshLighting` Partial.
- **Movement modes:** crouch (C=Duck), ladders (`LadderVolume`), swimming (`WaterVolume` brushes; XIII zones have no water flags), falling damage through the pawn's own `Landed`/`TakeFallingDamage`.
- **Robustness:** campaign sweep (`xiii-tool campaign`, 35 maps in `MapInfo.NextMapLevelWithUnr` order): suspended actors 412 -> 3, missing natives 0; reach-walk 22,345/23,265 navigation edges (terrain: concatenated multi-region vertex arrays; only the base grid is used, per `Engine.dll` `ATerrainInfo::LineCheck`/`Render`). Steam patched install: identical results to GOG on all 35 maps (INI `EditPackages` load order).
- **Presentation:** HUD via script `PostRender` + 27 Canvas/HUD natives + decoded fonts; localisation (`xiii-locale`, XIII `localized` flag is `0x00400000`); audio: Sound -> HX by GUID resource reference (96.7%), music streamed from `.hsc`, ambient emitters with XIII roll-off; materials (blend/two-sided/animated UVs); baked vertex lighting (static meshes, terrain); particles (815 systems; triggered emitters start inactive per `TrigerredEmitter.PostBeginPlay`); BSP Model tail decoded from `UModel::Serialize` (6,396/7,194 byte-exact; `LightMap` empty in 7,192).
- **Performance:** VM tick 2.6 ms -> 0.36-0.52 ms (Banque01, ~900 actors); all maps > 240 FPS release on the RTX 4090.
- **Scale:** 90 UU/m (user-approved estimate).

### Fifth block (2026-10-06, up to `9903901`) — measured, prototype, not a playable mission

- **Plage01 start to end by the game's own logic (headless route test):** the level-start cutscene, both objectives (`objectif91`/`92` fired by the game's chains), corpse search for the truck key, `Porte1`, then `XIIIGameInfo.EndGame` -> `XIIIPlayerController.GameEnded` -> `GameEndedSuccess` -> `PlayingVideo` (`cine01`, real Bink duration) -> `ServerTravel("banque01.unr")` at 123.4 s. No `take_control`/`set_goal` bridges on the route; remaining host decisions: diagnostic teleports and an `XIII.m60` grant in the route script, tick-scope restores, the game-end cutscene stop. Root causes fixed from Engine.dll: list natives never clear the removed node's link (`execRemoveController` 0x10367ac0, `execRemovePawnFromList` 0x103b02e0); plain variable reads through a just-destroyed actor return the stale value (`execContext` 0x101173a0, `CleanupDestroyed` 0x10387ae0); `PlayerTick` dispatch (state overrides only); exact-class `FindInventoryType`; spawn event order measured in `ULevel::SpawnActor` (PostNetBeginPlay after PostBeginPlay, skipped on clients).
- **Plage00 intro:** plays to control return (voice lengths from decoded waves; 0.28 s / 3.1 s lines, control at 6.2 s).
- **Video (user decision: clean-room):** new crate `xiii-video`. Bink 1 video decoder byte-exact (Y/Cb/Cr) vs a black-box FFmpeg oracle on all sampled frames, all 19 cutscenes / 24,167 frames, 0 errors; Bink audio (DCT variant) 100,190 packets, 0 errors, 85.8-92.4 dB (DLL floor/clip cross-fade rounding); decoder tables are read at runtime from the installation's `binkw32.dll`, never stored in the repo. `--video` plays with the audio clock as master. An earlier FFmpeg-derived attempt was rejected and archived outside the repo (`local/archive/`), never merged.
- **Combat/weapon:** the diagnostic grant runs the game's own `GiveTo`; the muzzle flash comes from the game's chain (no host bridge); the shooter's own attachment no longer blocks its trace; calls dropped on suspended actors are traced and counted.
- **Menus:** UnrealScript delegates in the VM; the menu is driven by the game's own `InitComponent`/`OnDraw` delegates with the real panel layout.
- **Save/load (partial):** checkpoint save files in a user directory from the game's `DoSave`/`SaveAtCheckpoint`; `--play --load N`. In progress (`local/wt/sweep`, item20b, uncommitted): menu Load/Continue through a `SaveSlotProvider`, and making checkpoint loads set `StartSpotEvent = "LOAD"` the way the game does (`Plage00.FirstFrame` skips the wounded-health write only then).
- **World data:** all 7,194 BSP Models decode byte-exact; reach classification (closed movers labelled, ladder/jump edges excluded): 98.6 % of walk-testable edges pass.
- Full verification at `9903901`: 747 tests passed; Plage00 `--trace` byte-identical.
- **Delegation note:** OpenCode Go (weekly), OpenAI `gpt-6-luna`, Ollama Cloud (5 h) and Z.AI (5 h) all hit limits on 2026-10-06; the Codex CLI workspace (second OpenAI subscription) had no credits. Hard bit-level reverse engineering (Bink) succeeded only with Claude sub-agents. Codex CLI headless needs `-c 'windows.sandbox="unelevated"' -s workspace-write` (helper `local/codex-run.sh`) and worktrees without the `XIII_Game` junction.

### Fourth 2026-10-05 block (up to `3d97dc8`) — measured, prototype, not a playable mission

- **Combat (item14b):** hit zones from decoded SkeletalMesh bone boxes posed by the current animation (MiocheM: head 75 dmg, spine 31); `Weapon.PlayFiringSound` emits a positional `PlaySound` event; AI perception (`SeePlayer`/`EnemyNotVisible` from `SightRadius`/`PeripheralVision`/line of sight), `SetEnemy` raises `EnemyAcquired`, 10 AI natives, independent `Timer`/`Timer2`/`Timer3`. Base01 `BaseSoldier17` runs `Tenir -> Acquisition -> Attaque`, fires its M16, player Health 150 -> 90. Labelled host bridges: AI `Fire` re-issue (engine `AWeapon::Tick`) and focus rotation. Plage00/01 soldiers are ordered to the scripted `faction` stasis and never fight. The Plage01 Beretta pickup is not reached by walking (player stops 80 UU short; diagnostic grant kept) - investigated in item14c.
- **Campaign residuals (item3p):** 0 suspended actors on all 35 maps (per-instance class-default subobjects for `BreakableMover`, unknown animation = UE2 visible no-op); rotator `*`/`/`/`==`/`!=` and float `**` natives.
- **Collision (item1j):** extent-box pawn primitive confirmed in `Engine.dll` (`MINFLOORZ` 0.7, `MAXSTEPHEIGHT` 35, `physWalking` sub-step bound 8); reach 22,345 -> 22,353. item1k (porting `physWalking`/`stepUp` structure + `ULevel::FindSpot`) measures 22,504 and walkable-ledge stalls 136 -> 35; its evidence wording is being corrected before merge.
- **Footsteps (item6e):** `XIIIPlayerPawn.PlayFootStep` reads the floor material's `XIIIFootStepSound`; the host synthesises the notify cadence from the decoded `Run`/`Walk` clips (no third-person anim in `--play`): Plage00 sand `XIIIFSSab`, Plage01 planks `XIIIFSBoi`, Banque01 marble `XIIIFSMar`. The 195 unresolved sound references are not resolvable from the shipped data (reasons pinned by an opt-in test).
- **Rendering:** BSP `FBspVertexStream` decoded (32 B: position, 4-byte flags-or-colour, uv0, uv1; position-validated on Plage00/01/Banque01; the 4-byte field is white at corners and 0 at collinear vertices, so it is **not** rendered as lighting); variable-length `LightMapBits`; labelled tail 4.45 MB -> 1.34 MB. Dynamic lights (item5h): decoded `Light` actors, UE2 HSB colour, additive light-only pass over the unlit baked materials, VM lights follow actors in `--play`; Beam/Spark emitters. Labelled muzzle-flash presentation bridge (grant skips `AttachToPawn`; item14c removes it).
- **Menus (item16):** `--menu` loads `MapMenu`, spawns `XIDInterf.XIIIRootWindow`/`XIIIMenu`, runs their decoded draw/input callbacks through the Canvas path (real comic-panel textures and fonts); New Game -> `VideoPlayer` (Partial: no decoder yet) -> `EndOfVideo` -> the game's own `ClientTravel("Plage00")`; the host starts `--play`. Layout is a first pass (captions misplaced, stray white bars; host calls page callbacks directly because delegates are not interpreted) - item16b.
- **Video (user decision 2026-10-05):** all 19 cutscenes are Bink 1 (`BIKi`); the user chose an **own clean-room Rust decoder** (new crate `xiii-video`, from public format documentation only; never copy or translate FFmpeg code). Plage01's `EndMapVideo` is `cine01`, so Plage01 cannot end until `VideoPlayer` plays (or completes) it and `PlayingVideo.PlayerTick` is dispatched.
- Full verification at `3d97dc8`: 630 tests passed, 0 failed; Plage00 `--trace` byte-identical to the baseline.

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

1. **Toward a completable Plage00/Plage01 with the player in control:** windowed `--play` run of the Plage01 route (the headless route test passes; confirm the windowed path and remove the remaining route teleports by fixing movement/pathing gaps); play the `cine01` video in `--play` through `VideoPlayer` backed by `xiii-video` (today only its duration is honoured); Banque01 run-through after travel.
2. **Save/load:** finish item20b (game-driven `LOAD` start event, menu Load/Continue), inventory/ammo detail, `SoundToLaunch`.
3. **Menus:** item16c (3D `MapMenu` backdrop, menu audio, working options with persistence, translucency).
4. **Combat:** soldiers leaving `faction` stasis on their scripted cues on Plage00/01; damage/death presentation; Bink audio track selection (the 5 tracks are probably language dubs, unverified).
5. **Rendering:** the comic outline look (needs reference captures).
6. **VM fidelity:** `Vm::destroy` marks the actor deleted before `Destroyed` runs (the engine does the reverse; list natives work around it with raw reads); function calls/writes through a just-destroyed actor still return Accessed None.

Do not report playable progress from the viewer, the `--play` prototype or headless traces until a mission can be completed by the game's own logic without host shortcuts.

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
