//! Diagnostic skinned-character viewer (M2b runtime).
//!
//! Decodes an `Engine.SkeletalMesh` and its `Engine.MeshAnimation` from an owned installation,
//! uploads the mesh as a GPU-skinned Bevy `Mesh` (`JOINT_INDEX`/`JOINT_WEIGHT` plus a
//! `SkinnedMesh` with inverse-bind matrices) and drives one entity per bone from **our** CPU
//! animation sampler (`xiii_decode::skeletal::normalize::evaluate_pose`, the same function
//! `xiii-tool anim render/validate` uses). The Unreal -> Bevy coordinate policy is applied in
//! exactly one place, [`source_to_bevy_transform`], which is built only from the helpers and
//! constants in `xiii_decode::common` (`to_bevy_position`, `to_bevy_direction`,
//! `SOURCE_TO_BEVY`). Because the change of basis is linear, converting every local bone
//! transform is the same as applying it once at the skeleton root.
//!
//! Turntable camera, a 1 m floor grid and a clip/frame/time overlay; `--exit-after-secs`,
//! `--screenshot` and `--frame N` (frozen pose and front-on camera for comparisons) are
//! supported. Unlit diagnostic materials; textures come from the decoded material references
//! through the existing texture decoder. This is an importer diagnostic, not a playable mode.

use std::collections::{BTreeMap, HashMap};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy::asset::RenderAssetUsages;
use bevy::camera::visibility::NoFrustumCulling;
use bevy::image::{ImageAddressMode, ImageSampler, ImageSamplerDescriptor};
use bevy::mesh::Indices;
use bevy::mesh::VertexAttributeValues;
use bevy::mesh::skinning::{SkinnedMesh, SkinnedMeshInverseBindposes};
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, PrimitiveTopology, TextureDimension, TextureFormat};
use bevy::render::view::screenshot::{Screenshot, ScreenshotCaptured, save_to_disk};

use crate::cli::Options;
use xiii_decode::common::{
    SOURCE_TO_BEVY, UNREAL_UNITS_PER_METER, rotator_to_bevy_matrix, to_bevy_direction,
    to_bevy_position,
};
use xiii_decode::skeletal::math::Transform as SkTransform;
use xiii_decode::skeletal::normalize::{
    AnimSet, Clip, Skeleton, SkinnedMesh as DecodedMesh, evaluate_pose,
};
use xiii_decode::skeletal::{
    MESH_ANIMATION_CLASS, SKELETAL_MESH_CLASS, decode_mesh_animation, decode_skeletal_mesh,
};
use xiii_decode::texture::{RgbaImage, decode_texture};
use xiii_package::{Limits, ObjectRef, PropertyValue};
use xiii_world::{Loaded, PackageCache};

/// Viewer plugin for `--model`.
pub struct SkinnedPlugin {
    /// Parsed options.
    pub options: Options,
}

#[derive(Resource)]
struct SkinnedConfig {
    options: Options,
}

/// One bone entity; `character` indexes [`SkinnedScene::characters`], `bone` the skeleton.
#[derive(Component)]
struct BoneJoint {
    character: usize,
    bone: usize,
}

#[derive(Component)]
struct TurntableCam;

#[derive(Component)]
struct OverlayText;

/// Runtime data of one loaded character.
struct CharacterInfo {
    label: String,
    skeleton: Skeleton,
    anims: Option<AnimSet>,
    anim_label: Option<String>,
    clip: Option<usize>,
    bone_map: Vec<Option<usize>>,
}

#[derive(Resource, Default)]
struct SkinnedScene {
    characters: Vec<CharacterInfo>,
    center: Vec3,
    radius: f32,
    height: f32,
}

/// Clip playback state for the overlay (first character; all share `--anim`).
#[derive(Resource, Default)]
struct AnimClock {
    frame: f32,
    time: f32,
    clip: String,
}

#[derive(Resource)]
struct RunState {
    start: Instant,
    frame: u64,
    shot: u8,
    shot_done: bool,
    target_at: Option<Instant>,
}

#[derive(Resource, Default)]
struct ShotFlag(bool);

impl Plugin for SkinnedPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(SkinnedConfig {
            options: self.options.clone(),
        })
        .insert_resource(ClearColor(Color::srgb(0.16, 0.18, 0.22)))
        .init_resource::<SkinnedScene>()
        .init_resource::<AnimClock>()
        .init_resource::<ShotFlag>()
        .insert_resource(RunState {
            start: Instant::now(),
            frame: 0,
            shot: 0,
            shot_done: false,
            target_at: None,
        })
        .add_systems(Startup, setup)
        .add_systems(
            Update,
            (animate_bones, draw_helpers, turntable, overlay, unattended).chain(),
        );
    }
}

// ---------------------------------------------------------------------------------------
// Coordinate policy (single place; only xiii_decode::common constants are used)
// ---------------------------------------------------------------------------------------

/// Converts one source (Unreal) rigid local/mesh-space transform into Bevy space *once*.
///
/// `bevy = A * source`, where `A = diag(1/s) * SOURCE_TO_BEVY`. For the rotation, `R_b =
/// C R_s C^T` (the uniform scale cancels); for the translation, `t_b = to_bevy_position(t_s)`.
/// The same policy is used for the bind pose, the animation pose and the inverse-bind matrices,
/// so applying it to every local bone transform is identical to applying it at the skeleton
/// root (linear changes of basis commute with composition).
pub(crate) fn source_to_bevy_transform(t: &SkTransform) -> Transform {
    let rs = Mat3::from_quat(Quat::from_xyzw(
        t.rotation.x,
        t.rotation.y,
        t.rotation.z,
        t.rotation.w,
    ));
    let c = Mat3::from_cols_array(&xiii_world::to_cols(&SOURCE_TO_BEVY));
    let rb = c * rs * c.transpose();
    Transform {
        translation: Vec3::from_array(to_bevy_position(t.translation.to_array())),
        rotation: Quat::from_mat3(&rb).normalize(),
        scale: Vec3::ONE,
    }
}

/// Bevy rotation for a decoded `RotOrigin` rotator (source-space yaw/pitch/roll). The XIII
/// characters are authored +Y-forward; XIIIM/MigA store `RotOrigin` yaw 49152 (270 degrees),
/// which turns that authored forward onto source +X, the policy's forward.
pub(crate) fn bevy_rot_origin(rot: [i32; 3]) -> Quat {
    let m = rotator_to_bevy_matrix(rot);
    Quat::from_mat3(&Mat3::from_cols_array(&xiii_world::to_cols(&m))).normalize()
}

/// Bevy local transforms (one per bone, parent-relative) from source **global** pose transforms.
pub(crate) fn bevy_locals(skeleton: &Skeleton, pose_globals: &[SkTransform]) -> Vec<Transform> {
    skeleton
        .bones
        .iter()
        .enumerate()
        .map(|(i, b)| {
            let local = match b.parent {
                Some(p) => pose_globals[p].inverse().mul(&pose_globals[i]),
                None => pose_globals[i],
            };
            source_to_bevy_transform(&local)
        })
        .collect()
}

/// Inverse bind matrices in Bevy space (mesh space), one per bone.
pub(crate) fn bevy_inverse_bindposes(skeleton: &Skeleton) -> Vec<Mat4> {
    skeleton
        .bind_globals()
        .iter()
        .map(|b| source_to_bevy_transform(b).to_matrix().inverse())
        .collect()
}

/// Mirrors the GPU skinning chain on the CPU: Bevy local transforms -> globals (entity
/// hierarchy) -> `global * inverse_bindpose` -> linear-blend skinning. Returned positions are
/// world-space metres. Used by the tests as the "GPU expected" reference.
#[cfg(test)]
pub(crate) fn skinned_positions_gpu(
    decoded: &DecodedMesh,
    skeleton: &Skeleton,
    locals: &[Transform],
    inverse_bindposes: &[Mat4],
) -> Vec<Vec3> {
    let mut globals = vec![Mat4::IDENTITY; locals.len()];
    for (i, b) in skeleton.bones.iter().enumerate() {
        let m = locals.get(i).copied().unwrap_or_default().to_matrix();
        globals[i] = match b.parent {
            Some(p) => globals[p] * m,
            None => m,
        };
    }
    let joint: Vec<Mat4> = globals
        .iter()
        .zip(inverse_bindposes)
        .map(|(g, ib)| *g * *ib)
        .collect();
    decoded
        .positions
        .iter()
        .zip(decoded.joints.iter().zip(&decoded.weights))
        .map(|(p, (j, w))| {
            let local = Vec3::from_array(to_bevy_position(p.to_array())).extend(1.0);
            let mut acc = Vec3::ZERO;
            for k in 0..xiii_decode::skeletal::normalize::MAX_INFLUENCES {
                if w[k] != 0.0 {
                    let m = joint
                        .get(usize::from(j[k]))
                        .copied()
                        .unwrap_or(Mat4::IDENTITY);
                    acc += (m * local).truncate() * w[k];
                }
            }
            acc
        })
        .collect()
}

/// Largest per-vertex distance between two position sets, in Unreal units.
#[cfg(test)]
pub(crate) fn max_error_uu(a: &[Vec3], b: &[Vec3]) -> f32 {
    a.iter()
        .zip(b)
        .map(|(x, y)| (*x - *y).length() * UNREAL_UNITS_PER_METER)
        .fold(0.0f32, f32::max)
}

// ---------------------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------------------

/// One decoded character before it is uploaded.
pub(crate) struct LoadedModel {
    label: String,
    loaded: Arc<Loaded>,
    skeleton: Skeleton,
    decoded: DecodedMesh,
    anims: Option<AnimSet>,
    anim_label: Option<String>,
}

fn find_export(p: &xiii_package::Package, name: &str, class: &str) -> Option<usize> {
    (0..p.exports().len()).find(|&i| {
        p.export_class_path(i) == Some(class)
            && p.object_name(ObjectRef::Export(i as u32))
                .is_some_and(|n| n.eq_ignore_ascii_case(name))
    })
}

/// Loads `Package.Mesh` (a `SkeletalMesh`) and its referenced `MeshAnimation`.
pub(crate) fn load_model(cache: &mut PackageCache, spec: &str) -> Result<LoadedModel, String> {
    let (pkg_name, mesh_name) = spec
        .split_once('.')
        .ok_or_else(|| format!("--model {spec:?} must be Package.Mesh"))?;
    let loaded = cache.get(pkg_name)?;
    let mi = find_export(&loaded.package, mesh_name, SKELETAL_MESH_CLASS)
        .ok_or_else(|| format!("no SkeletalMesh {mesh_name} in {}", loaded.name))?;
    let raw = decode_skeletal_mesh(&loaded.package, &loaded.data, mi)
        .map_err(|e| format!("{mesh_name}: {e}"))?;
    let skeleton = Skeleton::from_mesh(&raw).map_err(|e| format!("{mesh_name}: skeleton: {e}"))?;
    let decoded = DecodedMesh::from_raw(&raw, &skeleton, Some(&loaded.package))
        .map_err(|e| format!("{mesh_name}: skin: {e}"))?;
    let (anims, anim_label) = match raw.animation {
        ObjectRef::Null => (None, None),
        r => {
            let (apkg, ai) = cache.resolve(&loaded, r)?;
            if apkg.package.export_class_path(ai) != Some(MESH_ANIMATION_CLASS) {
                return Err(format!(
                    "{spec}: {} is not a MeshAnimation",
                    apkg.package
                        .object_path(ObjectRef::Export(ai as u32))
                        .unwrap_or("?")
                ));
            }
            let raw_anim = decode_mesh_animation(&apkg.package, &apkg.data, ai)
                .map_err(|e| format!("{}: {e}", apkg.name))?;
            let set = AnimSet::from_raw(&raw_anim).map_err(|e| format!("{}: {e}", apkg.name))?;
            let label = format!(
                "{} ({})",
                apkg.package
                    .object_name(ObjectRef::Export(ai as u32))
                    .unwrap_or("?"),
                apkg.name
            );
            (Some(set), Some(label))
        }
    };
    Ok(LoadedModel {
        label: format!("{}.{}", pkg_name, mesh_name),
        loaded,
        skeleton,
        decoded,
        anims,
        anim_label,
    })
}

/// Index of a clip by case-insensitive name, with a helpful error listing some names.
fn clip_index(set: &AnimSet, name: &str) -> Result<usize, String> {
    set.clips
        .iter()
        .position(|c| c.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| {
            let sample: Vec<&str> = set.clips.iter().take(24).map(|c| c.name.as_str()).collect();
            format!(
                "no sequence '{name}' ({} clips, e.g. {})",
                set.clips.len(),
                sample.join(", ")
            )
        })
}

// ---------------------------------------------------------------------------------------
// Materials / textures
// ---------------------------------------------------------------------------------------

/// Material classes whose texture is reached through an object property (first present wins).
/// Read-only duplicate of the diagnostic chain in `xiii-world::import_map` (the skinned viewer
/// cannot use that private importer); direct `Engine.Texture` references are the common case.
const MATERIAL_LINKS: &[(&str, &[&str])] = &[
    ("Shader", &["Diffuse", "SelfIllumination", "Specular"]),
    ("FinalBlend", &["Material"]),
    ("TexPanner", &["Material"]),
    ("TexRotator", &["Material"]),
    ("TexScaler", &["Material"]),
    ("TexOscillator", &["Material"]),
    ("TexEnvMap", &["Material"]),
    ("TexCoordSource", &["Material"]),
    ("TexModifier", &["Material"]),
    ("SinusModifier", &["Material"]),
    ("Combiner", &["Material1", "Material2"]),
    ("ColorModifier", &["Material"]),
];

/// `(image, has_alpha)` or the failure reason, keyed by lower-case package name + export.
type TextureEntry = Result<(Handle<Image>, bool), String>;

struct TextureResolver {
    cache: HashMap<(String, usize), TextureEntry>,
}

impl TextureResolver {
    fn resolve(
        &mut self,
        cache: &mut PackageCache,
        images: &mut Assets<Image>,
        from: &Arc<Loaded>,
        r: ObjectRef,
        depth: u32,
    ) -> Result<(Handle<Image>, bool), String> {
        if depth > 8 {
            return Err("material chain too deep".into());
        }
        let (pkg, idx) = cache.resolve(from, r)?;
        let key = (pkg.name.to_ascii_lowercase(), idx);
        if let Some(v) = self.cache.get(&key) {
            return v.clone();
        }
        let class = pkg.package.export_class_path(idx).unwrap_or("?").to_owned();
        let short = class.rsplit('.').next().unwrap_or("");
        let result = if short.eq_ignore_ascii_case("Texture") {
            decode_texture(&pkg.package, &pkg.data, idx)
                .map_err(|e| format!("texture decode: {e}"))
                .and_then(|t| {
                    let img = t
                        .decode_mip(0)
                        .map_err(|e| format!("texture pixels: {e}"))?;
                    let has_alpha = img.pixels.as_chunks::<4>().0.iter().any(|p| p[3] < 250);
                    Ok((images.add(image_from_rgba(&img)), has_alpha))
                })
        } else if let Some((_, names)) = MATERIAL_LINKS
            .iter()
            .find(|(c, _)| c.eq_ignore_ascii_case(short))
        {
            let next = pkg
                .package
                .read_object_properties(&pkg.data, idx, &Limits::default())
                .ok()
                .and_then(|props| {
                    names.iter().find_map(|name| {
                        props.block.properties.iter().find_map(|p| {
                            match (
                                pkg.package.property_name(p).eq_ignore_ascii_case(name),
                                &p.value,
                            ) {
                                (true, PropertyValue::Object(o)) if !o.is_null() => Some(*o),
                                _ => None,
                            }
                        })
                    })
                });
            match next {
                Some(o) => self.resolve(cache, images, &pkg, o, depth + 1),
                None => Err(format!("{class} without {}", names.join("/"))),
            }
        } else {
            Err(format!("unsupported material class {class}"))
        };
        self.cache.insert(key, result.clone());
        result
    }
}

fn image_from_rgba(t: &RgbaImage) -> Image {
    let mut img = Image::new(
        Extent3d {
            width: t.width,
            height: t.height,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        t.pixels.clone(),
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::RENDER_WORLD,
    );
    img.sampler = ImageSampler::Descriptor(ImageSamplerDescriptor {
        address_mode_u: ImageAddressMode::Repeat,
        address_mode_v: ImageAddressMode::Repeat,
        ..ImageSamplerDescriptor::linear()
    });
    img
}

/// One unlit `StandardMaterial` per decoded material slot.
#[allow(clippy::too_many_arguments)]
fn material_handles(
    cache: &mut PackageCache,
    from: &Arc<Loaded>,
    decoded: &DecodedMesh,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    resolver: &mut TextureResolver,
    counters: &mut BTreeMap<String, usize>,
) -> Vec<Handle<StandardMaterial>> {
    let magenta = materials.add(StandardMaterial {
        base_color: Color::srgb(1.0, 0.0, 1.0),
        unlit: true,
        ..default()
    });
    decoded
        .materials
        .iter()
        .map(|m| {
            let Some(r) = m.texture else {
                *counters.entry("material.none".into()).or_default() += 1;
                return magenta.clone();
            };
            match resolver.resolve(cache, images, from, r, 0) {
                Ok((handle, has_alpha)) => {
                    *counters.entry("material.decoded".into()).or_default() += 1;
                    materials.add(StandardMaterial {
                        base_color_texture: Some(handle),
                        unlit: true,
                        alpha_mode: if has_alpha {
                            AlphaMode::Mask(0.5)
                        } else {
                            AlphaMode::Opaque
                        },
                        ..default()
                    })
                }
                Err(e) => {
                    let label = m.texture_path.clone().unwrap_or_else(|| format!("{r:?}"));
                    *counters
                        .entry(format!(
                            "material.failed ({})",
                            e.split(':').next().unwrap_or("")
                        ))
                        .or_default() += 1;
                    counters
                        .entry("material.failed_example".into())
                        .or_insert(0);
                    println!("[skinned] material {label}: {e}");
                    magenta.clone()
                }
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------------------
// Mesh upload
// ---------------------------------------------------------------------------------------

fn build_mesh(decoded: &DecodedMesh, indices: Vec<u32>) -> Mesh {
    let mut mesh = Mesh::new(
        PrimitiveTopology::TriangleList,
        RenderAssetUsages::RENDER_WORLD,
    );
    let positions: Vec<[f32; 3]> = decoded
        .positions
        .iter()
        .map(|p| to_bevy_position([p.x, p.y, p.z]))
        .collect();
    let normals: Vec<[f32; 3]> = decoded
        .normals
        .iter()
        .map(|n| to_bevy_direction([n.x, n.y, n.z]))
        .collect();
    mesh.insert_attribute(Mesh::ATTRIBUTE_POSITION, positions);
    mesh.insert_attribute(Mesh::ATTRIBUTE_NORMAL, normals);
    mesh.insert_attribute(Mesh::ATTRIBUTE_UV_0, decoded.uvs.clone());
    mesh.insert_attribute(
        Mesh::ATTRIBUTE_JOINT_INDEX,
        VertexAttributeValues::Uint16x4(decoded.joints.clone()),
    );
    mesh.insert_attribute(
        Mesh::ATTRIBUTE_JOINT_WEIGHT,
        VertexAttributeValues::Float32x4(decoded.weights.clone()),
    );
    mesh.insert_indices(Indices::U32(indices));
    mesh
}

// ---------------------------------------------------------------------------------------
// Setup
// ---------------------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
fn setup(
    mut commands: Commands,
    cfg: Res<SkinnedConfig>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut bindposes: ResMut<Assets<SkinnedMeshInverseBindposes>>,
    mut exit: MessageWriter<AppExit>,
) {
    let Some(game_dir) = cfg.options.game_dir.clone() else {
        eprintln!("error: --model needs --game-dir");
        exit.write(AppExit::error());
        return;
    };
    let Some(spec) = cfg.options.model.clone() else {
        eprintln!("error: --model is required in skinned mode");
        exit.write(AppExit::error());
        return;
    };
    let specs: Vec<String> = spec
        .split(',')
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
        .collect();
    if specs.is_empty() {
        eprintln!("error: --model is empty");
        exit.write(AppExit::error());
        return;
    }

    let mut cache = match PackageCache::open(&game_dir) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[skinned] cannot open {}: {e}", game_dir.display());
            exit.write(AppExit::error());
            return;
        }
    };
    let started = Instant::now();
    let mut models = Vec::new();
    for s in &specs {
        match load_model(&mut cache, s) {
            Ok(m) => models.push(m),
            Err(e) => {
                eprintln!("[skinned] load failed: {e}");
                exit.write(AppExit::error());
                return;
            }
        }
    }

    let mut resolver = TextureResolver {
        cache: HashMap::new(),
    };
    let mut material_counters: BTreeMap<String, usize> = BTreeMap::new();
    let spacing = 2.2f32;
    let mut characters = Vec::new();
    let mut max_height = 0.0f32;
    let mut center = Vec3::ZERO;

    for (ci, m) in models.iter().enumerate() {
        let clip = match cfg.options.anim.as_deref() {
            Some(name) => {
                let Some(set) = m.anims.as_ref() else {
                    eprintln!(
                        "[skinned] {} has no MeshAnimation; --anim {name} not possible",
                        m.label
                    );
                    exit.write(AppExit::error());
                    return;
                };
                match clip_index(set, name) {
                    Ok(i) => Some(i),
                    Err(e) => {
                        eprintln!("[skinned] {}: {e}", m.label);
                        exit.write(AppExit::error());
                        return;
                    }
                }
            }
            None => None,
        };
        let bone_map = m
            .anims
            .as_ref()
            .map(|s| s.bone_map(&m.skeleton))
            .unwrap_or_default();
        let offset = (ci as f32 - (models.len() as f32 - 1.0) * 0.5) * spacing;
        // Mesh orientation: the decoded RotOrigin turns the authored mesh forward onto the
        // policy's forward (source +X). Applied at the skeleton root only.
        let root = commands
            .spawn((
                Transform {
                    translation: Vec3::new(offset, 0.0, 0.0),
                    rotation: bevy_rot_origin(m.decoded.rot_origin),
                    scale: Vec3::ONE,
                },
                Visibility::default(),
                Name::new(m.label.clone()),
            ))
            .id();
        let mut joints = Vec::with_capacity(m.skeleton.bones.len());
        for (bi, bone) in m.skeleton.bones.iter().enumerate() {
            let parent = bone.parent.map_or(root, |p| joints[p]);
            let e = commands
                .spawn((
                    Transform::default(),
                    Visibility::default(),
                    ChildOf(parent),
                    BoneJoint {
                        character: ci,
                        bone: bi,
                    },
                    Name::new(bone.name.clone()),
                ))
                .id();
            joints.push(e);
        }

        let ibp = bindposes.add(SkinnedMeshInverseBindposes::from(bevy_inverse_bindposes(
            &m.skeleton,
        )));
        let mats = material_handles(
            &mut cache,
            &m.loaded,
            &m.decoded,
            &mut materials,
            &mut images,
            &mut resolver,
            &mut material_counters,
        );
        for (si, section) in m.decoded.sections.iter().enumerate() {
            let start = section.first_index as usize;
            let end = start + section.index_count as usize;
            let indices = m
                .decoded
                .indices
                .get(start..end)
                .unwrap_or_default()
                .to_vec();
            commands.spawn((
                Mesh3d(meshes.add(build_mesh(&m.decoded, indices))),
                MeshMaterial3d(
                    mats.get(usize::from(section.material))
                        .cloned()
                        .unwrap_or_else(|| {
                            materials.add(StandardMaterial {
                                base_color: Color::srgb(1.0, 0.0, 1.0),
                                unlit: true,
                                ..default()
                            })
                        }),
                ),
                SkinnedMesh {
                    inverse_bindposes: ibp.clone(),
                    joints: joints.clone(),
                },
                Transform::default(),
                Visibility::default(),
                NoFrustumCulling,
                ChildOf(root),
                Name::new(format!("{}#{}", m.label, si)),
            ));
        }
        print_model_diagnostics(m, clip);
        let h = diagnostics_height(m);
        max_height = max_height.max(h);
        center += Vec3::new(offset, 0.0, 0.0);
        characters.push(CharacterInfo {
            label: m.label.clone(),
            skeleton: m.skeleton.clone(),
            anims: m.anims.clone(),
            anim_label: m.anim_label.clone(),
            clip,
            bone_map,
        });
    }
    center /= models.len() as f32;
    let radius = (max_height * 1.6).max(3.0);
    commands.insert_resource(SkinnedScene {
        characters,
        center,
        radius,
        height: max_height,
    });

    // Turntable camera (frozen front-on when --frame is set).
    let yaw = std::f32::consts::PI;
    let target = Vec3::new(center.x, max_height * 0.5, center.z);
    let eye = target + Vec3::new(yaw.sin() * radius, radius * 0.35, yaw.cos() * radius);
    commands.spawn((
        Camera3d::default(),
        Transform::from_translation(eye).looking_at(target, Vec3::Y),
        TurntableCam,
    ));
    print_material_counters(&material_counters);
    println!(
        "[skinned] loaded {} model(s) in {:.2}s; {} character(s), height {:.1} UU ({:.2} m), camera radius {:.2} m",
        models.len(),
        started.elapsed().as_secs_f32(),
        models.len(),
        max_height * UNREAL_UNITS_PER_METER,
        max_height,
        radius
    );

    commands.spawn((
        OverlayText,
        Text::new("loading"),
        TextFont {
            font_size: FontSize::Px(13.0),
            ..default()
        },
        TextColor(Color::srgb(0.95, 0.95, 0.85)),
        Node {
            position_type: PositionType::Absolute,
            top: px(6),
            left: px(6),
            padding: UiRect::all(px(5)),
            ..default()
        },
        BackgroundColor(Color::srgba(0.0, 0.0, 0.0, 0.55)),
    ));
}

/// Bind-pose height in metres (converted positions).
fn diagnostics_height(m: &LoadedModel) -> f32 {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for p in &m.decoded.positions {
        let y = to_bevy_position([p.x, p.y, p.z])[1];
        lo = lo.min(y);
        hi = hi.max(y);
    }
    hi - lo
}

/// Prints measured per-model diagnostics (bones, weights, bounds at bind and at frame 0).
fn print_model_diagnostics(m: &LoadedModel, clip: Option<usize>) {
    let st = m.decoded.influence_stats;
    println!(
        "[skinned] {}: bones {} roots {} verts {} tris {} sections {} materials {} max_influences {} dropped {} truncated {} unweighted {} weight_sum {:.4}..{:.4}",
        m.label,
        m.skeleton.bones.len(),
        m.skeleton.root_count(),
        m.decoded.positions.len(),
        m.decoded.indices.len() / 3,
        m.decoded.sections.len(),
        m.decoded.materials.len(),
        st.max_influences,
        st.invalid_bone_influences,
        st.points_truncated,
        st.points_without_influences,
        st.raw_sum_min,
        st.raw_sum_max,
    );
    println!(
        "[skinned]   mesh transform: scale ({:.3}, {:.3}, {:.3}) origin ({:.1}, {:.1}, {:.1}) rot_origin {:?} (applied as root orientation)",
        m.decoded.mesh_scale.x,
        m.decoded.mesh_scale.y,
        m.decoded.mesh_scale.z,
        m.decoded.mesh_origin.x,
        m.decoded.mesh_origin.y,
        m.decoded.mesh_origin.z,
        m.decoded.rot_origin,
    );
    let (lo, hi) = y_bounds(m.decoded.positions.iter().map(|p| [p.x, p.y, p.z]));
    println!(
        "[skinned]   bind: feet {:.1} UU, top {:.1} UU, height {:.1} UU ({:.3} m)",
        lo * UNREAL_UNITS_PER_METER,
        hi * UNREAL_UNITS_PER_METER,
        (hi - lo) * UNREAL_UNITS_PER_METER,
        hi - lo
    );
    if let (Some(i), Some(set)) = (clip, m.anims.as_ref()) {
        let map = set.bone_map(&m.skeleton);
        let pose = evaluate_pose(&m.skeleton, &set.clips[i], &map, 0.0, true);
        let pts: Vec<[f32; 3]> = m
            .decoded
            .skin(&m.skeleton, &pose)
            .iter()
            .map(|p| [p.x, p.y, p.z])
            .collect();
        let (lo, hi) = y_bounds(pts.into_iter());
        println!(
            "[skinned]   {} frame 0: feet {:.1} UU, top {:.1} UU, height {:.1} UU",
            set.clips[i].name,
            lo * UNREAL_UNITS_PER_METER,
            hi * UNREAL_UNITS_PER_METER,
            (hi - lo) * UNREAL_UNITS_PER_METER
        );
    }
}

/// (min, max) Bevy Y over source-space points, converted with the coordinate policy.
fn y_bounds(points: impl Iterator<Item = [f32; 3]>) -> (f32, f32) {
    let (mut lo, mut hi) = (f32::MAX, f32::MIN);
    for p in points {
        let y = to_bevy_position(p)[1];
        lo = lo.min(y);
        hi = hi.max(y);
    }
    if lo > hi { (0.0, 0.0) } else { (lo, hi) }
}

fn print_material_counters(counters: &BTreeMap<String, usize>) {
    for (k, v) in counters {
        if k.ends_with("_example") {
            continue;
        }
        println!("[skinned] {v:>6} {k}");
    }
}

// ---------------------------------------------------------------------------------------
// Animation
// ---------------------------------------------------------------------------------------

fn pose_globals(c: &CharacterInfo, frame: f32) -> Vec<SkTransform> {
    match (c.anims.as_ref(), c.clip) {
        (Some(set), Some(i)) => evaluate_pose(&c.skeleton, &set.clips[i], &c.bone_map, frame, true),
        _ => c.skeleton.bind_globals(),
    }
}

fn animate_bones(
    time: Res<Time>,
    cfg: Res<SkinnedConfig>,
    scene: Res<SkinnedScene>,
    mut clock: ResMut<AnimClock>,
    mut q: Query<(&BoneJoint, &mut Transform)>,
) {
    let mut first = true;
    for (ci, c) in scene.characters.iter().enumerate() {
        let (frame, seconds, name) = match (c.anims.as_ref(), c.clip) {
            (Some(set), Some(i)) => {
                let clip: &Clip = &set.clips[i];
                let frozen = cfg.options.frame;
                let f = frozen.unwrap_or_else(|| {
                    (time.elapsed_secs() * clip.rate).rem_euclid(clip.track_time.max(1.0))
                });
                (f, f / clip.rate.max(1e-6), clip.name.clone())
            }
            _ => (0.0, 0.0, "bind".to_owned()),
        };
        if first {
            clock.frame = frame;
            clock.time = seconds;
            clock.clip = name;
            first = false;
        }
        let locals = bevy_locals(&c.skeleton, &pose_globals(c, frame));
        for (joint, mut t) in &mut q {
            if joint.character == ci
                && let Some(local) = locals.get(joint.bone)
            {
                *t = *local;
            }
        }
    }
}

/// 1 m floor grid centred on the character row plus a forward (-Z) and right (+X) axis line.
fn draw_helpers(mut gizmos: Gizmos, scene: Res<SkinnedScene>) {
    let c = scene.center;
    gizmos.grid(
        Isometry3d::new(
            Vec3::new(c.x, 0.002, c.z),
            Quat::from_rotation_x(-std::f32::consts::FRAC_PI_2),
        ),
        UVec2::splat(24),
        Vec2::ONE,
        Color::srgba(1.0, 1.0, 1.0, 0.10),
    );
    gizmos.line(c, c + Vec3::NEG_Z * 1.5, Color::srgb(1.0, 0.2, 0.2));
    gizmos.line(c, c + Vec3::X * 1.0, Color::srgb(0.2, 1.0, 0.2));
    gizmos.line(c, c + Vec3::Y * 1.0, Color::srgb(0.3, 0.4, 1.0));
}

fn turntable(
    time: Res<Time>,
    cfg: Res<SkinnedConfig>,
    scene: Res<SkinnedScene>,
    mut q: Query<&mut Transform, With<TurntableCam>>,
) {
    let Ok(mut t) = q.single_mut() else {
        return;
    };
    let yaw = if cfg.options.frame.is_some() {
        std::f32::consts::PI
    } else {
        std::f32::consts::PI + time.elapsed_secs() * 0.3
    };
    let target = Vec3::new(scene.center.x, scene.height * 0.5, scene.center.z);
    let eye = target
        + Vec3::new(
            yaw.sin() * scene.radius,
            scene.radius * 0.35,
            yaw.cos() * scene.radius,
        );
    *t = Transform::from_translation(eye).looking_at(target, Vec3::Y);
}

fn overlay(
    clock: Res<AnimClock>,
    scene: Res<SkinnedScene>,
    mut text: Query<&mut Text, With<OverlayText>>,
) {
    let Ok(mut text) = text.single_mut() else {
        return;
    };
    let mut s = String::from("XIII skinned character viewer (diagnostic)\n");
    for c in &scene.characters {
        let clip = match (c.anims.as_ref(), c.clip) {
            (Some(set), Some(i)) => {
                let cl = &set.clips[i];
                format!(
                    "{} [{}] frames {} @ {:.0} fps (track {:.0})",
                    cl.name,
                    c.anim_label.as_deref().unwrap_or("?"),
                    cl.num_frames,
                    cl.rate,
                    cl.track_time
                )
            }
            _ => "bind pose".into(),
        };
        s.push_str(&format!("{}: {clip}\n", c.label));
    }
    s.push_str(&format!(
        "clip {} frame {:.2} / time {:.3}s\n",
        clock.clip, clock.frame, clock.time
    ));
    s.push_str("GPU LBS from the decoded sampler; unlit diagnostic materials; red = forward (-Z source +X).\n");
    text.0 = s;
}

// ---------------------------------------------------------------------------------------
// Unattended run / screenshot
// ---------------------------------------------------------------------------------------

fn unattended(
    mut commands: Commands,
    cfg: Res<SkinnedConfig>,
    mut state: ResMut<RunState>,
    flag: Res<ShotFlag>,
    mut exit: MessageWriter<AppExit>,
) {
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
    state.frame += 1;
    if elapsed < secs {
        return;
    }
    let reached = *state.target_at.get_or_insert_with(Instant::now);
    if state.shot == 1 && !state.shot_done && reached.elapsed() < Duration::from_secs(5) {
        return;
    }
    println!(
        "[skinned] exit after {:.1}s, {} frames, screenshot {}",
        elapsed,
        state.frame,
        match (&cfg.options.screenshot, state.shot_done) {
            (Some(p), true) => format!("saved {}", p.display()),
            (Some(p), false) => format!("NOT confirmed {}", p.display()),
            (None, _) => "none".into(),
        }
    );
    exit.write(AppExit::Success);
}

// ---------------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_decode::skeletal::math::{Quat as Q, Transform as T, Vec3 as V};

    fn bone(
        name: &str,
        parent: Option<usize>,
        rot: Q,
        pos: V,
    ) -> xiii_decode::skeletal::normalize::Bone {
        // A bind-local transform; bind_global for a root equals the local.
        let local = T::new(rot, pos);
        xiii_decode::skeletal::normalize::Bone {
            name: name.to_owned(),
            parent,
            flags: 0,
            bind_local: local,
            bind_global: local,
        }
    }

    fn two_bone_skeleton() -> Skeleton {
        // Bone 0 at the origin, bone 1 one metre up (in UU: 90) under bone 0.
        Skeleton {
            bones: vec![
                bone("root", None, Q::IDENTITY, V::ZERO),
                bone("child", Some(0), Q::IDENTITY, V::new(0.0, 0.0, 90.0)),
            ],
        }
    }

    fn quad_mesh() -> DecodedMesh {
        // Two vertices rigidly bound to bone 1, one to bone 0.
        DecodedMesh {
            positions: vec![V::new(0.0, 0.0, 0.0), V::new(0.0, 0.0, 90.0)],
            normals: vec![V::ZERO, V::ZERO],
            uvs: vec![[0.0, 0.0], [1.0, 1.0]],
            point_of_vertex: vec![0, 1],
            joints: vec![[0, 0, 0, 0], [1, 0, 0, 0]],
            weights: vec![[1.0, 0.0, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]],
            indices: vec![0, 1, 0],
            sections: vec![],
            materials: vec![],
            mesh_scale: V::new(1.0, 1.0, 1.0),
            mesh_origin: V::ZERO,
            rot_origin: [0, 0, 0],
            influence_stats: Default::default(),
        }
    }

    #[test]
    fn coordinate_policy_maps_axes_once() {
        // Identity stays identity; a source +X translation becomes Bevy -Z (1 m = 90 UU).
        let id = source_to_bevy_transform(&T::IDENTITY);
        assert!((id.translation - Vec3::ZERO).length() < 1e-6);
        assert!(id.rotation.is_near_identity());
        let p = source_to_bevy_transform(&T::new(Q::IDENTITY, V::new(90.0, 0.0, 0.0)));
        assert!(
            (p.translation - Vec3::NEG_Z).length() < 1e-5,
            "{:?}",
            p.translation
        );
        // Source yaw 90 deg (about +Z) must be a -90 deg Bevy yaw (about +Y), det +1.
        let yaw = xiii_decode::skeletal::math::Quat::from_array([
            0.0,
            0.0,
            (std::f32::consts::FRAC_PI_4).sin(),
            (std::f32::consts::FRAC_PI_4).cos(),
        ]);
        let b = source_to_bevy_transform(&T::new(yaw, V::ZERO));
        assert!((b.rotation.length() - 1.0).abs() < 1e-5);
        // Source +X (Bevy -Z) turns to source +Y (Bevy +X): a clockwise yaw seen from above,
        // matching common::rotator_to_bevy_matrix.
        let fwd = b.rotation * Vec3::NEG_Z;
        assert!((fwd.x - 1.0).abs() < 1e-4 && fwd.z.abs() < 1e-4, "{fwd:?}");
    }

    #[test]
    fn gpu_mirror_matches_cpu_skin_including_float_error() {
        let sk = two_bone_skeleton();
        let mesh = quad_mesh();
        let inv = bevy_inverse_bindposes(&sk);
        // Bind pose: locals from the bind globals.
        let bind = bevy_locals(&sk, &sk.bind_globals());
        let gpu = skinned_positions_gpu(&mesh, &sk, &bind, &inv);
        let cpu: Vec<Vec3> = mesh
            .positions
            .iter()
            .map(|p| Vec3::from_array(to_bevy_position(p.to_array())))
            .collect();
        assert!(
            max_error_uu(&cpu, &gpu) < 1e-3,
            "{}",
            max_error_uu(&cpu, &gpu)
        );

        // A parent rotation about source +X: the child (and its vertex) must move. Globals are
        // composed through the hierarchy exactly as `evaluate_pose` returns them.
        let parent = T::new(Q::from_axis_angle(V::new(1.0, 0.0, 0.0), 0.5), V::ZERO);
        let pose = vec![
            parent,
            parent.mul(&T::new(Q::IDENTITY, V::new(0.0, 0.0, 90.0))),
        ];
        let locals = bevy_locals(&sk, &pose);
        let gpu = skinned_positions_gpu(&mesh, &sk, &locals, &inv);
        let cpu: Vec<Vec3> = mesh
            .skin(&sk, &pose)
            .iter()
            .map(|p| Vec3::from_array(to_bevy_position(p.to_array())))
            .collect();
        let err = max_error_uu(&cpu, &gpu);
        assert!(err < 1e-3, "max error {err} UU");
        // The top vertex really moved (the test is not vacuous).
        assert!(
            (gpu[1] - Vec3::new(0.0, 1.0, 0.0)).length() > 1e-3,
            "{:?}",
            gpu[1]
        );
    }

    #[test]
    fn gpu_mirror_handles_truncated_and_invalid_influences() {
        // 5 influences would be impossible in the corpus (max 3); the mirror only reads the
        // 4 slots that exist and ignores zero-weight slots.
        let sk = two_bone_skeleton();
        let mut mesh = quad_mesh();
        mesh.joints = vec![[0, 1, 0, 0], [1, 0, 0, 0]];
        mesh.weights = vec![[0.25, 0.75, 0.0, 0.0], [1.0, 0.0, 0.0, 0.0]];
        let inv = bevy_inverse_bindposes(&sk);
        let pose = vec![
            T::new(Q::IDENTITY, V::new(0.0, 0.0, 0.0)),
            T::new(Q::IDENTITY, V::new(0.0, 0.0, 180.0)),
        ];
        let locals = bevy_locals(&sk, &pose);
        let gpu = skinned_positions_gpu(&mesh, &sk, &locals, &inv);
        let cpu: Vec<Vec3> = mesh
            .skin(&sk, &pose)
            .iter()
            .map(|p| Vec3::from_array(to_bevy_position(p.to_array())))
            .collect();
        assert!(
            max_error_uu(&cpu, &gpu) < 1e-3,
            "{}",
            max_error_uu(&cpu, &gpu)
        );
    }

    #[test]
    fn inverse_bind_is_identity_at_bind() {
        let sk = two_bone_skeleton();
        let inv = bevy_inverse_bindposes(&sk);
        let globals: Vec<Mat4> = sk
            .bind_globals()
            .iter()
            .map(|b| source_to_bevy_transform(b).to_matrix())
            .collect();
        for (g, ib) in globals.iter().zip(&inv) {
            let id = *g * *ib;
            let want = Mat4::IDENTITY;
            for c in 0..4 {
                for r in 0..4 {
                    assert!((id.col(c)[r] - want.col(c)[r]).abs() < 1e-4);
                }
            }
        }
    }

    /// Opt-in: XIIIM Walk/WaitNeutre CPU vs GPU-path positions (scale-free, both in UU).
    #[test]
    fn gog_xiiim_walk_idle_gpu_path_matches_cpu() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to run the skinned test");
            return;
        };
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        let model = load_model(&mut cache, "xiiipersos.XIIIM").expect("load XIIIM");
        let set = model.anims.as_ref().expect("XIIIM has MigA");
        let map = set.bone_map(&model.skeleton);
        let inv = bevy_inverse_bindposes(&model.skeleton);
        // Real sequence names from `xiii-tool anim list`: Walk (placeholder 31) and
        // WaitNeutre (idle, 61). Names are looked up, not assumed.
        for name in ["Walk", "WaitNeutre"] {
            let clip = set.clip(name).unwrap_or_else(|| panic!("clip {name}"));
            let last = (clip.num_frames.saturating_sub(1)) as f32;
            for frac in [0.0, 0.25, 0.5] {
                let frame = format!("{:.1}", last * frac).parse::<f32>().unwrap();
                let pose = evaluate_pose(&model.skeleton, clip, &map, frame, true);
                let cpu: Vec<Vec3> = model
                    .decoded
                    .skin(&model.skeleton, &pose)
                    .iter()
                    .map(|p| Vec3::from_array(to_bevy_position(p.to_array())))
                    .collect();
                let locals = bevy_locals(&model.skeleton, &pose);
                let gpu = skinned_positions_gpu(&model.decoded, &model.skeleton, &locals, &inv);
                let err = max_error_uu(&cpu, &gpu);
                assert!(
                    err < 0.01,
                    "{name} frame {frame}: max error {err} UU ({} verts)",
                    cpu.len()
                );
                println!("{name} frame {frame}: max error {err:.3e} UU");
            }
        }
    }

    /// Opt-in requirement 4: idle frame 0 feet on the floor, height ~160 UU, facing -Z.
    #[test]
    fn gog_xiiim_idle_frame0_checks() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to run the skinned test");
            return;
        };
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        let model = load_model(&mut cache, "xiiipersos.XIIIM").expect("load XIIIM");
        let set = model.anims.as_ref().expect("XIIIM has MigA");
        let map = set.bone_map(&model.skeleton);
        let clip = set.clip("WaitNeutre").expect("WaitNeutre");
        let pose = evaluate_pose(&model.skeleton, clip, &map, 0.0, true);
        let skinned: Vec<xiii_decode::skeletal::math::Vec3> =
            model.decoded.skin(&model.skeleton, &pose);
        let (mut lo, mut hi) = (f32::MAX, f32::MIN);
        for p in &skinned {
            let y = to_bevy_position(p.to_array())[1];
            lo = lo.min(y);
            hi = hi.max(y);
        }
        let feet_uu = lo * UNREAL_UNITS_PER_METER;
        let height_uu = (hi - lo) * UNREAL_UNITS_PER_METER;
        println!("idle frame 0: feet {feet_uu:.1} UU, height {height_uu:.1} UU");
        assert!(feet_uu.abs() < 2.0, "feet not on the floor: {feet_uu} UU");
        assert!((150.0..170.0).contains(&height_uu), "height {height_uu} UU");
        // Facing: the authored-forward (foot -> toe) direction is +Y in source. Without the
        // decoded RotOrigin that becomes Bevy +X; with RotOrigin (yaw 49152) it becomes the
        // policy forward, Bevy -Z.
        let foot = model.skeleton.find("X L Foot").expect("X L Foot");
        let toe = model.skeleton.find("X L Toe0").expect("X L Toe0");
        let a = model.skeleton.bones[foot].bind_global.translation;
        let b = model.skeleton.bones[toe].bind_global.translation;
        let authored = Vec3::from_array(to_bevy_direction([b.x - a.x, b.y - a.y, 0.0])).normalize();
        assert!(
            authored.x > 0.5 && authored.z.abs() < 0.5,
            "authored forward is not source +Y: {authored:?}"
        );
        let rotated = bevy_rot_origin(model.decoded.rot_origin) * authored;
        assert!(
            rotated.z < -0.5 && rotated.x.abs() < 0.5,
            "RotOrigin did not turn the character onto the policy forward: {rotated:?}"
        );
    }

    /// Opt-in: the SlaterM 0xFFFF case. The decoder drops out-of-range influences and
    /// renormalizes; the viewer must report the count and every vertex weight must sum to 1.
    #[test]
    fn gog_slaterm_drops_invalid_influences() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to run the skinned test");
            return;
        };
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let mut cache = PackageCache::open(&path).expect("open install");
        let model = load_model(&mut cache, "xiiipersos.SlaterM").expect("load SlaterM");
        let dropped = model.decoded.influence_stats.invalid_bone_influences;
        println!("SlaterM dropped out-of-range influences: {dropped}");
        assert!(dropped > 0, "expected the SlaterM 0xFFFF influences");
        let n_bones = model.skeleton.bones.len();
        for (v, (j, w)) in model
            .decoded
            .joints
            .iter()
            .zip(&model.decoded.weights)
            .enumerate()
        {
            let sum: f32 = w.iter().sum();
            assert!((sum - 1.0).abs() < 1e-4, "vertex {v} weights sum {sum}");
            for k in 0..xiii_decode::skeletal::normalize::MAX_INFLUENCES {
                if w[k] > 0.0 {
                    assert!(
                        usize::from(j[k]) < n_bones,
                        "vertex {v} influence bone {} >= {n_bones}",
                        j[k]
                    );
                }
            }
        }
    }
}
