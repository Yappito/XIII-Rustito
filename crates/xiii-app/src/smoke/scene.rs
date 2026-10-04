//! Lit primitive scene plus an asymmetric axis marker.
//!
//! The marker is unlit so its colours read correctly regardless of lighting:
//! red bar along +X, green along +Y, blue along **-Z** (Bevy's camera forward),
//! and a white cube at the origin. A future Unreal->Bevy axis conversion can be
//! checked against this marker by importing a known-orientation asset.

use bevy::prelude::*;

pub struct ScenePlugin;

impl Plugin for ScenePlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(ClearColor(Color::srgb(0.10, 0.11, 0.14)))
            .insert_resource(GlobalAmbientLight {
                color: Color::WHITE,
                brightness: 150.0,
                ..default()
            })
            .add_systems(Startup, spawn_scene)
            .add_systems(Update, (spin, draw_grid));
    }
}

#[derive(Component)]
struct Spin(f32);

fn spawn_scene(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    // Ground.
    commands.spawn((
        Mesh3d(meshes.add(Plane3d::default().mesh().size(20.0, 20.0))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.35, 0.37, 0.40),
            perceptual_roughness: 0.9,
            ..default()
        })),
    ));

    // A few lit primitives placed away from the axis marker.
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::new(1.0, 1.0, 1.0))),
        MeshMaterial3d(materials.add(Color::srgb(0.85, 0.65, 0.35))),
        Transform::from_xyz(-3.0, 0.5, 1.5),
        Spin(0.6),
    ));
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(0.6))),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.75, 0.75, 0.80),
            metallic: 0.9,
            perceptual_roughness: 0.25,
            ..default()
        })),
        Transform::from_xyz(3.8, 0.6, -0.8),
    ));
    commands.spawn((
        Mesh3d(meshes.add(Cylinder::new(0.4, 1.6))),
        MeshMaterial3d(materials.add(Color::srgb(0.35, 0.55, 0.85))),
        Transform::from_xyz(-2.5, 0.8, -3.0),
    ));
    commands.spawn((
        Mesh3d(meshes.add(Torus::new(0.35, 0.7))),
        MeshMaterial3d(materials.add(Color::srgb(0.70, 0.35, 0.60))),
        Transform::from_xyz(2.5, 0.7, -3.0),
        Spin(-0.9),
    ));

    spawn_axis_marker(&mut commands, &mut meshes, &mut materials);

    // Key light with shadows.
    commands.spawn((
        DirectionalLight {
            illuminance: 10_000.0,
            shadow_maps_enabled: true,
            ..default()
        },
        Transform::from_xyz(4.0, 8.0, 3.0).looking_at(Vec3::ZERO, Vec3::Y),
    ));
}

fn spawn_axis_marker(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
) {
    const LEN: f32 = 2.0;
    const THICK: f32 = 0.12;
    const BASE_Y: f32 = 0.01;
    let mut unlit = |c: Color| {
        materials.add(StandardMaterial {
            base_color: c,
            unlit: true,
            ..default()
        })
    };
    let red = unlit(Color::srgb(1.0, 0.1, 0.1));
    let green = unlit(Color::srgb(0.1, 1.0, 0.1));
    let blue = unlit(Color::srgb(0.15, 0.3, 1.0));
    let white = unlit(Color::WHITE);

    let origin = Vec3::new(0.0, BASE_Y + THICK, 0.0);
    commands.spawn((
        Name::new("axis +X (red)"),
        Mesh3d(meshes.add(Cuboid::new(LEN, THICK, THICK))),
        MeshMaterial3d(red),
        Transform::from_translation(origin + Vec3::X * LEN * 0.5),
    ));
    commands.spawn((
        Name::new("axis +Y (green)"),
        Mesh3d(meshes.add(Cuboid::new(THICK, LEN, THICK))),
        MeshMaterial3d(green),
        Transform::from_translation(origin + Vec3::Y * LEN * 0.5),
    ));
    commands.spawn((
        Name::new("axis -Z (blue)"),
        Mesh3d(meshes.add(Cuboid::new(THICK, THICK, LEN))),
        MeshMaterial3d(blue),
        Transform::from_translation(origin - Vec3::Z * LEN * 0.5),
    ));
    commands.spawn((
        Name::new("axis origin"),
        Mesh3d(meshes.add(Cuboid::from_length(THICK * 2.0))),
        MeshMaterial3d(white),
        Transform::from_translation(origin),
    ));
}

fn spin(time: Res<Time>, mut q: Query<(&Spin, &mut Transform)>) {
    for (spin, mut t) in &mut q {
        t.rotate_y(spin.0 * time.delta_secs());
    }
}

/// 1 m reference grid on the ground plane (gizmos, redrawn each frame).
fn draw_grid(mut gizmos: Gizmos) {
    gizmos.grid(
        Isometry3d::new(
            Vec3::new(0.0, 0.002, 0.0),
            Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        ),
        UVec2::splat(20),
        Vec2::ONE,
        Color::srgba(1.0, 1.0, 1.0, 0.08),
    );
}
