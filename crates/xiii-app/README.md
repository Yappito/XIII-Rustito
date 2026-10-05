# xiii-app

Bevy 0.19.1 runtime binary. Today it only contains the **M0 smoke test**
(`src/smoke/`): a native window, a lit 3D primitive scene, a text overlay,
input logging, a generated audio tone and an unattended report mode. It uses
no game data. Real import-from-installation modes will be added beside
`smoke::SmokePlugin` (selected in `main.rs` from `cli::Mode`).

## Map viewer (M2a diagnostic)

```sh
cargo run --release -p xiii-app -- --map Plage00 --game-dir P:/AI/XIII/XIII_Game
cargo run --release -p xiii-app -- --map Plage01 --game-dir ... --exit-after-secs 6 --screenshot out.png
cargo run --release -p xiii-app -- --map Plage01 --game-dir ... --view -50.1,22.6,-45.8,157.5,0
cargo run --release -p xiii-app -- --map Plage00 --game-dir ... --dump [--find closed]
cargo run --release -p xiii-app -- --map Plage01 --game-dir ... --collision-test
```

### Sky zone (UE2 skybox)

`xiii-world::zones` classifies every BSP polygon and static-mesh actor into a BSP zone: the
tree root is found from the `front`/`back` references, a point is walked to a leaf by its
`dot(n, p) = w` sign, and the leaf is mapped to a zone through `Model::leaf_zones`. The
decoded `leaf` slots are paired **opposite** to `front`/`back` on this data (the positive side
reads `leaf[1]`); an opt-in test cross-checks this against the engine-computed `Region.iLeaf`
of 336 Plage00 actors (331 match swapped, 0 unswapped). A BSP polygon uses its centroid nudged
along the node plane normal; terrain is left zone-less. The importer exposes one `SceneZone`
per zone with the `SkyZoneInfo` actor path/class, its **Bevy-space location**, fog/ambient
properties and per-zone polygon/object counts, plus `WorldScene::sky_zones`.

The viewer draws the sky with a **second camera** (`order 0`) placed at the `SkyZoneInfo`
location that copies the main camera's rotation each frame (`sky_follow`; no parallax — the
decoded `SkyZoneInfo` has no parallax property). The main camera (`order 1`) does not clear
colour and renders the playable zones on top; both cameras clear depth, so the world is not
occluded by the sky. Geometry is split by `RenderLayers`: sky-zone objects on layer 1 (only
the sky camera), everything else on layer 0 (only the main camera). Fake-backdrop BSP polygons
stay undrawn (they are the window into the sky) and remain counted. The overlay lists every
zone (id, SKY/playable, actor, polygons, objects, location) and the sky camera position.
`XIII_VIEWER_NO_SKY=1` disables the sky camera for before/after comparison captures.

## Headless doorway collision test (`--collision-test`)

`src/collision.rs` builds a `xiii-collision::CollisionWorld` from the imported map's
collision triangles, resolves the player extent box from the **inherited** class defaults
(`Default.ini` `DefaultGame=` -> GameInfo `DefaultPlayerClassName=` -> `Vm::class_layout`),
identifies the `Porte6` collision source and its bounding box, measures the door opening,
drops the box to the floor and walks it through the doorway twice with `move_slide`:

- **closed** (all collision): expected blocked by the `Porte6` source;
- **open** (`Porte6` sources excluded): expected to pass >= 1 m beyond the door plane.

Each case prints a `PASS`/`FAIL` line with the blocking source path, contact normal and
height, plus timings. The PlayerStart case uses the UE2-style `xiii_collision::walk_move`
(step-up on non-walkable contacts, floor following; `MINFLOORZ` 0.7 and upstream
`MAXSTEPHEIGHT` 35 UU converted with the coordinate policy); the aligned door case and a
labelled `move_slide` diagnostic line are kept for comparison. The 50 units/m constant is not
changed; the measured calibration data is in `local/reports/item1-collision.md`. The player
pawn resolves to `XIII.XIIIPlayerPawn` (CollisionRadius 34, CollisionHeight 75 — a half height
— via `XIII.XIIIPawn` -> `Engine.Pawn`). An opt-in integration test
(`tests/local_collision.rs`, `XIII_GOG_DIR`) asserts the aligned door case.

The map import itself lives in the Bevy-free `xiii-world` crate (moved out of
`src/viewer/load.rs`); `xiii-app` keeps only the Bevy conversion. It imports the map using
`xiii-install` for read-only package resolution and `xiii-decode`. Actor placement uses
**effective** values: the map's
tagged property if present, else the inherited class default resolved read-only through
`xiii_script` (`Vm::class_layout`), else the documented `Engine.Actor` default; the source of
every field is counted (`placement.<field>.map|class_default|engine_default`). `PrePivot` is
applied before scale/rotation (`xiii_decode::common::actor_to_bevy_pre_pivot`). It places
static-mesh actors, the level BSP and the terrain heightfield, and bakes the terrain layers
into one texture. Materials are followed through
Shader/FinalBlend/Tex*/SinusModifier/Combiner down to a texture and drawn unlit with a
`StandardMaterial`; unresolved materials are drawn magenta. Placed static meshes and terrain
are additionally modulated by the decoded baked vertex colours (`--lighting baked`, the
default; `--lighting off` ignores them for comparison), carried by a private mesh per coloured
object because the lighting is per placed actor. The stored BGRA colours are swapped to RGBA
and the 4th byte is forced opaque; an all-zero instance is treated as unlit rather than black.
The BSP still has no vertex lighting or lightmap (the Model lightmap tail is not decoded).
Every skipped item is a `skip.*`,
`fail.*` or `note.*` counter, shown in the overlay and printed. The crosshair reports the
object path (CPU ray against the render triangles). The fly camera starts at the PlayerStart
(or `--view x,y,z,yaw,pitch`, given in Bevy metres and degrees). `--dump` imports without a
window, adds collision ray probes, per-map placement statistics (top-20 actors by `|PrePivot|`)
and the `placement.*` provenance counters. The sky zone is rendered by a second camera (see
"Sky zone" above). Known gaps: translucent/modulated sea materials render dark, the BSP has no
baked lighting, and the sky uses the same unlit diagnostic materials as the rest of the
import. This is an importer diagnostic, not a playable mode.

## Skinned-character viewer (M2b diagnostic)

```sh
cargo run --release -p xiii-app -- --model xiiipersos.XIIIM --anim Walk \
    --game-dir P:/AI/XIII/XIII_Game --exit-after-secs 4 --screenshot out.png
cargo run --release -p xiii-app -- --model xiiipersos.XIIIM --anim WaitNeutre \
    --game-dir ... --frame 0 --screenshot idle0.png
cargo run --release -p xiii-app -- --model xiiipersos.XIIIM,xiiipersos.SlaterM \
    --anim Walk --game-dir ...
```

`--model PKG.MESH[,PKG.MESH...]` decodes one or more `Engine.SkeletalMesh` exports and their
`Engine.MeshAnimation` (the mesh's own reference, resolved through the installation). The
mesh is uploaded as a GPU-skinned Bevy `Mesh` (positions/normals/UVs, `JOINT_INDEX`
`Uint16x4`, `JOINT_WEIGHT` `Float32x4`, one `SkinnedMesh` per section with inverse-bind
matrices from the decoded bind pose) and one entity per bone. `--anim SEQ` plays a decoded
sequence (`xiii-tool anim list` prints the real names); without it the bind pose is shown.
`--frame N` freezes the clip at frame N and the camera front-on for comparison captures.

Joints are driven by **the shared CPU sampler** (`xiii_decode::skeletal::normalize::evaluate_pose`,
the one `xiii-tool anim render/validate` uses), not Bevy's animation graph. The Unreal -> Bevy
coordinate policy is applied in exactly one function, `skinned::source_to_bevy_transform`,
built only from `xiii_decode::common` (`to_bevy_position`, `to_bevy_direction`,
`SOURCE_TO_BEVY`); because the change of basis is linear, converting every local bone
transform is the same as applying it once at the skeleton root. The decoded mesh `RotOrigin`
(XIII characters are authored +Y-forward; XIIIM/SlaterM store yaw 49152 = 270 deg) is applied
as the character root rotation so the character faces the policy forward (source +X -> Bevy
-Z). `MeshScale`/`MeshOrigin` are not baked (not needed for the bind pose; `MeshOrigin.z` is
~80 UU and applying it would lift the feet). The mesh gets `NoFrustumCulling` (skinned AABBs
are not recomputed here).

Turntable camera, 1 m grid, forward/right/up axis lines and a clip/frame/time overlay;
`--exit-after-secs`/`--screenshot` behave as in the other modes. Materials are decoded from
the mesh's material references through the existing texture decoder and drawn unlit (magenta
= unresolved). Skipped/dropped influences are printed (`influence_stats`), never hidden. This
is an importer diagnostic, not a playable mode.

Opt-in tests (`XIII_GOG_DIR`) compare the CPU-skinned positions (what `anim render` draws)
with a CPU mirror of the GPU skinning chain (`joint_global * inverse_bindpose`, LBS) for
`XIIIM` `Walk`/`WaitNeutre` at frames 0, n/4, n/2 (max error ~1e-4 UU, asserted < 0.01), and
check idle frame 0 (feet 0.1 UU, height 160.9 UU, facing). A unit test builds a synthetic
2-bone rig to exercise the same path without game data.

## Run

```sh
cargo run -p xiii-app                       # interactive (same as --smoke)
cargo run -p xiii-app -- --frames 300 --screenshot shot.png
cargo run -p xiii-app -- --exit-after-secs 5 --no-vsync
WGPU_BACKEND=dx12 cargo run -p xiii-app -- --frames 200   # force a backend
```

Controls: WASD/QE move, Shift fast, hold right mouse to look (cursor locked),
Space plays the tone, Esc quits. With `--frames`/`--exit-after-secs` the app
requests the tone on frame 15, takes the optional screenshot at 75 % progress,
waits up to 3 s for those to complete, prints a `[smoke]` report (adapter,
backend, driver, window logical/physical size, present mode, frame-time
avg/min/max over all frames and after 30 warm-up frames, audio started yes/no)
and exits with code 0.

Axis marker at the origin (unlit): **red = +X, green = +Y, blue = -Z** (Bevy's
camera forward), white cube = origin. Use it to check the Unreal -> Bevy
conversion once a known-orientation asset is imported.

## Bevy feature set

`default-features = false` in the root `Cargo.toml`; names verified against
the bevy 0.19.1 crate manifest (and bevy_internal's feature graph).

| Feature | Why |
| --- | --- |
| `std`, `multi_threaded`, `async_executor` | Normal desktop task pools/executor. |
| `bevy_log` | `info!`/`warn!` and the log subscriber. |
| `bevy_asset` | `Assets<T>`, `AssetServer`; needed by meshes, images, audio. |
| `bevy_winit`, `bevy_window` | Native window and event loop. |
| `keyboard`, `mouse` | In 0.19 these `bevy_input` device features are opt-in; without them there's no `ButtonInput<KeyCode>`/mouse support. Gamepad (`bevy_gilrs`) left out until needed. |
| `bevy_render`, `bevy_core_pipeline`, `bevy_pbr` | 3D PBR (`Camera3d`, `StandardMaterial`, lights, shadows). `bevy_render` builds wgpu with DX12 + Vulkan (+Metal) backends. Pulls in `bevy_camera`, `bevy_light`, `bevy_mesh`, `bevy_material`, `bevy_shader`, `bevy_image`. |
| `bevy_gizmos_render` | Debug line drawing (reference grid); useful for later diagnostics. |
| `tonemapping_luts`, `ktx2`, `zstd_rust` | The default `Camera3d` tonemapper (TonyMcMapface) samples a KTX2/zstd LUT; without it Bevy uses a placeholder and logs errors. `zstd_rust` picks the pure-Rust zstd backend (`bevy_image` refuses to compile `zstd` without a backend). |
| `png` | Screenshot `save_to_disk`. |
| `bevy_ui`, `bevy_ui_render`, `default_font` | UI `Text` overlay with the embedded FiraMono subset (no font file). Pulls in `bevy_text`, `bevy_sprite(_render)`. |
| `bevy_audio`, `wav` | rodio/cpal output and WAV decoding for the in-memory tone. Vorbis/MP3 etc. left out until the game's audio formats are decided. |

Deliberately omitted from bevy's `default`: `bevy_gltf`, `bevy_scene`/BSN,
`bevy_animation`, picking, `bevy_post_process`, `bevy_anti_alias`,
`smaa_luts`, `dfg_lut`, `bevy_state`, `reflect_auto_register`, `sysinfo_plugin`,
`x11`/`wayland`/`webgl2`, `custom_cursor`, clipboard, `bevy_gilrs`, `hdr`.
Add them when a concrete need appears (Linux builds need `x11` and/or
`wayland`).

## Bevy 0.19 API notes (verified against 0.19.1 source/examples)

- Buffered events are **messages**: `#[derive(Message)]`, `app.add_message`,
  `MessageReader`/`MessageWriter::write`; `AppExit` is a message.
- `PointLight`/`DirectionalLight` use `shadow_maps_enabled` (not `shadows_enabled`).
- Ambient light: the resource is `GlobalAmbientLight`; `AmbientLight` is a
  per-camera component.
- Text size: `TextFont { font_size: FontSize::Px(..) }`; UI lengths via `px(..)`.
- Cursor grab/visibility live in the `CursorOptions` component on the window entity.
- Adapter info: `bevy::render::renderer::RenderAdapterInfo` (deref to
  `wgpu::AdapterInfo`), inserted into the main world after renderer init.
- Screenshots: `commands.spawn(Screenshot::primary_window()).observe(save_to_disk(path))`;
  observers take `On<ScreenshotCaptured>`.
- Mouse look: `AccumulatedMouseMotion` resource.
- Examples now use the `bsn_list!` scene macro; this crate uses plain
  `commands.spawn` and does not enable `bevy_scene`.
