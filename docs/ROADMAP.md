# Milestones and acceptance criteria

These are implementation tasks, not completed features. Order them by dependency and by how quickly they resolve feasibility. Do not defer the animation and scripting spikes until after a polished map viewer.

## M0 — Establish a reproducible baseline

**Current state:** investigation completed; binary table probe and evidence available. Native Rust/Bevy environment and original-game reference captures remain outstanding.

- Confirm the Rust toolchain and native Windows C++ build prerequisites. Pin Rust at or above Bevy's declared MSRV, Bevy 0.19.1, and the resulting lockfile.
- Create the initial three-crate workspace described in the design.
- Run a native Windows Bevy window/input/audio/GPU smoke test; record adapter/backend, resolution and build configuration. Do not use a WSL window as the only Windows test.
- Establish an original-game reference using a disposable copy or redirected writable state. Record the installation hash/profile and settings. The source GOG directory remains the import baseline.
- Confirm opening-map order and identify a short repeatable original-game route.

**Exit:** another developer can build the skeleton, run the probe and identify the exact content being compared. No original installation changes. No claims of game compatibility yet.

## M1 — Real Rust package inspection

- Port the research probe's narrow verified behavior into a bounded Rust reader; inspect independent UModel/UELib implementations where useful.
- Read the full package summary, GUID/generations, names, imports, exports and tagged properties for the measured revisions.
- Implement logical object IDs, platform-aware path resolution, inheritance/defaults and dependency reporting.
- Produce structured metadata and unsupported-layout reports.

**Exit:** the Rust tool matches the measured name/import/export counts for all 196 GOG packages; handles the additional Steam corpus without using a flat-folder assumption; rejects malformed synthetic inputs without panics or excessive allocation. Prove actual object references/defaults on selected maps. Do not count the existing Python table probe as this milestone.

Proposed future CLI, **not implemented**:

```text
xiii-tool inspect --game-dir <installation> --package Maps/Plage00.unr
xiii-tool dependencies --game-dir <installation> --map Plage00 --report <path>
```

## M2 — Resolve the three critical feasibility risks

### M2a: world and collision

- Decode one texture including its palette/mips, one `StaticPlage2` static mesh, one BSP model and one terrain sector with actor transforms.
- Import Plage00/Plage01 geometry incrementally. Use a diagnostic material initially.
- Decode collision flags, hulls/volumes and terrain holes rather than assuming render triangles fully represent collision.
- Check an asymmetric object/camera to settle axes, scale, winding and rotations.

**Exit:** a native Windows viewer displays recognizable, correctly placed original geometry, including terrain; source-object IDs can be inspected; a collision sweep hits the expected wall/ground and passes an intended doorway. Every included serializer has accounted for its object payload or explicitly documented unsupported tail.

### M2b: skeleton and animation

- Decode one character mesh, its skeleton and a real `MeshAnimation` sequence from the supplied `.u` assets.
- Establish bind transforms, hierarchy, weights, clip timing, compressed tracks, root movement and relevant animation events.

**Exit:** the character has a correct bind pose and plays at least an idle and a locomotion/action clip with stable skinning. Check several frames against the original. If this fails, retain the failure evidence and investigate before planning a complete FPS presentation around static meshes.

### M2c: script behavior and native calls

- Recover the compiled function/state payloads for selected PC `XIDMaps`, `XIII` and `XIDCine` classes.
- Enumerate opcode/native dependencies and cross-check selected disassembly with an independent tool where it actually supports the build.
- Implement a minimal interpreter/IR and native registry.
- Exercise a real authored trigger/state transition, timer or latent wait, actor lookup and one native scene operation in a controlled headless harness.

**Exit:** a measured behavior trace explains why that real script path succeeds, including native operations and object-property access. Unknown opcodes/natives fail explicitly. Produce a go/no-go decision on retaining compiled campaign logic. If a manual Rust campaign rewrite is chosen instead, record why and revise the campaign workload.

M2a–c can become parallel work only if the user or later project instructions authorize multiple agents; there is no standing delegation requirement.

## M3 — One playable campaign segment

Select the segment after reference play, provisionally Plage00 → Plage01. Avoid beginning with an unrelated multiplayer sandbox as the only milestone for campaign viability.

- Player spawn, movement, view, collision, inventory and one weapon.
- An enemy with working authored behavior, damage/death and required animation.
- Door/key/trigger interaction and the mission's required goals.
- One dialogue path and weapon effects mapped through real HX bank identifiers.
- Relevant comic-window/camera behavior and subtitles.
- A supported checkpoint, death/reload, and map transition.
- Read-only game import and cold/warm cache paths.

**Exit:** complete the selected segment through ordinary play, restart from its checkpoint, and transition correctly without required native stubs or manual console repairs. A developer fly camera remains a diagnostic mode and is not acceptance evidence.

Record frametime and memory on this host at 1080p. A provisional 60 FPS target is useful for regression, but the RTX 4090 is not a minimum-spec certification. Separate simulation timing from render rate and verify behavior at 30, 60 and 144 FPS.

## M4 — Campaign coverage

Maintain a per-map capability matrix derived from actual imports, classes, native calls and playthrough results. Add mechanics in order of campaign dependency: remaining weapons, thrown/improvised objects, stealth/hostages where present, ladders/water, special movers/vehicles, bosses, cinematics and other mission-specific behavior. This list is a scope guide; derive exact requirements from content and reference play.

- Validate each map's objectives, spawn/despawn logic, failure conditions and transitions.
- Cover localization, UI presentation, checkpoint restoration, dialogue and music transitions.
- Regress earlier maps as shared engine semantics improve.

**Exit:** a recorded end-to-end campaign playthrough with no blocking progression defects, plus checkpoint resume tests across representative mechanics. A list of loaded maps does not satisfy this criterion.

## M5 — Modern Windows release candidate

- Installation chooser/discovery, actionable errors and documented supported content profiles.
- Rebinding, raw mouse input, borderless/fullscreen, resolution/FOV settings, scalable UI/subtitles, independent audio controls.
- Measured loading, memory and frametimes on more than the development GPU.
- Recoverable cache failures, interrupted imports and saves; writes in the correct user directories.
- Distribution containing only project code and authorized dependencies, with attribution/license notices.
- A fresh native Windows-machine test without a development toolchain or original engine running.

**Exit:** a user can point the release at a supported owned installation and play the supported campaign scope. Describe remaining limits plainly.

## Deferred decisions

| Decision | When to settle it |
|---|---|
| Production physics/query backend and exact movement shape | M2a / early M3, from observed movement |
| Direct bytecode interpreter vs normalized internal instructions | M2c |
| Runtime FFI vs import-time tool vs Rust HX decoder | Audio spike before M3 |
| Bink playback decoder and packaging | Before the first required story video |
| Exact outline/material technique | After original-scene captures and decoded materials |
| Original-save import | After the new checkpoint format works |
| Multiplayer and protocol strategy | After single-player priorities are confirmed and the campaign core is stable |

Do not choose calendar promises before M2. Record time spent and unresolved layouts to make later estimates evidence-based.
