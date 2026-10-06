//! `--video`: play a Bink 1 cutscene decoded by the clean-room `xiii-video` decoder.
//!
//! The decoded RGBA frame is uploaded to a Bevy image each tick at the file's frame rate. If the
//! decoder cannot decode a frame it shows black, records the error and keeps running for the
//! unattended duration so a screenshot is still produced; the error is reported on exit.

use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};

use xiii_video::container::BikFile;
use xiii_video::{BinkTables, Decoder, YuvFrame};

use crate::cli::Options;

/// Resource holding the parsed options.
#[derive(Resource, Clone)]
pub struct VideoConfig {
    /// Runtime options (file, game dir, exit/screenshot).
    pub options: Options,
}

/// Runtime playback state.
#[derive(Resource)]
pub struct VideoState {
    data: Vec<u8>,
    bik: BikFile,
    decoder: Decoder,
    prev: Option<YuvFrame>,
    next_frame: usize,
    handle: Handle<Image>,
    start: Instant,
    last_advance: Instant,
    shot: u8,
    shot_done: bool,
    decoded: usize,
    errors: usize,
    first_error: Option<String>,
    target_at: Option<Instant>,
}

#[derive(Resource, Default)]
struct ShotFlag(bool);

/// Playback plugin.
pub struct VideoPlugin {
    /// Parsed command-line options.
    pub options: Options,
}

impl Plugin for VideoPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(VideoConfig {
            options: self.options.clone(),
        })
        .init_resource::<ShotFlag>()
        .add_systems(Startup, setup)
        .add_systems(Update, (advance, unattended));
    }
}

fn image_from_rgba(rgba: &[u8], w: usize, h: usize) -> Image {
    Image::new(
        Extent3d {
            width: w as u32,
            height: h as u32,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        rgba.to_vec(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    )
}

fn setup(mut commands: Commands, cfg: Res<VideoConfig>, mut images: ResMut<Assets<Image>>) {
    let Some(file) = cfg.options.video.clone() else {
        eprintln!("[video] no --video file");
        commands.write_message(AppExit::error());
        return;
    };
    let Some(game_dir) = cfg
        .options
        .game_dir
        .clone()
        .or_else(|| install_root_of(&file))
    else {
        eprintln!(
            "[video] --video needs --game-dir (or a file inside the installation's Video folder)"
        );
        commands.write_message(AppExit::error());
        return;
    };
    let data = match std::fs::read(&file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("[video] cannot read {}: {e}", file.display());
            commands.write_message(AppExit::error());
            return;
        }
    };
    let bik = match BikFile::parse(&data) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("[video] {}: {e}", file.display());
            commands.write_message(AppExit::error());
            return;
        }
    };
    let tables = match BinkTables::from_install(&game_dir) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[video] {e}");
            commands.write_message(AppExit::error());
            return;
        }
    };
    let width = bik.header.width as usize;
    let height = bik.header.height as usize;
    let black = vec![0u8; width * height * 4];
    let handle = images.add(image_from_rgba(&black, width, height));

    commands.spawn(Camera2d);
    commands.spawn((
        Node {
            width: Val::Percent(100.0),
            height: Val::Percent(100.0),
            ..default()
        },
        ImageNode {
            image: handle.clone(),
            ..default()
        },
    ));

    let decoder = Decoder::new(tables);
    println!(
        "[video] {} {}x{} {} frames {:.3} fps (decoder clean-room Bink 1)",
        file.display(),
        width,
        height,
        bik.frame_count(),
        bik.header.fps()
    );

    let now = Instant::now();
    commands.insert_resource(VideoState {
        data,
        bik,
        decoder,
        prev: None,
        next_frame: 0,
        handle,
        start: now,
        last_advance: now,
        shot: 0,
        shot_done: false,
        decoded: 0,
        errors: 0,
        first_error: None,
        target_at: None,
    });
}

/// The installation root for a cutscene stored as `<root>/Video/<file>.bik`.
fn install_root_of(file: &std::path::Path) -> Option<std::path::PathBuf> {
    let video_dir = file.parent()?;
    let name = video_dir
        .file_name()?
        .to_string_lossy()
        .to_ascii_lowercase();
    if name != "video" {
        return None;
    }
    video_dir.parent().map(std::path::Path::to_path_buf)
}

fn advance(
    cfg: Res<VideoConfig>,
    state: Option<ResMut<VideoState>>,
    mut images: ResMut<Assets<Image>>,
) {
    // Setup failed (and requested exit): nothing to play.
    let Some(mut state) = state else {
        return;
    };
    // Frames are due by wall-clock time since playback start, so playback keeps the file's
    // frame rate even when the window updates less often (e.g. unfocused); every due frame is
    // decoded (inter frames depend on their predecessor), only the newest one is uploaded.
    let fps = state.bik.header.fps().max(1.0);
    let due =
        ((state.start.elapsed().as_secs_f64() * fps) as usize + 1).min(state.bik.frame_count());
    let mut newest = None;
    while state.next_frame < due {
        let i = state.next_frame;
        state.next_frame += 1;
        let video = match xiii_video::frame_video(&state.data, &state.bik, i) {
            Ok(v) => v,
            Err(e) => {
                state.errors += 1;
                state.first_error.get_or_insert_with(|| e.to_string());
                continue;
            }
        };
        match state
            .decoder
            .decode_frame(video, &state.bik.header, state.prev.as_ref())
        {
            Ok((frame, _stats)) => {
                state.prev = Some(frame);
                state.decoded += 1;
                newest = Some(i);
            }
            Err(e) => {
                state.errors += 1;
                state
                    .first_error
                    .get_or_insert_with(|| format!("frame {i}: {e}"));
            }
        }
    }
    if newest.is_some()
        && let Some(frame) = state.prev.as_ref()
    {
        let rgba = frame.to_rgba();
        if let Some(mut img) = images.get_mut(&state.handle) {
            img.data = Some(rgba);
        }
        state.last_advance = Instant::now();
    }
    let _ = &cfg;
}

fn unattended(
    mut commands: Commands,
    cfg: Res<VideoConfig>,
    state: Option<ResMut<VideoState>>,
    flag: Res<ShotFlag>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(mut state) = state else {
        return;
    };
    let Some(secs) = cfg.options.exit_after_secs else {
        return;
    };
    let elapsed = state.start.elapsed().as_secs_f32();
    if flag.0 {
        state.shot_done = true;
    }
    if let Some(path) = &cfg.options.screenshot
        && state.shot == 0
        && elapsed >= secs * 0.75
    {
        let path = path.clone();
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            let _ = std::fs::create_dir_all(dir);
        }
        commands
            .spawn(Screenshot::primary_window())
            .observe(save_to_disk(path))
            .observe(|_: On<ScreenshotCaptured>, mut f: ResMut<ShotFlag>| f.0 = true);
        state.shot = 1;
    }
    if elapsed < secs {
        return;
    }
    let reached = *state.target_at.get_or_insert_with(Instant::now);
    if state.shot == 1 && !state.shot_done && reached.elapsed() < Duration::from_secs(5) {
        return;
    }
    println!(
        "[video] exit after {:.1}s: {} frames decoded, {} errors, {} total frames{}",
        elapsed,
        state.decoded,
        state.errors,
        state.bik.frame_count(),
        match &state.first_error {
            Some(e) => format!("; first error: {e}"),
            None => String::new(),
        }
    );
    exit.write(AppExit::Success);
}
