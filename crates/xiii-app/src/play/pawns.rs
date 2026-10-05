//! Map-pawn rendering for `--play`.
//!
//! Every **live** VM actor with an `Engine.SkeletalMesh` `Mesh` is drawn GPU-skinned at its VM
//! `Location`/`Rotation` and posed from the VM's own animation state (the channel's current
//! sequence, frame, rate and looping flag) with the shared CPU sampler
//! (`xiii_decode::skeletal::normalize::evaluate_pose`, the same one the `--model` viewer and
//! `xiii-tool anim` use). The player pawn is not drawn (first person).
//!
//! **Hidden actors** follow the decoded `Engine.Actor` properties: `bHidden` is the decoded
//! visibility flag; `DrawType` is the UE2 `EDrawType` byte where `0 = DT_None` (nothing drawn)
//! and `2 = DT_Mesh` (the value in every decoded `BaseSoldier` class default). An actor whose
//! mesh is attached to a bone (`AttachBone` set by `Actor.AttachToBone`) is **not** drawn because
//! the VM stores no attachment transform; it is listed in [`PawnScene::attachments`] instead.
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
use xiii_decode::skeletal::normalize::{Clip, evaluate_pose};
use xiii_script::{ActorAnimation, ObjectId, Value, Vm};
use xiii_world::PackageCache;

use super::session::Session;
use crate::viewer::skinned::{
    LoadedModel, SkinnedAssets, TextureResolver, bevy_locals, bevy_rot_origin,
    build_skinned_assets, load_model, spawn_skinned_entities,
};

/// Renderable class of an actor's `Mesh` object.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MeshKind {
    /// No `Mesh` (or a null one).
    None,
    /// `Engine.SkeletalMesh` (the pawn renderer can draw it).
    Skeletal,
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
    /// `AttachBone` is set (`Actor.AttachToBone`), so the mesh follows a bone the VM does not
    /// resolve.
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
    /// Attached to a bone with no attachment transform.
    AttachedToBone,
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
            SkipReason::AttachedToBone => "attached-to-bone",
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
        MeshKind::Skeletal => {}
    }
    if a.hidden {
        return Some(SkipReason::Hidden);
    }
    if a.draw_type == Some(0) {
        return Some(SkipReason::DrawTypeNone);
    }
    if a.attached_to_bone {
        return Some(SkipReason::AttachedToBone);
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
    /// Names of meshes attached to a bone (listed as not yet rendered).
    pub attachments: Vec<String>,
}

/// Builds the pure [`ActorView`] of `id` from the VM's public, read-only API. `player` is the
/// host player pawn id: exactly that actor is hidden first-person (a duplicate pawn the script
/// happened to spawn is still rendered, so a regression is visible rather than masked).
fn actor_view(vm: &Vm<'_>, id: ObjectId, player: ObjectId) -> ActorView {
    let o = &vm.objects[id as usize];
    let hidden = matches!(vm.get_property(id, "bHidden"), Some(Value::Bool(true)));
    let draw_type = match vm.get_property(id, "DrawType") {
        Some(Value::Byte(b)) => Some(*b),
        _ => None,
    };
    let attached_to_bone = matches!(
        vm.get_property(id, "AttachBone"),
        Some(Value::Name(n)) if !n.is_empty() && !n.eq_ignore_ascii_case("None")
    );
    let mesh = match vm.mesh_object(id) {
        Some((_, c))
            if c.rsplit('.')
                .next()
                .is_some_and(|s| s.eq_ignore_ascii_case("SkeletalMesh")) =>
        {
            MeshKind::Skeletal
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
                let Some((mesh_path, _)) = vm.mesh_object(id) else {
                    continue;
                };
                out.pawns.push(PawnSource {
                    id,
                    name: vm.objects[i].name.clone(),
                    mesh_path,
                });
            }
            Some(SkipReason::AttachedToBone) => {
                out.attachments.push(vm.objects[i].name.clone());
                *out.skipped
                    .entry(SkipReason::AttachedToBone.label())
                    .or_default() += 1;
            }
            Some(reason) => {
                *out.skipped.entry(reason.label()).or_default() += 1;
            }
        }
    }
    out
}

/// GPU assets of one decoded mesh, shared by every placed instance.
struct CachedModel {
    loaded: Arc<LoadedModel>,
    assets: SkinnedAssets,
    bone_map: Vec<Option<usize>>,
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
    /// Meshes attached to a bone (not rendered).
    pub attachments: Vec<String>,
    /// Fixed/Update passes that wrote a pose.
    pub pose_updates: u64,
    /// Pawns carrying item3g bone-controller state (`SpineYawControl` / `SetBoneDirection`) that
    /// this renderer does not apply yet.
    pub bone_controls_not_applied: usize,
    /// Meshes that failed to decode: `(label, error)`.
    pub failures: Vec<String>,
}

/// Source-space mesh offset of the actor origin: `Location` is the actor origin, and the mesh
/// origin sits at `MeshOrigin`; the mesh is drawn with its origin at `Location - MeshOrigin`.
fn mesh_origin_offset(mesh_origin: [f32; 3], mesh_scale: [f32; 3]) -> [f32; 3] {
    let s = to_bevy_scale(mesh_scale);
    to_bevy_position([
        mesh_origin[0] * s[0],
        mesh_origin[1] * s[1],
        mesh_origin[2] * s[2],
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
        scale: Vec3::ONE,
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
    let vm = session.vm();
    let selection = select(vm, session.player);
    let mut cache = match PackageCache::open(game_dir) {
        Ok(c) => c,
        Err(e) => {
            let mut scene = PawnScene {
                skipped: selection.skipped,
                attachments: selection.attachments,
                ..default()
            };
            scene
                .failures
                .push(format!("cannot open {game_dir:?}: {e}"));
            return scene;
        }
    };
    let mut resolver = TextureResolver::new();
    let mut materials_counter: BTreeMap<String, usize> = BTreeMap::new();
    let mut models: HashMap<String, Arc<CachedModel>> = HashMap::new();
    let mut scene = PawnScene {
        skipped: selection.skipped,
        attachments: selection.attachments,
        ..default()
    };
    for src in selection.pawns {
        let model = if let Some(m) = models.get(&src.mesh_path) {
            m.clone()
        } else {
            let loaded = match load_model(&mut cache, &src.mesh_path) {
                Ok(l) => Arc::new(l),
                Err(e) => {
                    scene
                        .failures
                        .push(format!("{} ({}): {e}", src.name, src.mesh_path));
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
                &mut materials_counter,
            );
            let bone_map = loaded
                .anims
                .as_ref()
                .map(|a| a.bone_map(&loaded.skeleton))
                .unwrap_or_default();
            let cm = Arc::new(CachedModel {
                bone_map,
                assets,
                loaded,
            });
            models.insert(src.mesh_path.clone(), cm.clone());
            cm
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
            model,
            current: Vec::new(),
        });
        if let Some(bs) = vm.bone_state(src.id)
            && (bs.spine.is_some() || !bs.directions.is_empty() || !bs.scales.is_empty())
        {
            scene.bone_controls_not_applied += 1;
        }
    }
    scene.models = models.len();
    scene
}

/// Picks the clip/frame/looping to sample for `anim`: the lowest-indexed active channel, else
/// the lowest-indexed channel (a finished non-looping sequence keeps its final frame), else
/// `None` (bind pose).
fn sample_channel<'m>(
    model: &'m CachedModel,
    anim: &ActorAnimation,
) -> Option<(&'m Clip, f32, bool)> {
    let set = model.loaded.anims.as_ref()?;
    let channel = anim
        .channels
        .iter()
        .filter(|c| c.active)
        .min_by_key(|c| c.channel)
        .or_else(|| anim.channels.iter().min_by_key(|c| c.channel))?;
    let clip = set.clip(&channel.sequence)?;
    Some((clip, channel.frame, channel.looping))
}

/// Per-frame placement and posing of every rendered pawn. Reads the VM state the
/// `is_actor`/`Location`/`Rotation` host owns and the VM's own animation channels.
pub(crate) fn update_pawns(
    session: NonSend<Result<Session, String>>,
    scene: Option<ResMut<PawnScene>>,
    mut transforms: Query<&mut Transform>,
    mut perf: ResMut<crate::perf::Perf>,
) {
    let t0 = std::time::Instant::now();
    let Ok(sess) = session.as_ref() else {
        return;
    };
    let Some(mut scene) = scene else {
        return;
    };
    let vm = sess.vm();
    for inst in &mut scene.instances {
        let Some(location) = vm.location_prop(inst.id) else {
            continue;
        };
        let rotation = vm.rotation_prop(inst.id).unwrap_or([0; 3]);
        let decoded = &inst.model.loaded.decoded;
        let root = root_transform(
            location,
            rotation,
            [
                decoded.mesh_origin.x,
                decoded.mesh_origin.y,
                decoded.mesh_origin.z,
            ],
            [
                decoded.mesh_scale.x,
                decoded.mesh_scale.y,
                decoded.mesh_scale.z,
            ],
            decoded.rot_origin,
        );
        if let Ok(mut t) = transforms.get_mut(inst.root) {
            *t = root;
        }

        let anim = vm.actor_animation(inst.id);
        let pose = match anim.as_ref().and_then(|a| sample_channel(&inst.model, a)) {
            Some((clip, frame, looping)) => evaluate_pose(
                &inst.model.loaded.skeleton,
                clip,
                &inst.model.bone_map,
                frame,
                looping,
            ),
            None => inst.model.loaded.skeleton.bind_globals(),
        };
        let locals = bevy_locals(&inst.model.loaded.skeleton, &pose);
        for (bi, &e) in inst.joints.iter().enumerate() {
            if let Some(local) = locals.get(bi)
                && let Ok(mut t) = transforms.get_mut(e)
            {
                *t = *local;
            }
        }
        inst.current = anim
            .as_ref()
            .map(|a| {
                a.channels
                    .iter()
                    .map(|c| (c.channel, c.sequence.clone(), c.frame))
                    .collect()
            })
            .unwrap_or_default();
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
        assert_eq!(skip_reason(&v), Some(SkipReason::AttachedToBone));
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
}
