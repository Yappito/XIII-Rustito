//! Particle-emitter rendering (item5f): steps the deterministic CPU simulators in
//! [`xiii_world::particles`] and draws their live particles.
//!
//! Sprite emitters are drawn as camera-facing quads (one dynamic mesh per sub-emitter, vertex
//! colours carry the `ColorScale` and fade); `MeshEmitter` particles reuse the decoded static-mesh
//! geometry. The `DrawStyle` maps to a Bevy [`AlphaMode`] (the same enum-name mapping the material
//! resolver uses). Textures come from the shared [`crate::viewer::spawn_scene_geometry`] image
//! handles, so particles go through the existing texture path.
//!
//! This is a diagnostic reconstruction: the exact original blending for `Darken`/`Modulate` and
//! the sprite size units are not verified against the retail renderer and are labelled as
//! hypotheses in the task report.

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::RenderLayers;
use bevy::mesh::Indices;
use bevy::prelude::*;
use bevy::render::render_resource::{Face, PrimitiveTopology};

use xiii_decode::common::{BevyTransform, rotator_to_bevy_matrix, to_bevy_position};
use xiii_world::particles::{
    EmitterDesc, EmitterShape, EmitterSim, ParticleSystem, fade_factors, initially_enabled,
};

/// All particle systems of the loaded scene (read-only; the per-entity sims live in
/// [`ParticleEmitterRender`]).
#[derive(Resource, Default)]
pub struct ParticleRenderData {
    /// Systems in map order.
    pub systems: Vec<ParticleSystem>,
}

/// One rendered sub-emitter: its simulator, mesh and material handles.
#[derive(Component)]
pub struct ParticleEmitterRender {
    /// Index into [`ParticleRenderData::systems`].
    pub system: usize,
    /// Index into the system's `emitters`.
    pub emitter: usize,
    /// Per-sub-emitter deterministic simulator.
    pub sim: EmitterSim,
    /// Active particle count of the previous frame (avoids re-adding an empty mesh).
    pub last_count: usize,
}

/// Registers the particle update system. Added by the viewer and `--play` (one hook line each).
pub struct ParticlePlugin;

impl Plugin for ParticlePlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<ParticleRenderData>()
            .add_systems(Update, update_particles);
    }
}

/// Spawns one entity per sub-emitter and inserts the shared [`ParticleRenderData`]. Called from
/// [`crate::viewer::spawn_scene_geometry`] so every scene-building caller gets particles.
///
/// `force_all` (`--particles all`) ignores the level-start state and enables every emitter, for
/// inspecting triggered effects in the viewer.
pub fn spawn_particles(
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    image_handles: &[Handle<Image>],
    scene: &xiii_world::WorldScene,
    force_all: bool,
) {
    if scene.particle_systems.is_empty() {
        return;
    }
    // Diagnostic off-switch for before/after comparison captures, like `XIII_VIEWER_NO_SKY`.
    if std::env::var_os("XIII_VIEWER_NO_PARTICLES").is_some() {
        return;
    }
    let fallback = fallback_texture(images);
    for (si, system) in scene.particle_systems.iter().enumerate() {
        for (ei, desc) in system.emitters.iter().enumerate() {
            if matches!(desc.shape, EmitterShape::Unsupported(_)) {
                continue;
            }
            let (texture_handle, two_sided) = match &desc.shape {
                EmitterShape::Mesh(mesh) => (
                    mesh.texture
                        .and_then(|t| image_handles.get(t).cloned())
                        .unwrap_or_else(|| fallback.clone()),
                    mesh.two_sided,
                ),
                _ => (
                    desc.texture
                        .and_then(|t| image_handles.get(t).cloned())
                        .unwrap_or_else(|| fallback.clone()),
                    true,
                ),
            };
            let material = materials.add(particle_material(
                desc.draw_style,
                texture_handle,
                two_sided,
            ));
            let mesh = meshes.add(Mesh::new(
                PrimitiveTopology::TriangleList,
                RenderAssetUsages::RENDER_WORLD,
            ));
            let mut sim = EmitterSim::new(si.wrapping_mul(97).wrapping_add(ei));
            sim.set_enabled(force_all || initially_enabled(system, desc));
            commands.spawn((
                ParticleEmitterRender {
                    system: si,
                    emitter: ei,
                    sim,
                    last_count: 0,
                },
                Mesh3d(mesh),
                MeshMaterial3d(material),
                RenderLayers::layer(crate::viewer::MAIN_LAYER),
                Transform::IDENTITY,
                Name::new(format!("particles {}#{}", system.path, desc.class)),
            ));
        }
    }
    commands.insert_resource(ParticleRenderData {
        systems: scene.particle_systems.clone(),
    });
}

/// Maps `EParticleDrawStyle` to Bevy's [`AlphaMode`]. Enum order is the UT2003 reference order
/// plus the XIII `PTDS_Masked` tail (measured name-table order in `engine.u`; not verified
/// against the retail framebuffer states, a **hypothesis**).
pub fn draw_style_alpha(style: u8) -> AlphaMode {
    match style {
        0 => AlphaMode::Opaque,    // PTDS_Regular
        1 => AlphaMode::Blend,     // PTDS_AlphaBlend
        2 => AlphaMode::Multiply,  // PTDS_Modulated
        3 => AlphaMode::Blend,     // PTDS_Translucent
        4 => AlphaMode::Blend,     // PTDS_AlphaModulate
        5 => AlphaMode::Multiply,  // PTDS_Darken (subtractive approximated)
        6 => AlphaMode::Add,       // PTDS_Brighten
        7 => AlphaMode::Mask(0.5), // PTDS_Masked
        _ => AlphaMode::Blend,
    }
}

fn particle_material(draw_style: u8, texture: Handle<Image>, two_sided: bool) -> StandardMaterial {
    StandardMaterial {
        base_color_texture: Some(texture),
        unlit: true,
        alpha_mode: draw_style_alpha(draw_style),
        cull_mode: if two_sided { None } else { Some(Face::Back) },
        ..default()
    }
}

/// A procedural soft disc used when a sub-emitter has no resolvable texture. It contains no game
/// data (diagnostic placeholder; the missing texture is counted as `particle.note.no_texture`).
fn fallback_texture(images: &mut Assets<Image>) -> Handle<Image> {
    const SIZE: u32 = 64;
    let mut pixels = vec![0u8; (SIZE * SIZE * 4) as usize];
    let c = (SIZE as f32 - 1.0) * 0.5;
    for y in 0..SIZE {
        for x in 0..SIZE {
            let dx = (x as f32 - c) / c;
            let dy = (y as f32 - c) / c;
            let d = (dx * dx + dy * dy).sqrt();
            let a = ((1.0 - d).clamp(0.0, 1.0) * 255.0) as u8;
            let o = ((y * SIZE + x) * 4) as usize;
            pixels[o..o + 4].copy_from_slice(&[255, 255, 255, a]);
        }
    }
    images.add(Image::new(
        bevy::render::render_resource::Extent3d {
            width: SIZE,
            height: SIZE,
            depth_or_array_layers: 1,
        },
        bevy::render::render_resource::TextureDimension::D2,
        pixels,
        bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    ))
}

/// A Bevy-space emitter point: per-axis scale, rotation then translation (the placement rule in
/// `xiii_decode::common::actor_to_bevy`).
fn apply_transform(t: &BevyTransform, v: Vec3) -> Vec3 {
    let scaled = Vec3::new(v.x * t.scale[0], v.y * t.scale[1], v.z * t.scale[2]);
    let r = Mat3::from_cols_array(&xiii_world::to_cols(&t.rotation));
    r * scaled + Vec3::from_array(t.translation)
}

/// Rotator (pitch, yaw, roll in Unreal units) to a Bevy rotation matrix.
fn spin_matrix(spin: [f32; 3]) -> Mat3 {
    let rot = [
        spin[0].round() as i32,
        spin[1].round() as i32,
        spin[2].round() as i32,
    ];
    Mat3::from_cols_array(&xiii_world::to_cols(&rotator_to_bevy_matrix(rot)))
}

/// Particle vertex colour from the `ColorScale` RGB and the fade factors (alpha from the fade;
/// the stored color alpha is unused upstream).
fn particle_color(desc: &EmitterDesc, p: &xiii_world::particles::Particle) -> [f32; 4] {
    let f = fade_factors(desc, p.age);
    [
        (f32::from(p.color[0]) / 255.0) * f[0],
        (f32::from(p.color[1]) / 255.0) * f[1],
        (f32::from(p.color[2]) / 255.0) * f[2],
        f[3].clamp(0.0, 1.0),
    ]
}

/// UV sub-rect of subdivision frame `frame` in a `cols x rows` atlas.
fn subdivision_uv(desc: &EmitterDesc, frame: u32) -> [f32; 4] {
    let cols = desc.subdivisions_u.max(1);
    let rows = desc.subdivisions_v.max(1);
    let frame = frame % (cols * rows).max(1);
    let col = frame % cols;
    let row = frame / cols;
    let du = 1.0 / cols as f32;
    let dv = 1.0 / rows as f32;
    [
        col as f32 * du,
        row as f32 * dv,
        (col + 1) as f32 * du,
        (row + 1) as f32 * dv,
    ]
}

/// Rebuilds the dynamic mesh of one sub-emitter from its live particles, in world space (the
/// entity transform stays identity).
fn build_mesh(
    desc: &EmitterDesc,
    sim: &EmitterSim,
    transform: &BevyTransform,
    camera: Option<&Transform>,
) -> Mesh {
    let (cam_right, cam_up) = camera
        .map(|t| (t.rotation * Vec3::X, t.rotation * Vec3::Y))
        .unwrap_or((Vec3::X, Vec3::Y));
    let scale = 1.0 / xiii_decode::common::UNREAL_UNITS_PER_METER;
    let mut positions: Vec<[f32; 3]> = Vec::new();
    let mut normals: Vec<[f32; 3]> = Vec::new();
    let mut uvs: Vec<[f32; 2]> = Vec::new();
    let mut colors: Vec<[f32; 4]> = Vec::new();
    let mut indices: Vec<u32> = Vec::new();
    for p in sim.particles.iter().filter(|p| p.active) {
        let color = particle_color(desc, p);
        let centre = apply_transform(transform, Vec3::from_array(to_bevy_position(p.position)));
        match &desc.shape {
            EmitterShape::Mesh(reference) => {
                let basis = Mat3::from_cols_array(&xiii_world::to_cols(&transform.rotation))
                    * spin_matrix(p.spin);
                let particle_scale = Vec3::new(
                    p.size[0].abs() * scale * transform.scale[0].abs(),
                    p.size[1].abs() * scale * transform.scale[1].abs(),
                    p.size[2].abs() * scale * transform.scale[2].abs(),
                );
                let base = positions.len() as u32;
                for (i, pos) in reference.positions.iter().enumerate() {
                    let local = Vec3::from_array(*pos) * particle_scale;
                    positions.push((basis * local + centre).to_array());
                    let n = reference.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]);
                    normals.push((basis * Vec3::from_array(n)).normalize_or_zero().to_array());
                    uvs.push(reference.uvs.get(i).copied().unwrap_or([0.0, 0.0]));
                    colors.push(color);
                }
                indices.extend(reference.indices.iter().map(|&i| base + i));
            }
            _ => {
                let (w, h) = (p.size[0].abs() * scale * 0.5, p.size[1].abs() * scale * 0.5);
                let right = cam_right * w;
                let up = cam_up * h;
                let [u0, v0, u1, v1] = subdivision_uv(desc, p.subdivision);
                let base = positions.len() as u32;
                positions.push((centre - right + up).to_array());
                positions.push((centre + right + up).to_array());
                positions.push((centre + right - up).to_array());
                positions.push((centre - right - up).to_array());
                // Billboard normal faces the camera (diagnostic; not used by unlit materials).
                let n = camera
                    .map(|t| (centre - t.translation).normalize_or_zero().to_array())
                    .unwrap_or([0.0, 0.0, 1.0]);
                for _ in 0..4 {
                    normals.push(n);
                    colors.push(color);
                }
                uvs.extend_from_slice(&[[u0, v0], [u1, v0], [u1, v1], [u0, v1]]);
                indices.extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
            }
        }
    }
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    if positions.is_empty() {
        return mesh;
    }
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, colors);
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

/// Steps every sub-emitter simulator and rebuilds its mesh. Registered by [`ParticlePlugin`].
///
/// A fresh mesh asset is added and the entity's `Mesh3d` handle is swapped instead of mutating
/// the existing asset in place: replacing the vertex buffers of a live `RENDER_WORLD` mesh
/// triggers `bevy_render::slab_allocator` use-after-free errors.
pub fn update_particles(
    mut commands: Commands,
    time: Res<Time>,
    data: Res<ParticleRenderData>,
    cameras: Query<(&Transform, &Camera)>,
    mut emitters: Query<(Entity, &mut ParticleEmitterRender)>,
    mut meshes: ResMut<Assets<Mesh>>,
) {
    if data.systems.is_empty() {
        return;
    }
    // The main camera is the highest-order 3D camera (the sky camera has order 0).
    let camera = cameras.iter().max_by_key(|(_, c)| c.order).map(|(t, _)| *t);
    let dt = time.delta_secs();
    for (entity, mut e) in &mut emitters {
        let Some(system) = data.systems.get(e.system) else {
            continue;
        };
        let Some(desc) = system.emitters.get(e.emitter) else {
            continue;
        };
        e.sim.step(desc, dt);
        let count = e.sim.active();
        if count == 0 && e.last_count == 0 {
            continue;
        }
        let mesh = build_mesh(desc, &e.sim, &system.transform, camera.as_ref());
        let handle = meshes.add(mesh);
        commands.entity(entity).insert(Mesh3d(handle));
        e.last_count = count;
    }
}

/// Reads back the total number of live particles.
pub fn live_particle_count(emitters: &Query<&ParticleEmitterRender>) -> usize {
    emitters.iter().map(|e| e.sim.active()).sum()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use xiii_world::particles::ParticleValue;

    #[test]
    fn draw_styles_map_to_expected_alpha_modes() {
        assert_eq!(draw_style_alpha(0), AlphaMode::Opaque);
        assert_eq!(draw_style_alpha(6), AlphaMode::Add);
        assert_eq!(draw_style_alpha(5), AlphaMode::Multiply);
        assert_eq!(draw_style_alpha(7), AlphaMode::Mask(0.5));
        // An unknown enum value stays visible rather than silently opaque.
        assert_eq!(draw_style_alpha(200), AlphaMode::Blend);
    }

    #[test]
    fn subdivision_uv_wraps_out_of_range_frames() {
        let mut values = BTreeMap::new();
        values.insert("textureusubdivisions".to_owned(), ParticleValue::Int(4));
        values.insert("texturevsubdivisions".to_owned(), ParticleValue::Int(2));
        let desc = xiii_world::particles::decode_emitter("SpriteEmitter", "t", &values);
        let uv = subdivision_uv(&desc, 0);
        assert!((uv[0]).abs() < 1e-6 && (uv[1]).abs() < 1e-6);
        // Frame 99 wraps into the 4x2 atlas instead of sampling outside it.
        let uv = subdivision_uv(&desc, 99);
        assert!((0.0..=1.0).contains(&uv[0]) && (0.0..=1.0).contains(&uv[2]));
        // A single-cell atlas is the full texture.
        let empty = xiii_world::particles::decode_emitter("SpriteEmitter", "t", &BTreeMap::new());
        assert_eq!(subdivision_uv(&empty, 3), [0.0, 0.0, 1.0, 1.0]);
    }
}
