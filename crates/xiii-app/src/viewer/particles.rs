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
    BeamDesc, EmitterDesc, EmitterShape, EmitterSim, Particle, ParticleSystem, SparkDesc,
    fade_factors, initially_enabled,
};

/// Diagnostic cap on the number of points sampled along a beam path (bounds geometry).
const MAX_BEAM_POINTS: usize = 24;
/// Diagnostic cap on `RotatingSheets` (each sheet is a full ribbon).
const MAX_BEAM_SHEETS: u32 = 4;
/// Diagnostic cap on the beam length in Unreal units (~22 m at 90 UU/m).
const MAX_BEAM_UNITS: f32 = 2000.0;
/// Diagnostic cap on the number of spark trail segments.
const MAX_SPARK_SEGMENTS: u32 = 8;

/// All particle systems of the loaded scene (read-only; the per-entity sims live in
/// [`ParticleEmitterRender`]).
#[derive(Resource, Default)]
pub struct ParticleRenderData {
    /// Systems in map order.
    pub systems: Vec<ParticleSystem>,
}

/// Last per-frame particle census, used by `--benchmark` to show whether emitted particles
/// changed the workload between revisions.
#[derive(Resource, Default, Debug, Clone, Copy)]
pub struct ParticleStats {
    /// Renderable emitter components in the loaded map.
    pub emitters: usize,
    /// Emitters currently enabled by the VM bridge.
    pub enabled_emitters: usize,
    /// Emitters that the authored level-start properties enable.
    pub authored_enabled_emitters: usize,
    /// Active particles after the most recent simulator step.
    pub live_particles: usize,
}

/// One rendered sub-emitter: its simulator, mesh and material handles.
#[derive(Component)]
pub struct ParticleEmitterRender {
    /// Index into [`ParticleRenderData::systems`].
    pub system: usize,
    /// Index into the system's `emitters`.
    pub emitter: usize,
    /// Corresponding VM object after the play session links map exports.
    pub vm_id: Option<xiii_script::ObjectId>,
    /// Whether the VM path lookup has been attempted. `vm_id == None` alone cannot distinguish
    /// an unresolved emitter from a cached miss.
    pub vm_lookup_attempted: bool,
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
            .init_resource::<ParticleStats>()
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
            // Start with the full (empty) attribute layout the per-frame rebuild uses: a mesh
            // with no attributes has no valid vertex layout, which the depth prepass (needed by
            // the projector decals) rejects as a pipeline validation error.
            let mut empty = Mesh::new(
                PrimitiveTopology::TriangleList,
                RenderAssetUsages::RENDER_WORLD,
            );
            empty.insert_attribute(Mesh::ATTRIBUTE_POSITION, Vec::<[f32; 3]>::new());
            empty.insert_attribute(Mesh::ATTRIBUTE_NORMAL, Vec::<[f32; 3]>::new());
            empty.insert_attribute(Mesh::ATTRIBUTE_UV_0, Vec::<[f32; 2]>::new());
            empty.insert_attribute(Mesh::ATTRIBUTE_COLOR, Vec::<[f32; 4]>::new());
            let mesh = meshes.add(empty);
            let mut sim = EmitterSim::new(si.wrapping_mul(97).wrapping_add(ei));
            sim.set_enabled(force_all || initially_enabled(system, desc));
            commands.spawn((
                ParticleEmitterRender {
                    system: si,
                    emitter: ei,
                    vm_id: None,
                    vm_lookup_attempted: false,
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

/// Accumulator for the triangle list of one sub-emitter.
#[derive(Default)]
struct MeshAccum {
    positions: Vec<[f32; 3]>,
    normals: Vec<[f32; 3]>,
    uvs: Vec<[f32; 2]>,
    colors: Vec<[f32; 4]>,
    indices: Vec<u32>,
}

impl MeshAccum {
    fn base(&self) -> u32 {
        self.positions.len() as u32
    }

    /// Appends a quad `(a, b, c, d)` with one normal, colour and four UVs.
    fn quad(&mut self, p: [Vec3; 4], normal: Vec3, color: [f32; 4], uv: [[f32; 2]; 4]) {
        let base = self.base();
        for v in p {
            self.positions.push(v.to_array());
            self.normals.push(normal.to_array());
            self.colors.push(color);
        }
        self.uvs.extend_from_slice(&uv);
        self.indices
            .extend_from_slice(&[base, base + 1, base + 2, base, base + 2, base + 3]);
    }
}

/// Per-particle drawing context shared by the beam/spark builders.
struct DrawCtx<'a> {
    desc: &'a EmitterDesc,
    transform: &'a BevyTransform,
    camera: Option<&'a Transform>,
    scale: f32,
    color: [f32; 4],
}

/// Midpoint of a min/max range (order-independent). A min == max range is valid and returns that
/// value; `fallback` is used only when the midpoint is zero and the caller needs a non-zero value.
fn range_mid(r: [f32; 2], fallback: f32) -> f32 {
    let (lo, hi) = if r[0] <= r[1] {
        (r[0], r[1])
    } else {
        (r[1], r[0])
    };
    let mid = (lo + hi) * 0.5;
    if mid.abs() < 1e-6 { fallback } else { mid }
}

fn noise_mid(r: [[f32; 2]; 3]) -> Vec3 {
    // A zero noise range must stay zero (no fallback).
    Vec3::new(
        (r[0][0] + r[0][1]) * 0.5,
        (r[1][0] + r[1][1]) * 0.5,
        (r[2][0] + r[2][1]) * 0.5,
    )
}

/// Beam path samples in Unreal units relative to the emitter: a straight line along the particle
/// velocity (or acceleration) with low/high-frequency sinusoidal noise. `BeamDistanceRange`
/// selects the length when set; otherwise the particle's velocity over its lifetime is used.
/// The sample count is `LowFrequencyPoints + HighFrequencyPoints` (capped for geometry size).
/// **Hypothesis**: the retail beam integrates its own noise tables; this reproduces the intent.
fn beam_path(desc: &EmitterDesc, beam: &BeamDesc, p: &Particle) -> Vec<[f32; 3]> {
    let v = Vec3::from_array(p.velocity);
    let a = Vec3::from_array(desc.acceleration);
    let speed = v.length();
    let dir = if speed > 1e-3 {
        v / speed
    } else if a.length() > 1e-3 {
        a.normalize()
    } else {
        Vec3::X
    };
    // The beam reaches at least as far as the particle travels (the decoded trajectory); a
    // positive `BeamDistanceRange` extends it when larger. The exact `EBeamEndPointType`
    // semantics are not decoded, so this is a **hypothesis**.
    let travel = speed * p.lifetime;
    let configured = if beam.distance_range[1] > 0.0 {
        ((beam.distance_range[0] + beam.distance_range[1]) * 0.5).abs()
    } else {
        0.0
    };
    let length = travel.max(configured).clamp(1.0, MAX_BEAM_UNITS);
    let n = (beam.low_frequency_points + beam.high_frequency_points) as usize;
    let n = n.clamp(2, MAX_BEAM_POINTS);
    let lo = noise_mid(beam.low_frequency_noise);
    let hi = noise_mid(beam.high_frequency_noise);
    // Deterministic per-particle phase from the spawn position (same particle -> same beam).
    let seed = (p.position[0] * 12.9898 + p.position[1] * 78.233 + p.position[2] * 37.719).sin()
        * 43758.547;
    let phase = seed.rem_euclid(1.0) * std::f32::consts::TAU;
    let base = Vec3::from_array(p.position);
    (0..n)
        .map(|k| {
            let t = k as f32 / (n - 1) as f32;
            let w = t * std::f32::consts::PI;
            let off = Vec3::new(
                lo.x * (w + phase).sin() + hi.x * (5.0 * w + 2.0 * phase).sin(),
                lo.y * (1.3 * w + 1.7 * phase).sin() + hi.y * (6.0 * w + 3.0 * phase).sin(),
                lo.z * (0.7 * w + 2.3 * phase).sin() + hi.z * (4.0 * w + 1.3 * phase).sin(),
            );
            (base + dir * length * t + off).to_array()
        })
        .collect()
}

/// Appends up to [`MAX_BEAM_SHEETS`] textured ribbons along [`beam_path`], each rotated around
/// the beam tangent by an equal fraction of a half turn.
fn push_beam_ribbons(ctx: &DrawCtx<'_>, beam: &BeamDesc, p: &Particle, out: &mut MeshAccum) {
    let points: Vec<Vec3> = beam_path(ctx.desc, beam, p)
        .iter()
        .map(|q| apply_transform(ctx.transform, Vec3::from_array(to_bevy_position(*q))))
        .collect();
    if points.len() < 2 {
        return;
    }
    let half_width = p.size[0].abs() * ctx.scale * 0.5;
    let sheets = beam.rotating_sheets.clamp(1, MAX_BEAM_SHEETS);
    for sheet in 0..sheets {
        let angle = std::f32::consts::PI * sheet as f32 / sheets as f32;
        let base = out.base();
        for (k, &c) in points.iter().enumerate() {
            let t = k as f32 / (points.len() - 1) as f32;
            let tangent = if k == 0 {
                (points[1] - points[0]).normalize_or_zero()
            } else {
                (points[k] - points[k - 1]).normalize_or_zero()
            };
            let mut w = tangent.cross(Vec3::Y);
            if w.length_squared() < 1e-8 {
                w = tangent.cross(Vec3::X);
            }
            let w = Quat::from_axis_angle(tangent, angle) * w.normalize_or_zero();
            let left = c - w * half_width;
            let right = c + w * half_width;
            out.positions.push(left.to_array());
            out.positions.push(right.to_array());
            out.normals.push(w.to_array());
            out.normals.push(w.to_array());
            out.colors.push(ctx.color);
            out.colors.push(ctx.color);
            out.uvs.push([0.0, t * beam.texture_v_scale]);
            out.uvs
                .push([beam.texture_u_scale, t * beam.texture_v_scale]);
            if k > 0 {
                let prev = base + (k as u32 - 1) * 2;
                let cur = base + k as u32 * 2;
                out.indices
                    .extend_from_slice(&[prev, cur, cur + 1, prev, cur + 1, prev + 1]);
            }
        }
    }
}

/// Appends camera-facing line-sprite quads for a `SparkEmitter`: each particle is a trail of
/// `LineSegmentsRange` segments spaced by `TimeBetweenSegmentsRange`, drawn back along its
/// velocity. **Hypothesis**: the retail trail interpolates the particle's own recent positions.
fn push_spark_trails(ctx: &DrawCtx<'_>, spark: &SparkDesc, p: &Particle, out: &mut MeshAccum) {
    let v = Vec3::from_array(p.velocity);
    let speed = v.length();
    let dir = if speed > 1e-3 { v / speed } else { Vec3::Y };
    let spacing = range_mid(spark.time_between_segments, 1.0 / 60.0).max(1e-4);
    let segments =
        (range_mid(spark.line_segments, 1.0).round() as u32).clamp(1, MAX_SPARK_SEGMENTS);
    let half_width = (p.size[0].abs() * ctx.scale * 0.15).max(0.001);
    let points: Vec<Vec3> = (0..=segments)
        .map(|k| {
            let t = k as f32 * spacing;
            let q = Vec3::from_array(p.position) - dir * (speed * t);
            apply_transform(
                ctx.transform,
                Vec3::from_array(to_bevy_position(q.to_array())),
            )
        })
        .collect();
    let up = ctx.camera.map(|t| t.rotation * Vec3::Y).unwrap_or(Vec3::Y);
    for seg in points.windows(2) {
        let (a, b) = (seg[0], seg[1]);
        let seg_dir = (b - a).normalize_or_zero();
        let mut perp = seg_dir.cross(up);
        if perp.length_squared() < 1e-8 {
            perp = seg_dir.cross(Vec3::X);
        }
        let perp = perp.normalize_or_zero() * half_width;
        out.quad(
            [a - perp, a + perp, b + perp, b - perp],
            seg_dir.cross(perp).normalize_or_zero(),
            ctx.color,
            [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 1.0]],
        );
    }
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
    let mut out = MeshAccum::default();
    for p in sim.particles.iter().filter(|p| p.active) {
        let color = particle_color(desc, p);
        let centre = apply_transform(transform, Vec3::from_array(to_bevy_position(p.position)));
        let ctx = DrawCtx {
            desc,
            transform,
            camera,
            scale,
            color,
        };
        match &desc.shape {
            EmitterShape::Mesh(reference) => {
                let basis = Mat3::from_cols_array(&xiii_world::to_cols(&transform.rotation))
                    * spin_matrix(p.spin);
                let particle_scale = Vec3::new(
                    p.size[0].abs() * scale * transform.scale[0].abs(),
                    p.size[1].abs() * scale * transform.scale[1].abs(),
                    p.size[2].abs() * scale * transform.scale[2].abs(),
                );
                let base = out.base();
                for (i, pos) in reference.positions.iter().enumerate() {
                    let local = Vec3::from_array(*pos) * particle_scale;
                    out.positions.push((basis * local + centre).to_array());
                    let n = reference.normals.get(i).copied().unwrap_or([0.0, 1.0, 0.0]);
                    out.normals
                        .push((basis * Vec3::from_array(n)).normalize_or_zero().to_array());
                    out.uvs
                        .push(reference.uvs.get(i).copied().unwrap_or([0.0, 0.0]));
                    out.colors.push(color);
                }
                out.indices
                    .extend(reference.indices.iter().map(|&i| base + i));
            }
            EmitterShape::Beam(beam) => push_beam_ribbons(&ctx, beam, p, &mut out),
            EmitterShape::Spark(spark) => push_spark_trails(&ctx, spark, p, &mut out),
            EmitterShape::Sprite | EmitterShape::Unsupported(_) => {
                let (w, h) = (p.size[0].abs() * scale * 0.5, p.size[1].abs() * scale * 0.5);
                let right = cam_right * w;
                let up = cam_up * h;
                let [u0, v0, u1, v1] = subdivision_uv(desc, p.subdivision);
                out.positions.push((centre - right + up).to_array());
                out.positions.push((centre + right + up).to_array());
                out.positions.push((centre + right - up).to_array());
                out.positions.push((centre - right - up).to_array());
                // Billboard normal faces the camera (diagnostic; not used by unlit materials).
                let n = camera
                    .map(|t| (centre - t.translation).normalize_or_zero().to_array())
                    .unwrap_or([0.0, 0.0, 1.0]);
                for _ in 0..4 {
                    out.normals.push(n);
                    out.colors.push(color);
                }
                out.uvs
                    .extend_from_slice(&[[u0, v0], [u1, v0], [u1, v1], [u0, v1]]);
                let base = out.base();
                out.indices.extend_from_slice(&[
                    base,
                    base + 1,
                    base + 2,
                    base,
                    base + 2,
                    base + 3,
                ]);
            }
        }
    }
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    if out.positions.is_empty() {
        return mesh;
    }
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, out.positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, out.normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, out.uvs);
    mesh.insert_attribute(Mesh::ATTRIBUTE_COLOR, out.colors);
    mesh.insert_indices(Indices::U32(out.indices));
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
    mut stats: ResMut<ParticleStats>,
) {
    if data.systems.is_empty() {
        *stats = ParticleStats::default();
        return;
    }
    // The main camera is the highest-order 3D camera (the sky camera has order 0).
    let camera = cameras.iter().max_by_key(|(_, c)| c.order).map(|(t, _)| *t);
    let dt = time.delta_secs();
    let mut census = ParticleStats::default();
    for (entity, mut e) in &mut emitters {
        let Some(system) = data.systems.get(e.system) else {
            continue;
        };
        let Some(desc) = system.emitters.get(e.emitter) else {
            continue;
        };
        e.sim.step(desc, dt);
        let count = e.sim.active();
        census.emitters += 1;
        census.enabled_emitters += usize::from(e.sim.enabled);
        census.authored_enabled_emitters += usize::from(initially_enabled(system, desc));
        census.live_particles += count;
        if count == 0 && e.last_count == 0 {
            continue;
        }
        let mesh = build_mesh(desc, &e.sim, &system.transform, camera.as_ref());
        let handle = meshes.add(mesh);
        commands.entity(entity).insert(Mesh3d(handle));
        e.last_count = count;
    }
    *stats = census;
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

    fn particle(position: [f32; 3], velocity: [f32; 3]) -> xiii_world::particles::Particle {
        xiii_world::particles::Particle {
            position,
            velocity,
            age: 0.5,
            lifetime: 1.0,
            size: [20.0, 20.0, 20.0],
            color: [255, 255, 255, 255],
            spin: [0.0; 3],
            spin_rate: [0.0; 3],
            subdivision: 0,
            active: true,
        }
    }

    #[test]
    fn beam_path_uses_distance_range_and_caps_points() {
        let values: BTreeMap<String, ParticleValue> = [
            (
                "beamdistancerange".to_owned(),
                ParticleValue::Range([100.0, 100.0]),
            ),
            ("lowfrequencypoints".to_owned(), ParticleValue::Int(3)),
            ("highfrequencypoints".to_owned(), ParticleValue::Int(10)),
            (
                "highfrequencynoiserange".to_owned(),
                ParticleValue::RangeVector([[0.0, 0.0]; 3]),
            ),
        ]
        .into_iter()
        .collect();
        let desc = xiii_world::particles::decode_emitter("BeamEmitter", "b", &values);
        let EmitterShape::Beam(beam) = &desc.shape else {
            panic!("beam shape");
        };
        let pts = beam_path(&desc, beam, &particle([0.0, 0.0, 0.0], [100.0, 0.0, 0.0]));
        assert_eq!(pts.len(), 13);
        // First sample is the particle origin; last sample is the 100 UU distance along +X.
        assert!(pts[0].iter().all(|v| v.abs() < 1e-4), "{:?}", pts[0]);
        assert!(
            (pts.last().unwrap()[0] - 100.0).abs() < 1e-3,
            "{:?}",
            pts.last()
        );
        // A huge configured sample count is capped for geometry size.
        let mut many = values.clone();
        many.insert("highfrequencypoints".to_owned(), ParticleValue::Int(10_000));
        let desc = xiii_world::particles::decode_emitter("BeamEmitter", "b", &many);
        let EmitterShape::Beam(beam) = &desc.shape else {
            panic!("beam shape");
        };
        let pts = beam_path(&desc, beam, &particle([0.0; 3], [100.0, 0.0, 0.0]));
        assert!(pts.len() <= MAX_BEAM_POINTS);
    }

    #[test]
    fn beam_without_distance_uses_velocity_times_lifetime() {
        let values: BTreeMap<String, ParticleValue> = [
            ("lowfrequencypoints".to_owned(), ParticleValue::Int(1)),
            ("highfrequencypoints".to_owned(), ParticleValue::Int(1)),
        ]
        .into_iter()
        .collect();
        let desc = xiii_world::particles::decode_emitter("BeamEmitter", "b", &values);
        let EmitterShape::Beam(beam) = &desc.shape else {
            panic!("beam shape");
        };
        // 50 UU/s for a 1 s lifetime -> 50 UU.
        let pts = beam_path(&desc, beam, &particle([0.0; 3], [50.0, 0.0, 0.0]));
        assert!((pts.last().unwrap()[0] - 50.0).abs() < 1e-3);
        // A stationary particle still yields a finite, non-degenerate beam.
        let pts = beam_path(&desc, beam, &particle([0.0; 3], [0.0; 3]));
        assert!(pts.iter().all(|p| p.iter().all(|v| v.is_finite())));
    }

    #[test]
    fn spark_trails_emit_one_quad_per_segment() {
        let values: BTreeMap<String, ParticleValue> = [
            (
                "linesegmentsrange".to_owned(),
                ParticleValue::Range([4.0, 4.0]),
            ),
            (
                "timebetweensegmentsrange".to_owned(),
                ParticleValue::Range([0.01, 0.01]),
            ),
        ]
        .into_iter()
        .collect();
        let desc = xiii_world::particles::decode_emitter("SparkEmitter", "s", &values);
        let EmitterShape::Spark(spark) = &desc.shape else {
            panic!("spark shape");
        };
        let transform = xiii_decode::common::actor_to_bevy([0.0; 3], [0; 3], [1.0; 3]);
        let ctx = DrawCtx {
            desc: &desc,
            transform: &transform,
            camera: None,
            scale: 1.0 / xiii_decode::common::UNREAL_UNITS_PER_METER,
            color: [1.0; 4],
        };
        let mut out = MeshAccum::default();
        push_spark_trails(
            &ctx,
            spark,
            &particle([0.0; 3], [300.0, 0.0, 0.0]),
            &mut out,
        );
        assert_eq!(out.indices.len(), 4 * 6);
        assert_eq!(out.positions.len(), 4 * 4);
        assert!(
            out.positions
                .iter()
                .all(|p| p.iter().all(|v| v.is_finite()))
        );
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
