//! XIII Classic runtime entry point.
//!
//! Today this only hosts the M0 smoke test (`--smoke`, default). Real modes
//! that import from an owned installation will be added as sibling plugins.

mod audio;
mod cli;
mod collision;
mod play;
mod reach;
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

    if opts.collision_test {
        let (Some(dir), Some(map)) = (&opts.game_dir, &opts.map) else {
            eprintln!("error: --collision-test needs --map and --game-dir");
            return AppExit::error();
        };
        return collision::run(map, dir);
    }

    if opts.reach_test {
        let (Some(dir), Some(map)) = (&opts.game_dir, &opts.map) else {
            eprintln!("error: --reach-test needs --map and --game-dir");
            return AppExit::error();
        };
        return reach::run(map, dir);
    }

    // `--play-script` without a screenshot is the headless deterministic mode (no window).
    if opts.mode == cli::Mode::Play && opts.play_script.is_some() && opts.screenshot.is_none() {
        return play::run_headless(&opts);
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
                cli::Mode::Skinned => {
                    "XIII Classic runtime - skinned character viewer (diagnostic)".into()
                }
                cli::Mode::Play => {
                    "XIII Classic runtime - movement prototype (NOT gameplay)".into()
                }
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
        cli::Mode::Skinned => {
            app.add_plugins(viewer::skinned::SkinnedPlugin { options: opts });
        }
        cli::Mode::Play => {
            app.add_plugins(play::PlayPlugin {
                options: opts.clone(),
            });
            app.add_plugins(audio::AudioFxPlugin { options: opts });
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
    let result = xiii_world::PackageCache::open(dir).and_then(|mut c| {
        let map_pkg = c.map(map)?;
        let actors = xiii_decode::model::level::scan_level(&map_pkg.package, &map_pkg.data);
        let scene = xiii_world::import_map(&mut c, map)?;
        print_placement_stats(map, &actors, &scene);
        Ok(scene)
    });
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
                        xiii_world::MaterialSlot::Texture(t) => scene.textures[*t].label.clone(),
                        xiii_world::MaterialSlot::Missing(e) => format!("<missing: {e}>"),
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

/// Per-map placement statistics for the placement task report: how many placed actors have
/// non-default PrePivot / DrawScale / DrawScale3D **from the map properties** (the class-default
/// fallbacks are counted separately as `placement.<field>.class_default` scene counters), and
/// the top placed actors by `|PrePivot|`.
fn print_placement_stats(
    map: &str,
    actors: &xiii_decode::model::level::LevelActors,
    scene: &xiii_world::WorldScene,
) {
    let mut with_pivot = 0usize;
    let mut with_ds = 0usize;
    let mut with_ds3 = 0usize;
    let mut top: Vec<(f32, String, [f32; 3])> = Vec::new();
    let mut with_pivot_prop = 0usize;
    for a in &actors.all_located {
        if let Some(v) = a.pre_pivot {
            with_pivot_prop += 1;
            let m = v.iter().fold(0.0f32, |s, x| s + x.abs());
            if m > 1e-6 {
                top.push((m, a.path.clone(), v));
            }
        }
        // Map-only non-default values (absence of the property is not a non-default value).
        if a.draw_scale.is_some_and(|v| (v - 1.0).abs() > 1e-6) {
            with_ds += 1;
        }
        if a.draw_scale_3d
            .is_some_and(|v| v.iter().any(|x| (x - 1.0).abs() > 1e-6))
        {
            with_ds3 += 1;
        }
    }
    for o in &scene.objects {
        if let Some(p) = o.placement
            && p.pre_pivot.iter().any(|v| v.abs() > 1e-6)
        {
            with_pivot += 1;
        }
    }
    top.sort_by(|a, b| b.0.total_cmp(&a.0));
    println!(
        "[dump] placement stats {map}: map actors with a Location {}; map DrawScale non-default {with_ds}, map DrawScale3D non-default {with_ds3}, map PrePivot property present {with_pivot_prop}; placed objects with effective non-default PrePivot {with_pivot}",
        actors.all_located.len()
    );
    println!(
        "[dump]   top {} by |PrePivot| (map values, all actor classes):",
        top.len().min(20)
    );
    for (m, path, v) in top.iter().take(20) {
        println!("[dump]   |PrePivot| {m:9.2} UU  {v:?}  {path}");
    }
}
