//! Projector (blob-shadow / projected-light) rendering as Bevy forward decals.
//!
//! ## Rendering approach
//!
//! Bevy 0.19.1 ships [`ForwardDecal`] / [`ForwardDecalMaterial<StandardMaterial>`] (verified in
//! `bevy_pbr 0.19.1`; the `ForwardDecalPlugin` is added by `PbrPlugin` unconditionally). A
//! forward decal is a 1x1 quad projected by the material shader onto the underlying geometry
//! using a depth prepass, which is exactly a UE2 projector's behaviour: a texture projected onto
//! whatever the projector's frustum sees. The camera must carry
//! [`bevy::core_pipeline::prepass::DepthPrepass`] (see [`super::fog::with_decals`]).
//!
//! UE2 projectors project through a cone of half-angle `FOV` (default 45 when `FOV` is 0) and
//! are capped by `MaxTraceDistance`. The forward-decal quad is scaled to the frustum cross
//! section at that distance and oriented with the actor's forward/up axes; the decal's own
//! projection does the rest. This is an approximation of the cone as a box (documented as such),
//! chosen because it needs no custom shader and is verifiable in a screenshot.
//!
//! ## Static map projectors and runtime shadows
//!
//! * Static map-placed projectors are spawned once at setup from
//!   [`xiii_world::WorldScene::projectors`], each with its resolved `ProjTexture` material
//!   ([`ProjectorDef::material_index`]).
//! * Runtime `ShadowProjector` actors a pawn spawns are decoded from the VM each frame
//!   ([`xiii_world::projectors::projector_pose_from_vm`]) and a decal entity follows the owning
//!   pawn. The shadow's `ProjTexture` is built at `ShadowProjector.PostBeginPlay` in the VM; when
//!   the VM cannot resolve that runtime material the decal uses a generated soft blob so the
//!   behaviour is still visible (reported, never claimed as the exact asset).

use std::collections::HashMap;

use bevy::camera::visibility::RenderLayers;
use bevy::pbr::decal::{ForwardDecal, ForwardDecalMaterial, ForwardDecalMaterialExt};
use bevy::prelude::*;

use xiii_world::WorldScene;
use xiii_world::projectors::{ProjectorBlend, ProjectorDef, ProjectorPose};

use super::MAIN_LAYER;

/// Marker for a projector decal spawned from a static map actor. The entity's `Name` carries
/// the projector path for diagnostics.
#[derive(Component)]
pub struct ProjectorDecal;

/// Assets the projector decals need, created at setup.
#[derive(Resource)]
pub struct ProjectorAssets {
    /// Fallback soft blob texture for a runtime shadow whose `ProjTexture` was not resolved.
    pub blob: Handle<Image>,
    /// Material handle per resolved material index (shared with the scene's textures).
    pub materials: HashMap<usize, Handle<ForwardDecalMaterial<StandardMaterial>>>,
}

/// Half-angle of the projector cone in radians (`FOV`/2, or the UE2 22.5-degree default).
fn half_angle(def: &ProjectorDef) -> f32 {
    (def.effective_fov_degrees() * 0.5).to_radians()
}

/// Frustum cross-section half-width (radius) of a projector in metres at its trace distance.
fn frustum_radius(def: &ProjectorDef) -> f32 {
    let dist = def.max_distance_m().max(0.1);
    (dist * half_angle(def).tan()).max(0.1)
}

/// Orientation of the decal quad: the quad's normal (local +Y before the mesh rotation, but the
/// decal mesh is built facing +Y) must point along the projector forward. We use a full
/// `from_rotation_arc` so the cone axis is exact.
fn decal_rotation(pose: &ProjectorPose) -> Quat {
    let forward = Vec3::from_array(pose.forward).normalize_or_zero();
    if forward.length_squared() < 1e-6 {
        Quat::IDENTITY
    } else {
        Quat::from_rotation_arc(Vec3::Y, forward)
    }
}

/// Scale of the decal quad: the frustum cross-section, with the UE2 `VScale` applied to the
/// vertical axis.
fn decal_scale(def: &ProjectorDef) -> Vec3 {
    let r = frustum_radius(def) * def.draw_scale.max(0.01);
    Vec3::new(r * 2.0, 1.0, r * 2.0)
}

/// Alpha mode for a projector blend.
fn decal_alpha(blend: ProjectorBlend) -> AlphaMode {
    match blend {
        // A darken/modulate projector multiplies the receiver; a forward decal with `Multiply`
        // gives the same visual result for a blob shadow.
        ProjectorBlend::Darken | ProjectorBlend::Modulate => AlphaMode::Multiply,
        ProjectorBlend::Overwrite => AlphaMode::Opaque,
        ProjectorBlend::Alpha | ProjectorBlend::Other(_) => AlphaMode::Blend,
    }
}

/// Builds a decal material for one projector texture handle.
pub fn decal_material(
    materials: &mut Assets<ForwardDecalMaterial<StandardMaterial>>,
    texture: Handle<Image>,
    blend: ProjectorBlend,
    tint: Color,
) -> Handle<ForwardDecalMaterial<StandardMaterial>> {
    materials.add(ForwardDecalMaterial {
        base: StandardMaterial {
            base_color: tint,
            base_color_texture: Some(texture),
            alpha_mode: decal_alpha(blend),
            unlit: true,
            // The decal must not be culled by the receiver's winding.
            cull_mode: None,
            ..default()
        },
        extension: ForwardDecalMaterialExt {
            depth_fade_factor: 1.0,
        },
    })
}

/// Generates a soft round alpha blob used when a runtime `ShadowProjector`'s texture was not
/// resolved by the VM. Diagnostic fallback, labelled in the overlay.
fn blob_image(size: u32) -> Image {
    let mut pixels = vec![0u8; (size * size * 4) as usize];
    let c = (size as f32 - 1.0) * 0.5;
    for y in 0..size {
        for x in 0..size {
            let dx = (x as f32 - c) / c;
            let dy = (y as f32 - c) / c;
            let r = (dx * dx + dy * dy).sqrt();
            // Opaque black in the centre fading to transparent at the rim.
            let a = ((1.0 - r) * 2.0).clamp(0.0, 1.0);
            let i = ((y * size + x) * 4) as usize;
            pixels[i] = 0;
            pixels[i + 1] = 0;
            pixels[i + 2] = 0;
            pixels[i + 3] = (a * 255.0) as u8;
        }
    }
    let mut img = Image::new(
        bevy::render::render_resource::Extent3d {
            width: size,
            height: size,
            depth_or_array_layers: 1,
        },
        bevy::render::render_resource::TextureDimension::D2,
        pixels,
        bevy::render::render_resource::TextureFormat::Rgba8UnormSrgb,
        bevy::asset::RenderAssetUsages::RENDER_WORLD,
    );
    img.sampler =
        bevy::image::ImageSampler::Descriptor(bevy::image::ImageSamplerDescriptor::linear());
    img
}

/// Creates the projector assets (the fallback blob and one shared decal material per resolved
/// scene material index). Called once at setup.
pub fn setup_projector_assets(
    images: &mut Assets<Image>,
    scene_materials: &[Handle<Image>],
    scene: &WorldScene,
    decal_materials: &mut Assets<ForwardDecalMaterial<StandardMaterial>>,
) -> ProjectorAssets {
    let blob = images.add(blob_image(64));
    let mut materials = HashMap::new();
    for p in &scene.projectors {
        let Some(idx) = p.def.material_index else {
            continue;
        };
        let Some(tex) = scene_materials.get(idx) else {
            continue;
        };
        let blend = p.def.blend;
        let tint = projector_tint(&p.def);
        materials
            .entry(idx)
            .or_insert_with(|| decal_material(decal_materials, tex.clone(), blend, tint));
    }
    ProjectorAssets { blob, materials }
}

/// A shadow projector tints the texture by `ShadowIntensity/255`; a light projector is white.
fn projector_tint(def: &ProjectorDef) -> Color {
    match def.shadow_intensity {
        Some(i) => {
            let v = (i as f32 / 255.0).clamp(0.0, 1.0);
            Color::srgb(v, v, v)
        }
        None => Color::WHITE,
    }
}

/// Spawns decals for every static map-placed projector into the main layer.
pub fn spawn_static_projectors(
    commands: &mut Commands,
    scene: &WorldScene,
    assets: &ProjectorAssets,
    decal_materials: &mut Assets<ForwardDecalMaterial<StandardMaterial>>,
) -> usize {
    let mut count = 0;
    for p in &scene.projectors {
        let material = match p.def.material_index {
            Some(idx) => assets.materials.get(&idx).cloned(),
            None => None,
        };
        // A map projector with no texture (or a shadow whose runtime texture is not in the
        // scene) gets the diagnostic blob so the placement is still visible.
        let material = material.unwrap_or_else(|| {
            decal_material(
                decal_materials,
                assets.blob.clone(),
                p.def.blend,
                projector_tint(&p.def),
            )
        });
        commands.spawn((
            ProjectorDecal,
            ForwardDecal,
            MeshMaterial3d(material),
            RenderLayers::layer(MAIN_LAYER),
            Transform::from_translation(Vec3::from_array(p.position))
                .with_rotation(decal_rotation(p))
                .with_scale(decal_scale(&p.def)),
            Name::new(format!("projector {}", p.name)),
        ));
        count += 1;
    }
    count
}

/// Spawns one decal entity for a runtime projector pose, tagged so a follow system can move it.
pub fn spawn_runtime_decal(
    commands: &mut Commands,
    pose: &ProjectorPose,
    assets: &ProjectorAssets,
    decal_materials: &mut Assets<ForwardDecalMaterial<StandardMaterial>>,
) -> Entity {
    let material = match pose.def.material_index {
        Some(idx) => assets.materials.get(&idx).cloned(),
        None => None,
    };
    let material = material.unwrap_or_else(|| {
        decal_material(
            decal_materials,
            assets.blob.clone(),
            pose.def.blend,
            projector_tint(&pose.def),
        )
    });
    commands
        .spawn((
            RuntimeProjector,
            ForwardDecal,
            MeshMaterial3d(material),
            RenderLayers::layer(MAIN_LAYER),
            Transform::from_translation(Vec3::from_array(pose.position))
                .with_rotation(decal_rotation(pose))
                .with_scale(decal_scale(&pose.def)),
            Name::new(format!("shadow {}", pose.name)),
        ))
        .id()
}

/// Marker for a runtime-sourced projector decal (spawned by `--play` from the VM). The runtime
/// bookkeeping lives in [`RuntimeProjectorDecals`].
#[derive(Component)]
pub struct RuntimeProjector;

/// A downward collision probe over the imported scene's line soup, so a runtime blob shadow can
/// be placed on the surface below its pawn (UE2 projects the shadow downward onto the receiver).
#[derive(Resource, Default)]
pub struct GroundQuery {
    triangles: Vec<([[f32; 3]; 3], u32)>,
}

impl GroundQuery {
    /// Builds the probe from an imported scene's line collision.
    pub fn from_scene(scene: &WorldScene) -> Self {
        Self {
            triangles: scene.line_collision().collect(),
        }
    }

    /// Nearest hit below `origin` within `max_dist` metres: `(point, unit normal)`.
    pub fn down(&self, origin: [f32; 3], max_dist: f32) -> Option<([f32; 3], [f32; 3])> {
        let dir = [0.0f32, -1.0, 0.0];
        let mut best: Option<(f32, [f32; 3])> = None;
        for (t, _) in &self.triangles {
            if let Some(d) = ray_tri_down(origin, dir, t)
                && d <= max_dist
                && best.is_none_or(|(b, _)| d < b)
            {
                best = Some((d, triangle_normal(*t)));
            }
        }
        best.map(|(d, n)| ([origin[0], origin[1] - d, origin[2]], n))
    }
}

/// Ray/triangle distance for a downward ray (positive `t`, one-sided not required).
fn ray_tri_down(o: [f32; 3], d: [f32; 3], t: &[[f32; 3]; 3]) -> Option<f32> {
    let sub = |a: [f32; 3], b: [f32; 3]| [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    let cross = |a: [f32; 3], b: [f32; 3]| {
        [
            a[1] * b[2] - a[2] * b[1],
            a[2] * b[0] - a[0] * b[2],
            a[0] * b[1] - a[1] * b[0],
        ]
    };
    let dot = |a: [f32; 3], b: [f32; 3]| a[0] * b[0] + a[1] * b[1] + a[2] * b[2];
    let e1 = sub(t[1], t[0]);
    let e2 = sub(t[2], t[0]);
    let p = cross(d, e2);
    let det = dot(e1, p);
    if det.abs() < 1e-12 {
        return None;
    }
    let inv = 1.0 / det;
    let s = sub(o, t[0]);
    let u = dot(s, p) * inv;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let q = cross(s, e1);
    let v = dot(d, q) * inv;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let dist = dot(e2, q) * inv;
    (dist > 1e-4).then_some(dist)
}

/// Unit triangle normal (winding-derived, unnormalised fallback zero).
fn triangle_normal(t: [[f32; 3]; 3]) -> [f32; 3] {
    let e1 = [t[1][0] - t[0][0], t[1][1] - t[0][1], t[1][2] - t[0][2]];
    let e2 = [t[2][0] - t[0][0], t[2][1] - t[0][1], t[2][2] - t[0][2]];
    let n = [
        e1[1] * e2[2] - e1[2] * e2[1],
        e1[2] * e2[0] - e1[0] * e2[2],
        e1[0] * e2[1] - e1[1] * e2[0],
    ];
    let len = (n[0] * n[0] + n[1] * n[1] + n[2] * n[2]).sqrt();
    if len < 1e-9 {
        [0.0, 1.0, 0.0]
    } else {
        [n[0] / len, n[1] / len, n[2] / len]
    }
}

/// Re-derives a runtime decal's `Transform` from a fresh pose (the pawn moved).
pub fn apply_pose(transform: &mut Transform, pose: &ProjectorPose) {
    transform.translation = Vec3::from_array(pose.position);
    transform.rotation = decal_rotation(pose);
    transform.scale = decal_scale(&pose.def);
}

/// Runtime projector decals currently spawned, keyed by VM object id (so a destroyed or
/// detached projector's decal is removed, and a moved owner's decal follows).
#[derive(Resource, Default)]
pub struct RuntimeProjectorDecals {
    /// VM object id -> decal entity and the last pose.
    pub active: std::collections::HashMap<u32, (Entity, ProjectorPose)>,
    /// Blob shadows successfully snapped to a surface below them.
    pub grounded: usize,
    /// Blob shadows with no surface under them (left at the actor position).
    pub ungrounded: usize,
}

/// Spawns/updates/removes runtime projector decals from the VM each frame. A `ShadowProjector`
/// is spawned per pawn with `bActorShadows`; the VM owns its `Location`, so the decal follows it.
/// `DetachProjector`/`Destroy` deletes the actor and the decal is removed with it.
///
/// The runtime shadow's `ProjTexture` is built by `ShadowProjector.PostBeginPlay` from a
/// procedural `ShadowBitmapMaterial`, which has no decodable image; those decals use the
/// generated soft blob (a labelled diagnostic fallback, not the exact asset).
pub fn update_runtime_projectors(
    mut commands: Commands,
    session: bevy::ecs::system::NonSend<Result<crate::play::session::Session, String>>,
    mut assets: Option<Res<ProjectorAssets>>,
    ground: Option<Res<GroundQuery>>,
    mut decals: ResMut<RuntimeProjectorDecals>,
    mut decal_materials: ResMut<Assets<ForwardDecalMaterial<StandardMaterial>>>,
    mut transforms: Query<&mut Transform>,
) {
    let Some(assets) = assets.take() else {
        return;
    };
    let Ok(session) = session.as_ref() else {
        return;
    };
    let vm = session.vm();
    decals.grounded = 0;
    decals.ungrounded = 0;
    let mut seen = std::collections::HashSet::new();
    for i in 0..vm.objects.len() {
        let id = i as u32;
        let o = &vm.objects[i];
        if !o.is_actor || o.deleted {
            continue;
        }
        if !o.name.to_ascii_lowercase().contains("projector") {
            continue;
        }
        let Some(mut pose) = xiii_world::projectors::projector_pose_from_vm(vm, id) else {
            continue;
        };
        // A runtime `ShadowProjector` is spawned at its owner's centre; UE2 projects it downward
        // onto the floor. Probe the collision soup below the actor and orient the blob to the
        // surface (the UE2 blob shadow is a ground decal, not a mid-air quad).
        if xiii_world::projectors::ProjectorDef::is_shadow_class(&pose.def.class_path) {
            let max = (pose.def.shadow_max_dist.unwrap_or(1500.0)
                / xiii_decode::common::UNREAL_UNITS_PER_METER)
                .max(1.0);
            if let Some((hit, normal)) = ground.as_deref().and_then(|g| g.down(pose.position, max))
            {
                pose.position = [hit[0], hit[1] + 0.03, hit[2]];
                pose.up = normal;
                pose.forward = if normal[1].abs() > 0.9 {
                    // A floor: the quad faces up, so its forward is a horizontal axis.
                    [0.0, 0.0, -1.0]
                } else {
                    normal
                };
                decals.grounded += 1;
            } else {
                decals.ungrounded += 1;
            }
        }
        seen.insert(id);
        match decals.active.get_mut(&id) {
            Some((entity, last)) => {
                if let Ok(mut t) = transforms.get_mut(*entity) {
                    apply_pose(&mut t, &pose);
                }
                *last = pose;
            }
            None => {
                let entity =
                    spawn_runtime_decal(&mut commands, &pose, &assets, &mut decal_materials);
                decals.active.insert(id, (entity, pose));
            }
        }
    }
    // Remove decals for projector actors that are gone or detached.
    decals.active.retain(|id, (entity, _)| {
        if seen.contains(id) {
            true
        } else {
            commands.entity(*entity).despawn();
            false
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_world::projectors::ProjectorBlend;

    #[test]
    fn radius_grows_with_fov_and_distance() {
        let mut d = ProjectorDef::engine_projector_default();
        d.fov = 90;
        d.max_trace_distance = 900; // 10 m
        // 10 m * tan(45) = 10 m radius.
        assert!(
            (frustum_radius(&d) - 10.0).abs() < 0.05,
            "{}",
            frustum_radius(&d)
        );
        d.fov = 0; // default 45-degree cone -> 22.5 half angle.
        let r = frustum_radius(&d);
        assert!((r - 10.0 * 22.5f32.to_radians().tan()).abs() < 0.05);
    }

    #[test]
    fn decal_alpha_maps_the_blend() {
        assert_eq!(decal_alpha(ProjectorBlend::Darken), AlphaMode::Multiply);
        assert_eq!(decal_alpha(ProjectorBlend::Alpha), AlphaMode::Blend);
        assert_eq!(decal_alpha(ProjectorBlend::Overwrite), AlphaMode::Opaque);
        assert_eq!(decal_alpha(ProjectorBlend::Other(9)), AlphaMode::Blend);
    }

    #[test]
    fn decal_rotation_points_the_quad_along_forward() {
        let pose = ProjectorPose {
            def: ProjectorDef::engine_shadow_default(),
            name: "p".into(),
            position: [0.0; 3],
            forward: [1.0, 0.0, 0.0],
            up: [0.0, 1.0, 0.0],
        };
        let q = decal_rotation(&pose);
        let n = q * Vec3::Y;
        assert!((n - Vec3::X).length() < 1e-4, "{n:?}");
    }

    #[test]
    fn blob_is_round_and_faded() {
        let img = blob_image(8);
        let data = img.data.as_ref().expect("blob has pixels");
        let at = |x: usize, y: usize| data[(y * 8 + x) * 4 + 3];
        assert_eq!(at(0, 0), 0, "corner is transparent");
        assert!(at(4, 4) > 200, "centre is opaque");
    }
}
