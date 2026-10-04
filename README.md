# XIII Classic — Rust / Bevy reimplementation

**Status: investigation and design, 4 October 2026. No playable port exists here yet.**

The intended result is a modern, standalone Rust application using Bevy that reads an existing, user-owned installation of **XIII (2003)**. Original maps, characters, textures, dialogue and cinematics stay in that installation. The new application supplies the engine and compatibility behavior, with derived caches and new saves stored separately.

This follows the installation-import approach of [iw4L](https://github.com/vladtrc/iw4L). It does not mean MW2's asset readers or scripting language can run XIII.

The initial investigation successfully read and validated the names, imports, exports and export byte ranges in **196 GOG packages and 213 packages in the locally installed Steam copy**. This establishes package-table access, not geometry decoding or gameplay compatibility. The Steam copy contains community patches; it is not a clean retail comparison.

## Start here

1. [Handoff for the next implementation agent](docs/HANDOFF.md)
2. [Investigation and measured findings](docs/INVESTIGATION.md)
3. [Runtime and importer design](docs/DESIGN.md)
4. [Milestones and acceptance criteria](docs/ROADMAP.md)
5. [Development environment and verification](docs/DEVELOPMENT.md)
6. [Sources and provenance](docs/SOURCES.md)

[AGENTS.md](AGENTS.md) gives persistent implementation guidelines. [Evidence](docs/evidence/) contains hashes, table summaries and installation comparisons; it contains no extracted asset payloads. The standard-library [probe](tools/probe_install.py) can reproduce the two inventories.

## Current recommendation

Build a direct package importer and a narrowly scoped UnrealScript compatibility runtime. Implement native engine operations in Rust, present the imported world through Bevy, and expand from an actual campaign segment. First prove world geometry, animation and script execution separately; then combine them into a playable opening sequence. Campaign fidelity is a planning default, not an explicitly selected priority over multiplayer.

The hardest uncertainties are XIII's static-mesh/terrain/collision layouts, animation compression, and the engine services required by compiled gameplay scripts. Bevy supplies modern application and rendering infrastructure; it does not supply those compatibility layers.

## Run the read-only investigation probe

From WSL, in this project directory:

```bash
python3 tools/probe_install.py XIII_Game --label gog --out docs/evidence/gog-inventory.json
python3 tools/probe_install.py '/mnt/p/SteamLibrary/steamapps/common/XIII - Classic' --label steam-local --out docs/evidence/steam-inventory.json
python3 -m unittest discover -s tools -p 'test_*.py'
```

These are real commands for the supplied research tools. Proposed Rust commands in the design and roadmap describe future interfaces and are explicitly marked as unimplemented.

Original game folders are excluded by `.gitignore`. No original executable was launched, no game installation was modified, and no development toolchain was installed during this investigation.
