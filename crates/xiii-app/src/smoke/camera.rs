//! Diagnostic fly camera: WASD/QE move, Shift for speed, right mouse to look.

use bevy::input::mouse::AccumulatedMouseMotion;
use bevy::prelude::*;
use bevy::window::{CursorGrabMode, CursorOptions, PrimaryWindow};

pub struct FlyCameraPlugin;

impl Plugin for FlyCameraPlugin {
    fn build(&self, app: &mut App) {
        app.add_systems(Startup, spawn_camera)
            .add_systems(Update, (fly_look, fly_move).chain());
    }
}

#[derive(Component)]
pub struct FlyCamera {
    pub yaw: f32,
    pub pitch: f32,
    /// Metres per second.
    pub speed: f32,
    /// Radians per mouse count.
    pub sensitivity: f32,
}

fn spawn_camera(mut commands: Commands) {
    // Look from front-right-above so +X (right), +Y (up) and -Z (away) are
    // all visible and unambiguous.
    let transform =
        Transform::from_xyz(4.5, 4.0, 7.0).looking_at(Vec3::new(0.0, 0.5, -1.0), Vec3::Y);
    let (yaw, pitch, _) = transform.rotation.to_euler(EulerRot::YXZ);
    commands.spawn((
        Camera3d::default(),
        transform,
        FlyCamera {
            yaw,
            pitch,
            speed: 4.0,
            sensitivity: 0.003,
        },
    ));
}

fn fly_look(
    buttons: Res<ButtonInput<MouseButton>>,
    motion: Res<AccumulatedMouseMotion>,
    mut cursor: Single<&mut CursorOptions, With<PrimaryWindow>>,
    mut cams: Query<(&mut FlyCamera, &mut Transform)>,
) {
    if buttons.just_pressed(MouseButton::Right) {
        cursor.grab_mode = CursorGrabMode::Locked;
        cursor.visible = false;
    }
    if buttons.just_released(MouseButton::Right) {
        cursor.grab_mode = CursorGrabMode::None;
        cursor.visible = true;
    }
    if !buttons.pressed(MouseButton::Right) || motion.delta == Vec2::ZERO {
        return;
    }
    for (mut cam, mut t) in &mut cams {
        cam.yaw -= motion.delta.x * cam.sensitivity;
        cam.pitch = (cam.pitch - motion.delta.y * cam.sensitivity).clamp(-1.54, 1.54);
        t.rotation = Quat::from_euler(EulerRot::YXZ, cam.yaw, cam.pitch, 0.0);
    }
}

fn fly_move(
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mut cams: Query<(&FlyCamera, &mut Transform)>,
) {
    let mut axis = Vec3::ZERO;
    for (key, dir) in [
        (KeyCode::KeyW, Vec3::NEG_Z),
        (KeyCode::KeyS, Vec3::Z),
        (KeyCode::KeyA, Vec3::NEG_X),
        (KeyCode::KeyD, Vec3::X),
        (KeyCode::KeyQ, Vec3::NEG_Y),
        (KeyCode::KeyE, Vec3::Y),
    ] {
        if keys.pressed(key) {
            axis += dir;
        }
    }
    if axis == Vec3::ZERO {
        return;
    }
    let boost = if keys.any_pressed([KeyCode::ShiftLeft, KeyCode::ShiftRight]) {
        4.0
    } else {
        1.0
    };
    for (cam, mut t) in &mut cams {
        // Horizontal movement follows the view yaw; Q/E move along world Y.
        let yaw = Quat::from_rotation_y(cam.yaw);
        let planar = yaw * Vec3::new(axis.x, 0.0, axis.z);
        let delta = (planar + Vec3::Y * axis.y).normalize_or_zero();
        t.translation += delta * cam.speed * boost * time.delta_secs();
    }
}
