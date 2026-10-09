//! Map-pawn rendering for `--play`.
//!
//! Every **live** VM actor with an `Engine.SkeletalMesh` `Mesh` is drawn GPU-skinned at its VM
//! `Location`/`Rotation` and posed from the VM's own animation state (the channel's current
//! sequence, frame, rate and looping flag) with the shared CPU sampler
//! (`xiii_decode::skeletal::blend`, layered over the same track sampler the `--model` viewer and
//! `xiii-tool anim` use). The player pawn is not drawn (first person).
//!
//! **Hidden actors** follow the decoded `Engine.Actor` properties: `bHidden` is the decoded
//! visibility flag; `DrawType` is the UE2 `EDrawType` byte where `0 = DT_None` (nothing drawn)
//! and `2 = DT_Mesh` (the value in every decoded `BaseSoldier` class default). An actor whose
//! mesh is attached to a bone (`AttachBone` set by `Actor.AttachToBone`) follows the evaluated
//! parent pose and its RelativeLocation/RelativeRotation. Spawned attachments are added at runtime.
//!
//! Mesh-space placement: the decoded `RotOrigin` is applied at the root exactly as the `--model`
//! viewer does. In addition the decoded `MeshOrigin` (the offset from the mesh origin to the
//! actor origin) is applied as `T(-MeshOrigin)`, because a `Pawn`'s VM `Location` is the centre
//! of its collision cylinder, not the mesh origin; without it the mesh would float by
//! half a character height. This order (`T(Location) * R_actor * R_origin * S * T(-MeshOrigin)`)
//! is a **hypothesis** (see the report).

use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

use bevy::ecs::system::NonSend;
use bevy::mesh::skinning::SkinnedMeshInverseBindposes;
use bevy::prelude::*;

use xiii_decode::common::{to_bevy_position, to_bevy_scale};
use xiii_decode::skeletal::blend::{self, Channel, Controller, Pose, Sample};
use xiii_decode::skeletal::math::{Transform as SourceTransform, Vec3 as SourceVec};
use xiii_decode::skeletal::normalize::{
    AnimSet, Bone, Clip, MaterialSlot, MeshSection, Skeleton, SkinnedMesh as DecodedMesh,
};
use xiii_script::{ActorAnimation, AnimChannelState, ObjRef, ObjectId, Value, Vm};
use xiii_world::PackageCache;

use super::session::Session;
use crate::viewer::skinned::{
    LoadedModel, SkinnedAssets, TextureResolver, bevy_rot_origin, build_skinned_assets, load_model,
    source_to_bevy_transform, spawn_skinned_entities,
};

/// Renderable class of an actor's `Mesh` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MeshKind {
    /// No `Mesh` (or a null one).
    None,
    /// `Engine.SkeletalMesh` (the pawn renderer can draw it).
    Skeletal,
    /// A rigid static mesh on a bone attachment (world static actors have a separate renderer).
    StaticAttachment,
    /// Some other mesh class (e.g. `Engine.StaticMesh`; not a pawn).
    Other,
}

/// Pure selection input of one actor, so the rules can be unit-tested without a VM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ActorView {
    /// Derives from `Actor`.
    pub is_actor: bool,
    /// Destroyed.
    pub deleted: bool,
    /// This is the host-owned player pawn (not drawn, first person).
    pub is_player: bool,
    /// A UE2 class-default object (`Default__<Class>`, created by `Vm::default_object`), not a
    /// placed actor.
    pub is_class_default: bool,
    /// Decoded `bHidden`.
    pub hidden: bool,
    /// Decoded `DrawType` (`None` when absent); `0` is `DT_None`.
    pub draw_type: Option<u8>,
    /// Class of the actor's `Mesh`.
    pub mesh: MeshKind,
    /// `AttachBone` is set; placement follows the evaluated parent skeleton.
    pub attached_to_bone: bool,
}

/// Why an actor is not drawn (`None` = it is drawn).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SkipReason {
    /// Not an actor.
    NotActor,
    /// Destroyed.
    Deleted,
    /// The player pawn.
    Player,
    /// A class-default object (`Default__<Class>`).
    ClassDefault,
    /// `bHidden`.
    Hidden,
    /// `DrawType == 0` (`DT_None`).
    DrawTypeNone,
    /// No `Mesh`.
    NoMesh,
    /// The `Mesh` is not a `SkeletalMesh`.
    NotSkeletal,
}

impl SkipReason {
    /// Stable label for counters and the overlay.
    pub(crate) fn label(self) -> &'static str {
        match self {
            SkipReason::NotActor => "not-actor",
            SkipReason::Deleted => "deleted",
            SkipReason::Player => "player",
            SkipReason::ClassDefault => "class-default",
            SkipReason::Hidden => "bHidden",
            SkipReason::DrawTypeNone => "DrawType=DT_None",
            SkipReason::NoMesh => "no-mesh",
            SkipReason::NotSkeletal => "mesh-not-skeletal",
        }
    }
}

/// Actor selection rules (pure). `None` = the actor is rendered as a pawn.
pub(crate) fn skip_reason(a: &ActorView) -> Option<SkipReason> {
    if !a.is_actor {
        return Some(SkipReason::NotActor);
    }
    if a.deleted {
        return Some(SkipReason::Deleted);
    }
    if a.is_player {
        return Some(SkipReason::Player);
    }
    if a.is_class_default {
        return Some(SkipReason::ClassDefault);
    }
    match a.mesh {
        MeshKind::None => return Some(SkipReason::NoMesh),
        MeshKind::Other => return Some(SkipReason::NotSkeletal),
        MeshKind::StaticAttachment if !a.attached_to_bone => return Some(SkipReason::NotSkeletal),
        MeshKind::Skeletal | MeshKind::StaticAttachment => {}
    }
    if a.hidden {
        return Some(SkipReason::Hidden);
    }
    if a.draw_type == Some(0) {
        return Some(SkipReason::DrawTypeNone);
    }
    None
}

/// One selected pawn: its VM object and the `Package.Mesh` path to decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PawnSource {
    /// VM object id.
    pub id: ObjectId,
    /// VM display name.
    pub name: String,
    /// `Package.Mesh` path of the `Mesh` property.
    pub mesh_path: String,
}

/// Result of scanning the VM: the renderable pawns plus what was skipped and why.
#[derive(Debug, Default)]
pub(crate) struct Selection {
    /// Actors to render.
    pub pawns: Vec<PawnSource>,
    /// Skipped actors by reason.
    pub skipped: BTreeMap<&'static str, usize>,
    /// Names of selected bone-attached meshes.
    pub attachments: Vec<String>,
}

/// Builds the pure [`ActorView`] of `id` from the VM's public, read-only API. `player` is the
/// host player pawn id: exactly that actor is hidden first-person (a duplicate pawn the script
/// happened to spawn is still rendered, so a regression is visible rather than masked).
fn actor_view(vm: &Vm<'_>, id: ObjectId, player: ObjectId) -> ActorView {
    let o = &vm.objects[id as usize];
    let mut hidden = matches!(vm.get_property(id, "bHidden"), Some(Value::Bool(true)));
    let draw_type = match vm.get_property(id, "DrawType") {
        Some(Value::Byte(b)) => Some(*b),
        _ => None,
    };
    let attached_to_bone = matches!(
        vm.get_property(id, "AttachmentBone"),
        Some(Value::Name(n)) if !n.is_empty() && !n.eq_ignore_ascii_case("None")
    );
    // InventoryAttachment defaults are bHidden=true; they are drawn with their parent pawn.
    // This parent-owned visibility is an explicit hypothesis pending native traversal parity.
    if attached_to_bone && vm.is_a(id, "InventoryAttachment") {
        hidden = instance_property(vm, id, "Base").is_none_or(|parent| {
            parent == player
                || vm.objects[parent as usize].deleted
                || matches!(vm.get_property(parent, "bHidden"), Some(Value::Bool(true)))
        });
    }
    let mesh = match render_mesh(vm, id) {
        Some((_, c))
            if c.rsplit('.')
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case("SkeletalMesh")) =>
        {
            MeshKind::Skeletal
        }
        Some((_, c)) if attached_to_bone && c.eq_ignore_ascii_case("Engine.StaticMesh") => {
            MeshKind::StaticAttachment
        }
        Some(_) => MeshKind::Other,
        None => MeshKind::None,
    };
    ActorView {
        is_actor: o.is_actor,
        deleted: o.deleted,
        is_player: id == player,
        is_class_default: o.name.starts_with("Default__"),
        hidden,
        draw_type,
        mesh,
        attached_to_bone,
    }
}

/// Selects every renderable pawn from the VM (`session.player` is the player pawn id).
pub(crate) fn select(vm: &Vm<'_>, player: ObjectId) -> Selection {
    let mut out = Selection::default();
    for i in 0..vm.objects.len() {
        let id = i as ObjectId;
        let view = actor_view(vm, id, player);
        // An actor with no mesh is not interesting for this scan; only count the meaningful
        // skip reasons so the counters are readable (every map has hundreds of mesh-less actors).
        if view.mesh == MeshKind::None {
            continue;
        }
        match skip_reason(&view) {
            None => {
                let Some((mesh_path, _)) = render_mesh(vm, id) else {
                    continue;
                };
                if view.attached_to_bone {
                    out.attachments.push(vm.objects[i].name.clone());
                }
                out.pawns.push(PawnSource {
                    id,
                    name: vm.objects[i].name.clone(),
                    mesh_path,
                });
            }
            Some(reason) => {
                *out.skipped.entry(reason.label()).or_default() += 1;
            }
        }
    }
    out
}

fn render_mesh(vm: &Vm<'_>, id: ObjectId) -> Option<(String, String)> {
    vm.mesh_object(id).or_else(|| vm.static_mesh_object(id))
}

/// Third-person weapons are StaticMesh actors, not the weapon's first-person SkeletalMesh.
/// A single identity joint adapts rigid geometry to the shared GPU upload path; it is synthetic.
fn load_render_model(cache: &mut PackageCache, spec: &str) -> Result<LoadedModel, String> {
    let (package, object) = spec
        .split_once('.')
        .ok_or_else(|| format!("invalid mesh {spec}"))?;
    let loaded = cache.get(package)?;
    let index = (0..loaded.package.exports().len())
        .find(|&i| {
            loaded
                .package
                .object_path(xiii_package::ObjectRef::Export(i as u32))
                .is_some_and(|p| p.eq_ignore_ascii_case(object))
        })
        .ok_or_else(|| format!("unresolved mesh {spec}"))?;
    if loaded.package.export_class_path(index) != Some("Engine.StaticMesh") {
        return load_model(cache, spec);
    }
    let raw = xiii_decode::static_mesh::decode_static_mesh(&loaded.package, &loaded.data, index)
        .map_err(|e| format!("{spec}: {e}"))?;
    let n = raw.vertices.len();
    let uvs = raw
        .uv_streams
        .first()
        .ok_or_else(|| format!("{spec}: missing UV stream"))?
        .uvs
        .clone();
    if n == 0
        || uvs.len() != n
        || raw
            .vertices
            .iter()
            .any(|v| v.position.iter().chain(&v.normal).any(|f| !f.is_finite()))
    {
        return Err(format!("{spec}: invalid static vertices/UVs"));
    }
    let mut sections = Vec::new();
    let mut indices = Vec::new();
    for (i, section) in raw.sections.iter().enumerate() {
        if i >= raw.materials.len() || i > u16::MAX as usize {
            return Err(format!("{spec}: unresolved static material slot {i}"));
        }
        let start = usize::from(section.first_index);
        let end = start + usize::from(section.num_faces) * 3;
        let source = raw
            .indices
            .get(start..end)
            .ok_or_else(|| format!("{spec}: static section outside index buffer"))?;
        if source.iter().any(|&v| usize::from(v) >= n) {
            return Err(format!("{spec}: static index outside vertex buffer"));
        }
        sections.push(MeshSection {
            material: i as u16,
            first_index: indices.len() as u32,
            index_count: source.len() as u32,
        });
        for t in source.as_chunks::<3>().0 {
            indices.extend([u32::from(t[0]), u32::from(t[2]), u32::from(t[1])]);
        }
    }
    let decoded = DecodedMesh {
        positions: raw
            .vertices
            .iter()
            .map(|v| SourceVec::from_array(v.position))
            .collect(),
        normals: raw
            .vertices
            .iter()
            .map(|v| SourceVec::from_array(v.normal))
            .collect(),
        uvs,
        point_of_vertex: (0..n as u32).collect(),
        joints: vec![[0; 4]; n],
        weights: vec![[1.0, 0.0, 0.0, 0.0]; n],
        indices,
        sections,
        materials: raw
            .materials
            .iter()
            .map(|m| MaterialSlot {
                texture: Some(m.material),
                texture_path: loaded.package.object_path(m.material).map(str::to_owned),
                poly_flags: 0,
            })
            .collect(),
        mesh_scale: SourceVec::new(1.0, 1.0, 1.0),
        ..Default::default()
    };
    let skeleton = Skeleton {
        bones: vec![Bone {
            name: "_rigid_static_mesh".into(),
            parent: None,
            flags: 0,
            bind_local: SourceTransform::IDENTITY,
            bind_global: SourceTransform::IDENTITY,
        }],
    };
    Ok(LoadedModel {
        label: spec.into(),
        loaded,
        skeleton,
        decoded,
        anims: None,
        anim_label: None,
    })
}

/// GPU assets of one decoded mesh, shared by every placed instance.
struct CachedModel {
    loaded: Arc<LoadedModel>,
    assets: SkinnedAssets,
}

/// One rendered pawn instance.
pub(crate) struct PawnInstance {
    /// VM object id.
    pub id: ObjectId,
    /// VM display name.
    pub name: String,
    root: Entity,
    joints: Vec<Entity>,
    model: Arc<CachedModel>,
    mesh_path: String,
    linked: Vec<(String, Arc<AnimSet>)>,
    /// Current channel `(channel, sequence, frame)` for the overlay (updated every frame).
    pub current: Vec<(u8, String, f32)>,
}

/// Render state of the `--play` pawns.
#[derive(Resource, Default)]
pub(crate) struct PawnScene {
    /// Rendered instances.
    pub instances: Vec<PawnInstance>,
    /// Distinct decoded meshes uploaded.
    pub models: usize,
    /// Actors skipped, by reason.
    pub skipped: BTreeMap<&'static str, usize>,
    /// Selected bone-attached meshes.
    pub attachments: Vec<String>,
    /// Fixed/Update passes that wrote a pose.
    pub pose_updates: u64,
    /// Pawns carrying item3g bone-controller state (`SpineYawControl` / `SetBoneDirection`) that
    /// this renderer does not apply yet.
    pub bone_controls_not_applied: usize,
    /// Meshes that failed to decode: `(label, error)`.
    pub failures: Vec<String>,
    game_dir: std::path::PathBuf,
    model_cache: HashMap<String, Arc<CachedModel>>,
    animation_cache: HashMap<String, Arc<AnimSet>>,
    failed_animations: std::collections::HashSet<String>,
    failed_actors: std::collections::HashSet<ObjectId>,
}

/// Source-space mesh offset of the actor origin: `Location` is the actor origin, and the mesh
/// origin sits at `MeshOrigin`; the mesh is drawn with its origin at `Location - MeshOrigin`.
fn mesh_origin_offset(mesh_origin: [f32; 3], mesh_scale: [f32; 3]) -> [f32; 3] {
    to_bevy_position([
        mesh_origin[0] * mesh_scale[0],
        mesh_origin[1] * mesh_scale[1],
        mesh_origin[2] * mesh_scale[2],
    ])
}

/// Root transform of a pawn at VM `location`/`rotation` with the decoded mesh orientation and
/// the mesh-origin offset (applied in once place, in Bevy space).
fn root_transform(
    location: [f32; 3],
    rotation: [i32; 3],
    mesh_origin: [f32; 3],
    mesh_scale: [f32; 3],
    rot_origin: [i32; 3],
) -> Transform {
    // R = actor rotation * mesh RotOrigin (the mesh is authored +Y-forward; RotOrigin turns it
    // onto the actor's forward axis). Both are source rotators, converted by the same policy.
    let rot = bevy_rot_origin(rotation) * bevy_rot_origin(rot_origin);
    let offset = Vec3::from_array(mesh_origin_offset(mesh_origin, mesh_scale));
    Transform {
        translation: Vec3::from_array(to_bevy_position(location)) - rot * offset,
        rotation: rot,
        scale: Vec3::from_array(to_bevy_scale(mesh_scale)),
    }
}

/// Decodes/upload one model (cached by mesh path) and spawns all requested instances.
#[allow(clippy::too_many_arguments)]
pub(crate) fn setup_pawns(
    commands: &mut Commands,
    session: &Session,
    game_dir: &Path,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    bindposes: &mut Assets<SkinnedMeshInverseBindposes>,
) -> PawnScene {
    let mut scene = PawnScene {
        game_dir: game_dir.to_owned(),
        ..default()
    };
    spawn_missing(
        session, &mut scene, commands, meshes, materials, images, bindposes,
    );
    scene
}

fn diagnostic(scene: &mut PawnScene, message: String) {
    if !scene.failures.contains(&message) {
        eprintln!("[play] animation: {message}");
        scene.failures.push(message);
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_missing(
    session: &Session,
    scene: &mut PawnScene,
    commands: &mut Commands,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    bindposes: &mut Assets<SkinnedMeshInverseBindposes>,
) {
    let selection = select(session.vm(), session.player);
    scene.skipped = selection.skipped;
    scene.attachments = selection.attachments;
    let pending: Vec<_> = selection
        .pawns
        .into_iter()
        .filter(|src| {
            !scene.failed_actors.contains(&src.id)
                && !scene
                    .instances
                    .iter()
                    .any(|i| i.id == src.id && i.mesh_path == src.mesh_path)
        })
        .collect();
    if pending.is_empty() {
        return;
    }
    let mut cache = match PackageCache::open(&scene.game_dir) {
        Ok(c) => c,
        Err(e) => {
            diagnostic(scene, format!("cannot open installation: {e}"));
            return;
        }
    };
    let mut resolver = TextureResolver::new();
    let mut counts = BTreeMap::new();
    for src in pending {
        if let Some(index) = scene.instances.iter().position(|i| i.id == src.id) {
            commands
                .entity(scene.instances.remove(index).root)
                .despawn();
        }
        let model = if let Some(m) = scene.model_cache.get(&src.mesh_path) {
            m.clone()
        } else {
            let loaded = match load_render_model(&mut cache, &src.mesh_path) {
                Ok(m) => Arc::new(m),
                Err(e) => {
                    diagnostic(scene, format!("{} ({}): {e}", src.name, src.mesh_path));
                    scene.failed_actors.insert(src.id);
                    continue;
                }
            };
            let assets = build_skinned_assets(
                &mut cache,
                &loaded.loaded,
                &loaded.decoded,
                &loaded.skeleton,
                meshes,
                materials,
                images,
                bindposes,
                &mut resolver,
                &mut counts,
            );
            let m = Arc::new(CachedModel { loaded, assets });
            scene.model_cache.insert(src.mesh_path.clone(), m.clone());
            m
        };
        let spawned = spawn_skinned_entities(
            commands,
            &model.loaded.skeleton,
            &model.assets,
            Transform::default(),
            src.name.clone(),
        );
        scene.instances.push(PawnInstance {
            id: src.id,
            name: src.name,
            root: spawned.root,
            joints: spawned.joints,
            mesh_path: src.mesh_path,
            model,
            linked: Vec::new(),
            current: Vec::new(),
        });
    }
    scene.models = scene.model_cache.len();
}

fn recipe(c: &AnimChannelState) -> Sample {
    Sample {
        sequence: c.sequence.clone(),
        frame: c.frame,
        looping: c.looping,
        tween_alpha: if c.tween_duration > 0.0 {
            1.0 - c.tween_remaining / c.tween_duration
        } else {
            1.0
        },
        source: c.tween_source.as_ref().map(|s| Box::new(recipe(s))),
    }
}

fn channels(anim: &ActorAnimation) -> Vec<Channel> {
    anim.channels
        .iter()
        .map(|c| Channel {
            stage: c.channel,
            sample: recipe(c),
            alpha: c.blend_alpha,
            in_time: c.blend_in,
            bone: c.blend_bone.clone(),
        })
        .collect()
}

fn load_link(cache: &mut PackageCache, path: &str) -> Result<Arc<AnimSet>, String> {
    let (package, object) = path
        .split_once('.')
        .ok_or_else(|| format!("invalid animation path {path}"))?;
    let pkg = cache.get(package)?;
    let index = (0..pkg.package.exports().len())
        .find(|&i| {
            pkg.package
                .object_path(xiii_package::ObjectRef::Export(i as u32))
                .is_some_and(|p| p.eq_ignore_ascii_case(object))
        })
        .ok_or_else(|| format!("unresolved linked animation {path}"))?;
    let raw = xiii_decode::skeletal::decode_mesh_animation(&pkg.package, &pkg.data, index)
        .map_err(|e| format!("{path}: {e}"))?;
    AnimSet::from_raw(&raw)
        .map(Arc::new)
        .map_err(|e| format!("{path}: {e}"))
}

fn clip<'a>(
    model: &'a LoadedModel,
    linked: &'a [(String, Arc<AnimSet>)],
    name: &str,
) -> Option<&'a Clip> {
    linked
        .iter()
        .find_map(|(_, a)| a.clip(name))
        .or_else(|| model.anims.as_ref()?.clip(name))
}

fn pose_for_model(
    vm: &Vm<'_>,
    id: ObjectId,
    model: &LoadedModel,
    linked: &[(String, Arc<AnimSet>)],
) -> Result<Pose, String> {
    let anim = vm
        .actor_animation(id)
        .ok_or_else(|| "missing actor animation state".to_owned())?;
    let mut controls: Vec<Controller> = Vec::new();
    if let Some(bs) = vm.bone_state(id) {
        if bs.rotations.iter().any(|r| !r.alpha.is_finite())
            || bs
                .locations
                .iter()
                .any(|r| !r.alpha.is_finite() || r.trans.iter().any(|v| !v.is_finite()))
        {
            return Err("nonfinite bone controller request".into());
        }
        // Slots override in ascending slot order. Requests for the same bone replace previous state.
        let mut scales: Vec<_> = bs.scales.iter().collect();
        scales.sort_by_key(|s| s.slot);
        for c in scales {
            controls.push(Controller {
                bone: c.bone.clone(),
                scale: Some(SourceVec::from_array(c.scale)),
                ..default()
            });
        }
        for c in &bs.rotations {
            if c.space == 0 && c.alpha > 0.0 {
                controls.push(Controller {
                    bone: c.bone.clone(),
                    rotation: Some((c.turn, c.alpha)),
                    ..default()
                });
            }
        }
        for c in &bs.locations {
            if !bs
                .rotations
                .iter()
                .any(|r| r.bone.eq_ignore_ascii_case(&c.bone) && r.alpha > 0.0)
            {
                controls.push(Controller {
                    bone: c.bone.clone(),
                    translation: Some((SourceVec::from_array(c.trans), c.alpha)),
                    ..default()
                });
            }
        }
    }
    blend::evaluate(&model.skeleton, &channels(&anim), &controls, |s| {
        clip(model, linked, s)
    })
    .map_err(|e| e.to_string())
}

fn instance_property(vm: &Vm<'_>, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(ObjRef::Instance(i)))) => Some(*i),
        _ => None,
    }
}

/// Resolves attachment chains independently of VM object order; cycles/missing bones are diagnostics.
fn placed_root(
    vm: &Vm<'_>,
    scene: &PawnScene,
    poses: &[Option<Pose>],
    index: usize,
    visiting: &mut Vec<ObjectId>,
) -> Result<Transform, String> {
    let inst = &scene.instances[index];
    if visiting.len() >= 64 || visiting.contains(&inst.id) {
        return Err(format!("cyclic attachment at {}", inst.name));
    }
    visiting.push(inst.id);
    let d = &inst.model.loaded.decoded;
    let own = root_transform(
        [0.0; 3],
        [0; 3],
        d.mesh_origin.to_array(),
        d.mesh_scale.to_array(),
        d.rot_origin,
    );
    let bone_name = match vm.get_property(inst.id, "AttachmentBone") {
        Some(Value::Name(n)) if !n.eq_ignore_ascii_case("None") && !n.is_empty() => Some(n),
        _ => None,
    };
    let result = if let Some(bone_name) = bone_name {
        let parent = instance_property(vm, inst.id, "Base")
            .ok_or_else(|| format!("{}: AttachBone without Base", inst.name))?;
        let pi = scene
            .instances
            .iter()
            .position(|p| p.id == parent)
            .ok_or_else(|| format!("{}: attachment parent {parent} is not rendered", inst.name))?;
        let ps = &scene.instances[pi].model.loaded.skeleton;
        let bi = ps
            .find(bone_name)
            .ok_or_else(|| format!("{}: unresolved attachment bone {bone_name}", inst.name))?;
        let pose = poses[pi]
            .as_ref()
            .ok_or_else(|| format!("{}: parent pose failed", inst.name))?;
        let b = pose.globals[bi];
        let r = vm
            .vector_prop(inst.id, "RelativeLocation")
            .unwrap_or([0.0; 3]);
        let rr = [0; 3];
        let rr = match vm.get_property(inst.id, "RelativeRotation") {
            Some(Value::Rotator(r)) => *r,
            _ => rr,
        };
        let attached = blend::attachment(
            blend::Coords::new(SourceTransform::IDENTITY, SourceVec::new(1.0, 1.0, 1.0)),
            b,
            SourceTransform::new(blend::rotator_quat(rr), SourceVec::from_array(r)),
        );
        let local = Transform {
            translation: Vec3::from_array(to_bevy_position(attached.origin.to_array())),
            rotation: Quat::IDENTITY,
            scale: Vec3::ONE,
        };
        let axes = attached
            .axes
            .map(|a| Vec3::from_array(xiii_decode::common::to_bevy_direction(a.to_array())));
        let source_to_bevy =
            Mat3::from_cols_array(&xiii_world::to_cols(&xiii_decode::common::SOURCE_TO_BEVY));
        let matrix = Mat3::from_cols(axes[0], axes[1], axes[2]) * source_to_bevy.transpose();
        let mut attachment_matrix = Mat4::from_mat3(matrix);
        attachment_matrix.w_axis = local.translation.extend(1.0);
        let parent = placed_root(vm, scene, poses, pi, visiting)?;
        Transform::from_matrix(parent.to_matrix() * attachment_matrix * own.to_matrix())
    } else {
        let loc = vm
            .location_prop(inst.id)
            .ok_or_else(|| format!("{} has no Location", inst.name))?;
        root_transform(
            loc,
            vm.rotation_prop(inst.id).unwrap_or([0; 3]),
            d.mesh_origin.to_array(),
            d.mesh_scale.to_array(),
            d.rot_origin,
        )
    };
    visiting.pop();
    Ok(result)
}

/// Per-frame placement and posing of every rendered pawn. Reads the VM state the
/// `is_actor`/`Location`/`Rotation` host owns and the VM's own animation channels.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_pawns(
    session: NonSend<Result<Session, String>>,
    scene: Option<ResMut<PawnScene>>,
    mut transforms: Query<&mut Transform>,
    mut visibility: Query<&mut Visibility>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut bindposes: ResMut<Assets<SkinnedMeshInverseBindposes>>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = std::time::Instant::now();
    let Ok(sess) = session.as_ref() else {
        return;
    };
    let Some(mut scene) = scene else {
        return;
    };
    spawn_missing(
        sess,
        &mut scene,
        &mut commands,
        &mut meshes,
        &mut materials,
        &mut images,
        &mut bindposes,
    );
    let vm = sess.vm();
    let mut messages = Vec::new();
    scene.bone_controls_not_applied = 0;
    let game_dir = scene.game_dir.clone();
    let mut animation_cache = std::mem::take(&mut scene.animation_cache);
    let mut failed_animations = std::mem::take(&mut scene.failed_animations);
    for inst in &mut scene.instances {
        if let Some(bs) = vm.bone_state(inst.id)
            && (bs.spine.is_some_and(|s| s.is_controlled)
                || !bs.directions.is_empty()
                || bs.rotations.iter().any(|r| r.space != 0))
        {
            messages.push(format!("{}: SpineYawControl/SetBoneDirection/nonzero rotation Space remain Partial (world-space controller conversion not decoded)",inst.name));
        }
        if let Some(anim) = vm.actor_animation(inst.id) {
            for source in anim
                .sources
                .iter()
                .filter(|s| !s.eq_ignore_ascii_case(&inst.mesh_path))
            {
                if !inst
                    .linked
                    .iter()
                    .any(|(p, _)| p.eq_ignore_ascii_case(source))
                {
                    let key = source.to_ascii_lowercase();
                    if let Some(a) = animation_cache.get(&key) {
                        inst.linked.push((source.clone(), Arc::clone(a)));
                        continue;
                    }
                    if failed_animations.contains(&key) {
                        continue; // The decode failure was already reported; pose lookup still fails.
                    }
                    match PackageCache::open(&game_dir).and_then(|mut c| load_link(&mut c, source))
                    {
                        Ok(a) => {
                            animation_cache.insert(key, Arc::clone(&a));
                            inst.linked.push((source.clone(), a));
                        }
                        Err(e) => {
                            failed_animations.insert(key);
                            messages.push(format!("{}: {e}", inst.name));
                        }
                    }
                }
            }
            inst.current = anim
                .channels
                .iter()
                .map(|c| (c.channel, c.sequence.clone(), c.frame))
                .collect();
        }
    }
    scene.animation_cache = animation_cache;
    scene.failed_animations = failed_animations;
    scene.bone_controls_not_applied = messages
        .iter()
        .filter(|m| m.contains("remain Partial"))
        .count();
    let poses: Vec<_> = scene
        .instances
        .iter()
        .map(
            |inst| match pose_for_model(vm, inst.id, &inst.model.loaded, &inst.linked) {
                Ok(p) => Some(p),
                Err(e) => {
                    messages.push(format!("{}: {e}", inst.name));
                    None
                }
            },
        )
        .collect();
    for (i, inst) in scene.instances.iter().enumerate() {
        let visible = skip_reason(&actor_view(vm, inst.id, sess.player)).is_none();
        let root = if visible && poses[i].is_some() {
            placed_root(vm, &scene, &poses, i, &mut Vec::new())
        } else {
            Err("not visible".into())
        };
        if let Ok(mut v) = visibility.get_mut(inst.root) {
            *v = if root.is_ok() {
                Visibility::Inherited
            } else {
                Visibility::Hidden
            };
        }
        let Ok(root) = root else {
            if visible && let Err(e) = root {
                messages.push(format!("{}: {e}", inst.name));
            }
            continue;
        };
        if let Ok(mut t) = transforms.get_mut(inst.root) {
            *t = root;
        }
        if let Some(pose) = &poses[i] {
            for (bi, &e) in inst.joints.iter().enumerate() {
                if let Ok(mut t) = transforms.get_mut(e) {
                    *t = source_to_bevy_transform(&pose.locals[bi]);
                    t.scale = Vec3::from_array(to_bevy_scale(pose.scales[bi].to_array()));
                }
            }
        }
    }
    for message in messages {
        diagnostic(&mut scene, message);
    }
    scene.pose_updates += 1;
    perf.span("pawn_pose", t0);
}

/// Headless pawn report for the deterministic `--play-script` run (no window/GPU): the same
/// selection and mesh decode as [`setup_pawns`], without creating Bevy assets. Used so the
/// headless mode also states how many pawns the VM holds with resolved meshes.
pub(crate) fn headless_report(session: &Session, game_dir: &Path) -> Result<String, String> {
    let vm = session.vm();
    let selection = select(vm, session.player);
    let mut cache = PackageCache::open(game_dir)?;
    let mut unique: BTreeMap<String, bool> = BTreeMap::new();
    let mut failures: Vec<String> = Vec::new();
    for p in &selection.pawns {
        if unique.contains_key(&p.mesh_path) {
            continue;
        }
        match load_model(&mut cache, &p.mesh_path) {
            Ok(_) => {
                unique.insert(p.mesh_path.clone(), true);
            }
            Err(e) => {
                failures.push(format!("{}: {e}", p.mesh_path));
                unique.insert(p.mesh_path.clone(), false);
            }
        }
    }
    let resolved = unique.values().filter(|v| **v).count();
    Ok(format!(
        "pawns headless: {} selected, {} unique mesh(es) ({} resolved, {} failed); skipped {:?}{}",
        selection.pawns.len(),
        unique.len(),
        resolved,
        unique.len() - resolved,
        selection.skipped,
        if failures.is_empty() {
            String::new()
        } else {
            format!("; failures: {}", failures.join(", "))
        }
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(mesh: MeshKind) -> ActorView {
        ActorView {
            is_actor: true,
            deleted: false,
            is_player: false,
            is_class_default: false,
            hidden: false,
            draw_type: Some(2),
            mesh,
            attached_to_bone: false,
        }
    }

    #[test]
    fn selection_rules_accept_a_visible_skeletal_actor() {
        assert_eq!(skip_reason(&view(MeshKind::Skeletal)), None);
    }

    #[test]
    fn selection_rules_reject_each_hidden_category() {
        let mut v = view(MeshKind::Skeletal);
        v.is_actor = false;
        assert_eq!(skip_reason(&v), Some(SkipReason::NotActor));
        let mut v = view(MeshKind::Skeletal);
        v.deleted = true;
        assert_eq!(skip_reason(&v), Some(SkipReason::Deleted));
        let mut v = view(MeshKind::Skeletal);
        v.is_player = true;
        assert_eq!(skip_reason(&v), Some(SkipReason::Player));
        let mut v = view(MeshKind::Skeletal);
        v.is_class_default = true;
        assert_eq!(skip_reason(&v), Some(SkipReason::ClassDefault));
        let mut v = view(MeshKind::Skeletal);
        v.hidden = true;
        assert_eq!(skip_reason(&v), Some(SkipReason::Hidden));
        let mut v = view(MeshKind::Skeletal);
        v.draw_type = Some(0);
        assert_eq!(skip_reason(&v), Some(SkipReason::DrawTypeNone));
        let mut v = view(MeshKind::Skeletal);
        v.draw_type = None;
        assert_eq!(skip_reason(&v), None, "an absent DrawType is not DT_None");
        assert_eq!(skip_reason(&view(MeshKind::None)), Some(SkipReason::NoMesh));
        assert_eq!(
            skip_reason(&view(MeshKind::Other)),
            Some(SkipReason::NotSkeletal)
        );
        let mut v = view(MeshKind::Skeletal);
        v.attached_to_bone = true;
        assert_eq!(skip_reason(&v), None);
    }

    /// The mesh origin is subtracted from the actor origin, so the mesh's origin (its feet for
    /// the XIIIM bind pose) lands below the collision centre.
    #[test]
    fn mesh_origin_subtracts_along_the_actor_up_axis() {
        let t = root_transform([0.0, 0.0, 90.0], [0; 3], [0.0, 0.0, 45.0], [1.0; 3], [0; 3]);
        assert!((t.translation.y - 0.5).abs() < 1e-5, "{:?}", t.translation);
        assert!(t.rotation.is_near_identity());
    }

    /// A 90-degree actor yaw turns the mesh-origin offset with it (the offset is applied in the
    /// actor's frame, not world axes).
    #[test]
    fn mesh_origin_rotates_with_the_actor() {
        // Yaw 16384 = +X -> +Y in source, i.e. Bevy forward -Z -> +X. MeshOrigin +45 UU on the
        // source Z (up) axis is unaffected by yaw; use a source +X offset instead (1 m forward).
        let t = root_transform(
            [0.0, 0.0, 0.0],
            [0, 16384, 0],
            [90.0, 0.0, 0.0],
            [1.0; 3],
            [0; 3],
        );
        // Mesh origin +X in source maps to Bevy -Z; after the yaw it points +X, so the actor
        // moves -X by one metre.
        assert!((t.translation.x + 1.0).abs() < 1e-4, "{:?}", t.translation);
        assert!(t.translation.y.abs() < 1e-5);
    }

    fn opt_in_root() -> Option<std::path::PathBuf> {
        let root = std::env::var_os("XIII_GOG_DIR")?;
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        Some(if path.is_relative() {
            ws.join(path)
        } else {
            path
        })
    }

    /// Opt-in corpus test (requirement 7): Plage00's `--play` VM spawns soldier pawns and every
    /// one whose `Mesh` is an `Engine.SkeletalMesh` is selected by the renderer and decodes into
    /// a skeleton (not just a name). Headless: no window/GPU assets are created here, the same
    /// selection and decode path `setup_pawns` uses is exercised.
    #[test]
    fn opt_in_plage00_soldier_pawns_resolve_meshes() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let session = Session::open(&game_dir, "Plage00").expect("open Plage00 session");
        let vm = session.vm();
        let selection = select(vm, session.player);
        let soldiers: Vec<&PawnSource> = selection
            .pawns
            .iter()
            .filter(|p| vm.is_a(p.id, "basesoldier"))
            .collect();
        println!(
            "[pawn test] Plage00: {} pawns selected ({} soldier), {} meshes to decode; skipped {:?}; attachments {:?}",
            selection.pawns.len(),
            soldiers.len(),
            selection
                .pawns
                .iter()
                .map(|p| p.mesh_path.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len(),
            selection.skipped,
            selection.attachments
        );
        assert!(!soldiers.is_empty(), "no BaseSoldier pawns selected");

        // Expected: every live soldier with a SkeletalMesh Mesh (regardless of the visibility
        // rules, which are the `select` filter being tested).
        let expected = (0..vm.objects.len())
            .filter(|&i| {
                let id = i as ObjectId;
                vm.objects[i].is_actor
                    && !vm.objects[i].deleted
                    && !vm.objects[i].name.starts_with("Default__")
                    && vm.is_a(id, "basesoldier")
                    && vm.mesh_object(id).is_some_and(|(_, c)| {
                        c.rsplit('.')
                            .next()
                            .is_some_and(|s| s.eq_ignore_ascii_case("SkeletalMesh"))
                    })
            })
            .count();
        let mut cache = PackageCache::open(&game_dir).expect("open install");
        let mut resolved = 0usize;
        for p in &soldiers {
            match load_model(&mut cache, &p.mesh_path) {
                Ok(m) => {
                    resolved += 1;
                    println!(
                        "[pawn test]   soldier {} mesh {} -> {} bones, {} clips",
                        p.name,
                        p.mesh_path,
                        m.skeleton.bones.len(),
                        m.anims.as_ref().map_or(0, |a| a.clips.len())
                    );
                }
                Err(e) => panic!(
                    "soldier {} mesh {} did not decode: {e}",
                    p.name, p.mesh_path
                ),
            }
        }
        println!(
            "[pawn test] spawned soldier pawns with SkeletalMesh: {expected}, resolved {resolved}"
        );
        assert!(expected >= 1, "expected at least one soldier pawn");
        assert!(
            resolved >= expected,
            "renderer selected {resolved} soldiers with resolved meshes but the VM has {expected}"
        );
    }

    #[test]
    fn opt_in_item47_temporary_suspension_diagnostic() {
        let Some(root) = opt_in_root() else {
            return;
        };
        for map in ["Amos01", "Plage01"] {
            let mut session = Session::open(&root, map).unwrap();
            let loc = session.player_location().unwrap();
            for _ in 0..1800 {
                session.drive_render_phase();
                session.step(
                    1.0 / 60.0,
                    loc,
                    0.0,
                    [0.0; 3],
                    &super::super::session::PlayerVMModes::default(),
                );
            }
            println!("item47 diagnostic {map} failures={:?}", session.failures);
        }
    }

    #[test]
    fn opt_in_base01_and_plage01_soldier_blends_and_attachments() {
        let Some(game_dir) = opt_in_root() else {
            println!("SKIPPED: set XIII_GOG_DIR for item47 soldier/weapon checks");
            return;
        };
        for map in ["Base01", "Plage01"] {
            let mut session = Session::open(&game_dir, map).expect("open session");
            let loc = session.player_location().expect("player location");
            for _ in 0..360 {
                session.step(
                    1.0 / 60.0,
                    loc,
                    0.0,
                    [0.0; 3],
                    &super::super::session::PlayerVMModes::default(),
                );
            }
            if map == "Base01" {
                let soldier = session
                    .vm()
                    .find_object("BaseSoldier17")
                    .expect("requested soldier");
                let sloc = session
                    .vm()
                    .vector_prop(soldier, "Location")
                    .expect("soldier location");
                let rotation = session
                    .vm()
                    .rotation_prop(soldier)
                    .expect("soldier rotation");
                let yaw = rotation[1] as f32 * std::f32::consts::TAU / 65536.0;
                let player = [
                    sloc[0] + 70.0 * yaw.cos(),
                    sloc[1] + 70.0 * yaw.sin(),
                    sloc[2] + 20.0,
                ];
                let health = session.player_health().expect("player health");
                for _ in 0..420 {
                    session.step(
                        1.0 / 60.0,
                        player,
                        yaw + std::f32::consts::PI,
                        [0.0; 3],
                        &super::super::session::PlayerVMModes::default(),
                    );
                }
                let weapon =
                    instance_property(session.vm(), soldier, "Weapon").expect("soldier weapon");
                assert!(
                    session.vm().is_a(weapon, "m16"),
                    "BaseSoldier17 must fire the authored M16"
                );
                assert!(
                    session.player_health().expect("health after combat") < health,
                    "BaseSoldier17 M16 did not damage the player"
                );
                println!(
                    "item47 BaseSoldier17 M16 combat: Health {health} -> {:?}",
                    session.player_health()
                );
            }
            let vm = session.vm();
            let selected = select(vm, session.player);
            let mut cache = PackageCache::open(&game_dir).expect("cache");
            let mut checked = 0;
            for src in &selected.pawns {
                let Some(parent) = instance_property(vm, src.id, "Base") else {
                    continue;
                };
                if !vm.is_a(parent, "basesoldier") {
                    continue;
                }
                if map == "Base01" && vm.objects[parent as usize].name != "BaseSoldier17" {
                    continue;
                }
                let (parent_mesh, _) = vm.mesh_object(parent).expect("parent mesh");
                let model = load_model(&mut cache, &parent_mesh).expect("decode soldier");
                let mut linked = Vec::new();
                for source in vm.actor_animation(parent).expect("animation").sources {
                    if !source.eq_ignore_ascii_case(&parent_mesh) {
                        linked.push((
                            source.clone(),
                            load_link(&mut cache, &source).expect("linked animation"),
                        ));
                    }
                }
                let pose = pose_for_model(vm, parent, &model, &linked).expect("soldier pose");
                let bone = match vm.get_property(src.id, "AttachmentBone") {
                    Some(Value::Name(n)) => n,
                    _ => panic!("attachment has no bone"),
                };
                let bi = model.skeleton.find(bone).expect("attachment bone");
                let relative = vm
                    .vector_prop(src.id, "RelativeLocation")
                    .expect("relative location");
                let rotation = match vm.get_property(src.id, "RelativeRotation") {
                    Some(Value::Rotator(r)) => *r,
                    _ => [0; 3],
                };
                let coords = blend::attachment(
                    blend::Coords::new(SourceTransform::IDENTITY, SourceVec::new(1.0, 1.0, 1.0)),
                    pose.globals[bi],
                    SourceTransform::new(
                        blend::rotator_quat(rotation),
                        SourceVec::from_array(relative),
                    ),
                );
                assert!(coords.origin.is_finite() && coords.axes.iter().all(|v| v.is_finite()));
                let weapon =
                    load_render_model(&mut cache, &src.mesh_path).expect("decode attached mesh");
                assert!(
                    !weapon.decoded.indices.is_empty(),
                    "attached weapon must have real decoded geometry"
                );
                println!(
                    "item47 {map}: {} -> {} bone={bone} relative={relative:?} mesh-origin={:?}, {} channels",
                    vm.objects[parent as usize].name,
                    src.name,
                    coords.origin,
                    vm.actor_animation(parent).unwrap().channels.len()
                );
                checked += 1;
            }
            assert!(
                checked > 0,
                "{map}: requested soldier weapon attachment not selected after initialization; attachments {:?}",
                selected.attachments
            );
        }
    }
}
