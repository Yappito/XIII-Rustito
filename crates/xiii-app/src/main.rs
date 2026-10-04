//! XIII Classic runtime entry point.
//!
//! Today this only hosts the M0 smoke test (`--smoke`, default). Real modes
//! that import from an owned installation will be added as sibling plugins.

mod cli;
mod smoke;

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

    if opts.unattended() {
        println!(
            "[smoke] unattended run: frames={:?} exit_after_secs={:?} screenshot={:?} size={}x{} no_vsync={}",
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
            title: "XIII Classic runtime - smoke test".into(),
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
    }

    app.run()
}
