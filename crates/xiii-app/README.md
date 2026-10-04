# xiii-app

Bevy 0.19.1 runtime binary. Today it only contains the **M0 smoke test**
(`src/smoke/`): a native window, a lit 3D primitive scene, a text overlay,
input logging, a generated audio tone and an unattended report mode. It uses
no game data. Real import-from-installation modes will be added beside
`smoke::SmokePlugin` (selected in `main.rs` from `cli::Mode`).

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
