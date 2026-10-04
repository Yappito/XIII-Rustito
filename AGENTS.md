# XIII Classic implementation guidelines

## Purpose and current scope

Build an independent Rust/Bevy runtime for the user's owned XIII Classic (2003) data, following the import-from-installation approach of iw4L. The initial task was investigation and handoff; do not confuse completed documentation with a completed port. Read `docs/HANDOFF.md`, `docs/INVESTIGATION.md`, `docs/DESIGN.md` and `docs/ROADMAP.md` before implementation.

The user explicitly prefers loading assets from an existing installation. Native Windows is the shipping/playtest target; WSL2 is available. Campaign-first sequencing is a working assumption, not an explicit rejection of multiplayer. The user intends a subsequent implementation agent/model to continue from these documents.

## Data and evidence

- Treat `XIII_Game/` and the discovered Steam installation as read-only source data. Put all caches, new saves, extracted assets, decompiler output and original-game captures elsewhere. Use a disposable working copy for tools that write into the game directory.
- Do not commit or redistribute original assets, executables, DLLs, source exports or saves. `.gitignore` contains baseline protections; review new files before committing.
- Use the GOG corpus as the initial profile. The local Steam corpus contains community patch files. Support its layout without assuming every patch-specific script is compatible.
- Distinguish measured results, upstream claims, hypotheses and planned features. Never report table parsing as mesh decoding, a map viewer as a playable mission, or fixed timestep as proven cross-platform determinism.
- Preserve package hashes, object paths, byte offsets and tool revisions in research notes. Do not silently ignore required data, opcode or native-operation failures.

## Implementation

- Keep disk discovery, bounded format parsing, normalized asset products, simulation and presentation separate. Start with a small workspace and split crates only when useful.
- Implement direct import. External exporters are research/validation aids and may be temporary local decoder adapters, not a mandatory manual player workflow.
- Pin and verify dependencies. The research baseline is Bevy 0.19.1, whose tagged manifest requires Rust 1.95.0 or newer. Use that release's APIs and examples; recheck before intentionally upgrading.
- Resolve package names case-insensitively while keeping exact paths, explicit platform/profile precedence and duplicate diagnostics. Never mix two installations implicitly.
- Maintain one authoritative owner for simulation fields; bridge script values/native components deliberately. Keep time and lifecycle ordering explicit.
- Required campaign behavior must not use silent success stubs. Diagnostic placeholders must be visible and excluded from playable acceptance claims.
- Test generated fixtures without proprietary data, then run opt-in local-corpus integration tests. Validate the native Windows target independently of WSL.
- Add regression tests for real parser/simulation risks, not tests that merely restate implementation details. Record what did and did not run.

## Handoff and scope discipline

Maintain `docs/HANDOFF.md` after each substantial implementation milestone. Update design decisions when evidence changes. Do not add an editor, multiplayer infrastructure, automatic updater, source compiler or generic UE2 engine before resolving the scoped importer/animation/script risks. No multi-agent delegation is required by this file.

No full gameplay source was found in the retail gameplay text buffers. The archived public script repository is probably Xbox-oriented and lacks a root license file; it is not a verified freely licensed PC implementation. Preserve provenance and attribution for any actual third-party code reuse; importing user-owned data is the product strategy.
