//! XIII Classic runtime entry point.
//!
//! Today this only hosts the M0 smoke test (`--smoke`, default). Real modes
//! that import from an owned installation will be added as sibling plugins.

mod cli;
mod smoke;
mod viewer;

use bevy::prelude::*;
use bevy::window::PresentMode;

fn main() -> AppExit {
    let opts = match cli::parse(std::env::args().skip(1)) {
        Ok(o) => o,
        Err(msg) => {
            if msg.is_empty() {
                println!("{}", cli::USAGE);
                return AppExit::Success;
            }
            eprintln!("error: {msg}\n\n{}", cli::USAGE);
            return AppExit::error();
        }
    };

    if opts.mode == cli::Mode::Viewer && opts.dump {
        return dump(&opts);
    }

    if opts.unattended() {
        println!(
            "[app] unattended run: frames={:?} exit_after_secs={:?} screenshot={:?} size={}x{} no_vsync={}",
            opts.frames,
            opts.exit_after_secs,
            opts.screenshot,
            opts.width,
            opts.height,
            opts.no_vsync
        );
    }

    let present_mode = if opts.no_vsync {
        PresentMode::AutoNoVsync
    } else {
        PresentMode::AutoVsync
    };

    let mut app = App::new();
    app.add_plugins(DefaultPlugins.set(WindowPlugin {
        primary_window: Some(Window {
            title: match opts.mode {
                cli::Mode::Smoke => "XIII Classic runtime - smoke test".into(),
                cli::Mode::Viewer => "XIII Classic runtime - map viewer (diagnostic)".into(),
            },
            resolution: (opts.width, opts.height).into(),
            present_mode,
            ..default()
        }),
        ..default()
    }));

    match opts.mode {
        cli::Mode::Smoke => {
            app.add_plugins(smoke::SmokePlugin { options: opts });
        }
        cli::Mode::Viewer => {
            app.add_plugins(viewer::ViewerPlugin { options: opts });
        }
    }

    app.run()
}

/// `--map ... --dump`: import without a window and print the counters.
fn dump(opts: &cli::Options) -> AppExit {
    let (Some(dir), Some(map)) = (&opts.game_dir, &opts.map) else {
        eprintln!("error: --dump needs --map and --game-dir");
        return AppExit::error();
    };
    let started = std::time::Instant::now();
    let result = viewer::load::PackageCache::open(dir)
        .and_then(|mut c| viewer::load::import_map(&mut c, map));
    match result {
        Ok(scene) => {
            let tris: usize = scene
                .objects
                .iter()
                .map(|o| scene.meshes[o.mesh].indices.len() / 3)
                .sum();
            println!(
                "[dump] {map}: objects {} meshes {} textures {} triangles {} in {:.2}s",
                scene.objects.len(),
                scene.meshes.len(),
                scene.textures.len(),
                tris,
                started.elapsed().as_secs_f32()
            );
            for (k, v) in &scene.counters {
                println!("[dump] {v:>6} {k}");
            }
            for (k, e) in &scene.examples {
                println!("[dump] example {k}: {e}");
            }
            for t in &scene.textures {
                println!(
                    "[dump] texture {} {}x{} {:?}",
                    t.label, t.image.width, t.image.height, t.alpha
                );
            }
            if let Some(f) = &opts.find {
                for o in &scene.objects {
                    let m = &scene.meshes[o.mesh];
                    let tex = match &m.material {
                        viewer::load::MaterialSlot::Texture(t) => scene.textures[*t].label.clone(),
                        viewer::load::MaterialSlot::Missing(e) => format!("<missing: {e}>"),
                    };
                    if o.path.to_ascii_lowercase().contains(f)
                        || tex.to_ascii_lowercase().contains(f)
                    {
                        let (mut lo, mut hi) = ([f32::MAX; 3], [f32::MIN; 3]);
                        for p in &m.positions {
                            for k in 0..3 {
                                lo[k] = lo[k].min(p[k]);
                                hi[k] = hi[k].max(p[k]);
                            }
                        }
                        println!(
                            "[dump] find {} | {tex} | translation {:?} scale {:?} rotation {:?} local bounds {lo:?}..{hi:?}",
                            o.path,
                            o.transform.translation,
                            o.transform.scale,
                            o.transform.rotation
                        );
                    }
                }
            }
            println!("[dump] player start {:?}", scene.player_start);
            if let Some((p, rot)) = scene.player_start {
                // Ray probes against the decoded collision (not a capsule sweep).
                let yaw = rot[1] as f32 * std::f32::consts::TAU / 65536.0;
                let fwd = [yaw.sin(), 0.0, -yaw.cos()];
                let right = [yaw.cos(), 0.0, yaw.sin()];
                let probes = [
                    ("down", [0.0, -1.0, 0.0]),
                    ("up", [0.0, 1.0, 0.0]),
                    ("forward", fwd),
                    ("back", fwd.map(|v| -v)),
                    ("right", right),
                    ("left", right.map(|v| -v)),
                ];
                for (name, d) in probes {
                    match scene.ray_collision(p, d) {
                        Some((dist, src)) => {
                            println!("[dump] collision ray {name}: {dist:.2} m -> {src}")
                        }
                        None => println!("[dump] collision ray {name}: no hit"),
                    }
                }
            }
            AppExit::Success
        }
        Err(e) => {
            eprintln!("error: {e}");
            AppExit::error()
        }
    }
}
