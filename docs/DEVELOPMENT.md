# Development and verification

## Host observed on 2026-10-04

| Item | Observed state |
|---|---|
| Project | Windows `P:\AI\XIII`; WSL `/mnt/p/AI/XIII` |
| Baseline install | `XIII_Game` inside the project; treat as read-only |
| Alternate install | `P:\SteamLibrary\steamapps\common\XIII - Classic`; contains community patches |
| WSL | WSL2 x86-64, kernel `6.6.114.1-microsoft-standard-WSL2` |
| GPU reported by Windows | NVIDIA GeForce RTX 4090, driver `32.0.16.1714` |
| WSL tools found | Python 3, Git, objdump, strings, Windows PowerShell interop |
| Rust/Cargo | Not found on either inspected PATH; not installed during this investigation |
| Windows C++ build tools | Standard `vswhere.exe` locator absent; build ability not established |
| Original game / Bevy runtime | Neither launched in this investigation |

Do not infer native build support from WSL tools or GPU availability from a successful headless parser run.

## Recommended build arrangement

Build and playtest the shipping application natively on Windows with the `x86_64-pc-windows-msvc` target. Use WSL for read-only investigation, parser development and Linux/headless tests. Both can use the same source folder, but use **different target/output directories** and do not run conflicting dependency updates or formatters concurrently.

Rust/Cargo installation and a native C++ toolchain are next-stage setup work. Follow [Bevy's Windows setup](https://bevy.org/learn/quick-start/getting-started/setup/) for MSVC, a Windows SDK and the relevant C++ tools. Bevy 0.19.1's tagged Cargo manifest requires Rust 1.95.0 or newer. Pin a tested toolchain in `rust-toolchain.toml`, use an exact initial Bevy version (`=0.19.1`), and commit `Cargo.lock` once a workspace exists.

Keep the initial Bevy feature selection small but sufficient for native windowing, input, assets, 3D rendering, UI/text and audio. Derive exact feature names from the **0.19.1** manifest/examples. Do not paste older Bevy bundle/event/material APIs from memory. The optional Avian physics candidate is 0.7; do not depend on its development `main` branch.

WSL filesystem access to `/mnt/p` is adequate for this investigation but can add build overhead. Prefer a Linux-native target directory for WSL compilation and a Windows-native target directory for Windows builds. Do not move or duplicate the user's 2.5 GB source installation just to speed Rust builds. Cross-compiling from Linux can be investigated later; it is not the shortest first Windows build path.

## Commands that work now

Run from the project root in WSL:

```bash
python3 tools/probe_install.py XIII_Game --label gog --out docs/evidence/gog-inventory.json
python3 tools/probe_install.py '/mnt/p/SteamLibrary/steamapps/common/XIII - Classic' --label steam-local --out docs/evidence/steam-inventory.json
python3 tools/build_evidence.py --gog-root XIII_Game
python3 -m unittest discover -s tools -p 'test_*.py'
```

The inventory probe reads assets/binaries/configuration and writes only the specified metadata report. It excludes save/profile/cache/log directories, hashes files in chunks, validates package tables for version 100, and returns a nonzero exit code if recognized packages fail. `--exports` adds object metadata, not payloads; keep verbose local reports outside source control if they are not needed for review. An output path inside the source installation is rejected.

`build_evidence.py` uses the two inventory JSON files, rereads selected baseline packages, and optionally uses `objdump` for DLL export summaries. Run it after the inventories. The focus-package hashes must match the GOG inventory or the script stops. Research scripts require Python 3.10 or newer and otherwise use the standard library.

The public baseline evidence is metadata only. Hashes refer to the exact local files observed, not all copies sold by GOG or Steam. The current install-comparison normalization is analytical, not a runtime load-order specification.

## Commands for the future Rust workspace

**These cannot run yet: no Cargo workspace or Rust runtime has been created.** Once implemented, use the ordinary workspace checks:

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

Add explicit installation-dependent integration tests, excluded/skipped with a clear explanation when no installation is supplied. Do not make ordinary public CI download game data or quietly pass integration tests that did not run. Record native Windows build and smoke-test results independently from WSL results.

## Original-engine investigation

The included UCC/editor and native DLL symbols are useful references. Before invoking an old tool, identify its input/output behavior and run it in a disposable working layout with writable configuration/logs isolated from both supplied installations. Some tools write to their working directory even for apparent reads. Record the exact binary hash, command, exit status and output provenance. Do not assume another game's UE2 editor understands XIII.

Start with tool help/listing and a single export experiment; exact supported commandlet syntax has **not** been verified. UCC class/source export cannot recover source that was stripped. An exported T3D scene may omit cooked collision, lightmaps, animation or game-specific fields. Treat it as a cross-check until proven complete.

For binary analysis, prioritize named serializers and native functions implicated by a concrete failing object or script. Record addresses relative to a hashed binary, not as universal offsets. A 32-bit research helper may aid comparisons; keep it out of the x64 shipping runtime's required path.

## Test strategy

| Layer | Required evidence |
|---|---|
| Reader | Compact-index boundaries, string encodings, truncated tables, invalid references, bad sizes, cycles and resource limits |
| Corpus | Counts/hash identities, complete object consumption, dependency resolution and explicit unsupported cases |
| Geometry | Axis/winding/scale probes, material assignments, bounds, terrain placement, doorways/volumes/collision sweeps |
| Animation | Bind pose, multiple clip timestamps, skinning weights, root/notify behavior |
| Script | Real opcode/native coverage, inheritance/defaults, events, latent actions and error traces |
| Simulation | Movement/weapon/timer behavior across render rates, interactions and repeatable checkpoints |
| Presentation | Original screenshots and timing traces under recorded settings; meaningful tolerances rather than unexplained pixel equality |
| Campaign | Authored progression and failure/restart paths, not just successful loading |
| Packaging | Fresh Windows run, source-install write protection, isolated settings/cache/saves |

Use generated fixtures in the repository. Keep source assets, extracted/decompiled content, original screenshots, gameplay recordings and save files under ignored `local/` or outside the project. A parser bug deserves a minimal synthetic regression where possible; do not copy an entire proprietary package into a test fixture.

## Ongoing documentation

Update `HANDOFF.md` with what actually works, tests run, exact next task and current blockers. Keep `INVESTIGATION.md`'s measured findings separate from `DESIGN.md` proposals. For major deviations, add a short dated decision explaining evidence and tradeoffs. Preserve source links/revisions and license notices when adapting third-party code. Do not claim a clean-room process: this investigation consulted open-source implementations and metadata.
