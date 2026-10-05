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

## Headless doorway collision test (`--collision-test`)

`src/collision.rs` builds a `xiii-collision::CollisionWorld` from the imported map's
collision triangles, resolves the player extent box from the **inherited** class defaults
(`Default.ini` `DefaultGame=` -> GameInfo `DefaultPlayerClassName=` -> `Vm::class_layout`),
identifies the `Porte6` collision source and its bounding box, measures the door opening,
drops the box to the floor and walks it through the doorway twice with `move_slide`:

- **closed** (all collision): expected blocked by the `Porte6` source;
- **open** (`Porte6` sources excluded): expected to pass >= 1 m beyond the door plane.

Each case prints a `PASS`/`FAIL` line with the blocking source path, contact normal and
height, plus timings. The 50 units/m constant is not changed; the measured calibration data
is in `local/reports/item1-collision.md`. The player pawn resolves to `XIII.XIIIPlayerPawn`
(CollisionRadius 34, CollisionHeight 75 — a half height — via `XIII.XIIIPawn` -> `Engine.Pawn`).
An opt-in integration test (`tests/local_collision.rs`, `XIII_GOG_DIR`) asserts both cases.

`src/viewer/load.rs` imports the map without Bevy types, using `xiii-install` for read-only
package resolution and `xiii-decode`. Actor placement uses **effective** values: the map's
tagged property if present, else the inherited class default resolved read-only through
`xiii_script` (`Vm::class_layout`), else the documented `Engine.Actor` default; the source of
every field is counted (`placement.<field>.map|class_default|engine_default`). `PrePivot` is
applied before scale/rotation (`xiii_decode::common::actor_to_bevy_pre_pivot`). It places
static-mesh actors, the level BSP and the terrain heightfield, and bakes the terrain layers
into one texture. Materials are followed through
Shader/FinalBlend/Tex*/SinusModifier/Combiner down to a texture and drawn unlit with a
`StandardMaterial`; unresolved materials are drawn magenta. Every skipped item is a `skip.*`,
`fail.*` or `note.*` counter, shown in the overlay and printed. The crosshair reports the
object path (CPU ray against the render triangles). The fly camera starts at the PlayerStart
(or `--view x,y,z,yaw,pitch`, given in Bevy metres and degrees). `--dump` imports without a
window, adds collision ray probes, per-map placement statistics (top-20 actors by `|PrePivot|`)
and the `placement.*` provenance counters. Known gaps: no skybox (sky-backdrop BSP surfaces
are skipped and counted), translucent/modulated sea materials render dark, and there is no
vertex lighting. This is an importer diagnostic, not a playable mode.

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
