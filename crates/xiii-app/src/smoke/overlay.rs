//! Text overlay: FPS/frame time, adapter/backend, window size, recent input.

use std::collections::VecDeque;

use bevy::diagnostic::{DiagnosticsStore, FrameTimeDiagnosticsPlugin};
use bevy::input::ButtonState;
use bevy::input::keyboard::KeyboardInput;
use bevy::input::mouse::{AccumulatedMouseMotion, MouseButtonInput};
use bevy::prelude::*;
use bevy::render::renderer::RenderAdapterInfo;
use bevy::window::PrimaryWindow;

use super::audio::ToneStats;

const HISTORY: usize = 6;

pub struct OverlayPlugin;

impl Plugin for OverlayPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<InputLog>()
            .add_systems(Startup, spawn_overlay)
            .add_systems(Update, (record_input, update_overlay).chain());
    }
}

#[derive(Resource, Default)]
struct InputLog {
    keys: VecDeque<String>,
    buttons: VecDeque<String>,
    last_motion: Vec2,
    total_motion: Vec2,
}

fn push(q: &mut VecDeque<String>, s: String) {
    if q.len() == HISTORY {
        q.pop_front();
    }
    q.push_back(s);
}

fn state_str(s: ButtonState) -> &'static str {
    match s {
        ButtonState::Pressed => "down",
        ButtonState::Released => "up",
    }
}

#[derive(Component)]
struct OverlayText;

fn spawn_overlay(mut commands: Commands) {
    commands.spawn((
        OverlayText,
        Text::new("starting..."),
        TextFont {
            font_size: FontSize::Px(15.0),
            ..default()
        },
        TextColor(Color::srgb(0.92, 0.95, 0.85)),
        Node {
            position_type: PositionType::Absolute,
            top: px(8),
            left: px(8),
            padding: UiRect::all(px(6)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
    ));
}

fn record_input(
    mut keys: MessageReader<KeyboardInput>,
    mut buttons: MessageReader<MouseButtonInput>,
    motion: Res<AccumulatedMouseMotion>,
    mut log: ResMut<InputLog>,
) {
    for k in keys.read() {
        if k.repeat {
            continue;
        }
        push(
            &mut log.keys,
            format!("{:?} {}", k.key_code, state_str(k.state)),
        );
    }
    for b in buttons.read() {
        push(
            &mut log.buttons,
            format!("{:?} {}", b.button, state_str(b.state)),
        );
    }
    log.last_motion = motion.delta;
    log.total_motion += motion.delta;
}

fn update_overlay(
    diagnostics: Res<DiagnosticsStore>,
    adapter: Option<Res<RenderAdapterInfo>>,
    window: Single<&Window, With<PrimaryWindow>>,
    log: Res<InputLog>,
    tone: Res<ToneStats>,
    mut text: Single<&mut Text, With<OverlayText>>,
) {
    let fps = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FPS)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    let ms = diagnostics
        .get(&FrameTimeDiagnosticsPlugin::FRAME_TIME)
        .and_then(|d| d.smoothed())
        .unwrap_or(0.0);
    let adapter = adapter.map_or_else(
        || "adapter: <pending>".to_string(),
        |a| format!("adapter: {} [{:?}, {:?}]", a.name, a.backend, a.device_type),
    );
    let join = |q: &VecDeque<String>| {
        if q.is_empty() {
            "-".to_string()
        } else {
            q.iter().cloned().collect::<Vec<_>>().join(", ")
        }
    };
    text.0 = format!(
        "XIII runtime smoke test (no game data)\n\
         {fps:.1} fps | {ms:.2} ms\n\
         {adapter}\n\
         window: {}x{} logical, {}x{} physical, scale {:.2}, {:?}\n\
         keys: {}\n\
         mouse buttons: {}\n\
         mouse delta: ({:+.1}, {:+.1})  total ({:+.0}, {:+.0})\n\
         audio tone: requested {} / started {}\n\
         axes: +X red, +Y green, -Z blue (Bevy forward)\n\
         WASD/QE move, Shift fast, RMB look, Space tone, Esc quit",
        window.width(),
        window.height(),
        window.physical_width(),
        window.physical_height(),
        window.scale_factor(),
        window.present_mode,
        join(&log.keys),
        join(&log.buttons),
        log.last_motion.x,
        log.last_motion.y,
        log.total_motion.x,
        log.total_motion.y,
        tone.requested,
        tone.started,
    );
}
