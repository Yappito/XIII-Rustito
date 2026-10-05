//! First-person weapon presentation for `--play` (item14).
//!
//! The player pawn itself is not drawn ([`super::pawns`] skips it, first person). This module
//! draws the player's current `Weapon` actor's decoded first-person mesh (`XIIIWeapon.MeshName`,
//! e.g. `XIIIArmes.FpsBerrettaM`, loaded by `Weapon.PostBeginPlay` through `DynamicLoadObject`)
//! as a child of the play camera, and poses it from the weapon actor's VM animation channels when
//! they exist (falling back to the decoded bind pose).
//!
//! **Presentation deviation.** The scripts position the first-person mesh with
//! `PlayerViewOffset` (a few Unreal units from the eye) and `DrawScale`. A modern camera near
//! plane (0.1 m) clips geometry that close, and the decoded mesh size is not calibrated. The
//! view therefore re-centres the decoded mesh on a fixed camera-local offset and scales it to a
//! fixed on-screen length. The script values are reported in the invocation line; the placement
//! is presentation only and does not affect firing (the trace originates at the pawn's eye).

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use bevy::ecs::system::NonSendMut;
use bevy::mesh::skinning::SkinnedMeshInverseBindposes;
use bevy::prelude::*;

use xiii_decode::common::to_bevy_position;
use xiii_decode::skeletal::normalize::{Clip, evaluate_pose};
use xiii_script::ObjectId;
use xiii_world::PackageCache;

use super::session::Session;
use super::{PlayCam, PlayConfig};
use crate::viewer::skinned::{
    LoadedModel, TextureResolver, bevy_locals, build_skinned_assets, load_model,
    spawn_skinned_entities,
};

/// Target on-screen length (metres) of the largest decoded mesh dimension.
const WEAPON_SIZE_M: f32 = 1.0;
/// Camera-local centre of the weapon view (Bevy axes: +X right, +Y up, -Z forward).
const WEAPON_OFFSET: Vec3 = Vec3::new(0.0, -0.30, -1.25);

#[derive(Resource, Default)]
pub(crate) struct WeaponView {
    /// Player weapon object the current view was built for.
    weapon: Option<ObjectId>,
    /// `Package.Mesh` path currently shown.
    pub mesh: Option<String>,
    root: Option<Entity>,
    joints: Vec<Entity>,
    model: Option<Arc<CachedWeapon>>,
    /// Human-readable state of the view (reported by the overlay).
    pub status: String,
    /// Last build failure, if any (never silent).
    pub failed: Option<String>,
    /// Frames the pose has been written.
    pub updates: u64,
    /// Channel sequence currently sampled (for the overlay).
    pub sequence: String,
}

struct CachedWeapon {
    loaded: Arc<LoadedModel>,
    bone_map: Vec<Option<usize>>,
    /// Uniform scale that makes the largest decoded dimension `WEAPON_SIZE_M`.
    scale: f32,
    /// Camera-local transform of the view (offset + scale), applied to the camera each frame.
    local: Transform,
}

/// Builds the weapon view when the player's `Weapon` changes (a `weapon` script command grants
/// one through the game's own `GiveTo`), then poses it every frame.
#[allow(clippy::too_many_arguments)]
pub(crate) fn update_weapon_view(
    mut commands: Commands,
    cfg: Res<PlayConfig>,
    mut session: NonSendMut<Result<Session, String>>,
    mut view: ResMut<WeaponView>,
    cam: Query<&Transform, With<PlayCam>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut bindposes: ResMut<Assets<SkinnedMeshInverseBindposes>>,
    mut transforms: Query<&mut Transform, Without<PlayCam>>,
) {
    let Ok(sess) = session.as_mut() else {
        return;
    };
    let weapon = sess.player_weapon();
    if weapon != view.weapon {
        view.weapon = weapon;
        view.mesh = None;
        view.failed = None;
        view.status.clear();
        view.sequence = "<bind>".to_owned();
        view.model = None;
        if let Some(root) = view.root.take() {
            commands.entity(root).despawn();
        }
        view.joints.clear();
        let Some(weapon) = weapon else {
            view.status = "no weapon".to_owned();
            return;
        };
        let Some(mesh_path) = sess.weapon_mesh_name(weapon) else {
            view.status = format!("weapon {weapon} has no MeshName/Mesh");
            return;
        };
        let script_offset = sess
            .weapon_vector(weapon, "PlayerViewOffset")
            .map(|v| format!("({:.1},{:.1},{:.1}) UU", v[0], v[1], v[2]))
            .unwrap_or_else(|| "absent".to_owned());
        let script_scale = sess
            .weapon_float(weapon, "DrawScale")
            .map(|s| format!("{s:.2}"))
            .unwrap_or_else(|| "absent".to_owned());
        let Some(game_dir) = cfg.options.game_dir.as_deref() else {
            view.failed = Some("no --game-dir".to_owned());
            return;
        };
        match build_weapon(
            &mut commands,
            game_dir,
            &mesh_path,
            &mut meshes,
            &mut materials,
            &mut images,
            &mut bindposes,
        ) {
            Ok((cached, creatures)) => {
                view.mesh = Some(mesh_path.clone());
                view.root = Some(creatures.root);
                view.joints = creatures.joints;
                view.status = format!(
                    "{mesh_path} ({} bones, {} clips, {} sections, {} verts, view scale {:.3}; \
                     script PlayerViewOffset {script_offset}, DrawScale {script_scale})",
                    cached.loaded.skeleton.bones.len(),
                    cached.loaded.anims.as_ref().map_or(0, |a| a.clips.len()),
                    cached.loaded.decoded.sections.len(),
                    cached.loaded.decoded.positions.len(),
                    cached.scale,
                );
                println!("[play] weapon view: {}", view.status);
                view.model = Some(Arc::new(cached));
            }
            Err(e) => {
                view.failed = Some(e.clone());
                view.status = format!("{mesh_path}: {e}");
            }
        }
    }
    // Pose every frame from the weapon's VM animation state (bind pose when it has none).
    let (Some(weapon), Some(model)) = (view.weapon, view.model.clone()) else {
        return;
    };
    let anim = sess.vm().actor_animation(weapon);
    let (clip, frame, looping, seq) =
        match anim.as_ref().and_then(|a| sample_weapon_channel(&model, a)) {
            Some((clip, frame, looping, seq)) => (Some(clip), frame, looping, seq),
            None => (None, 0.0, false, "<bind>".to_owned()),
        };
    view.sequence = seq;
    let pose = match clip {
        Some(c) => evaluate_pose(&model.loaded.skeleton, c, &model.bone_map, frame, looping),
        None => model.loaded.skeleton.bind_globals(),
    };
    let locals = bevy_locals(&model.loaded.skeleton, &pose);
    for (bi, &e) in view.joints.iter().enumerate() {
        if let Some(local) = locals.get(bi)
            && let Ok(mut t) = transforms.get_mut(e)
        {
            *t = *local;
        }
    }
    // Place the weapon relative to the current camera pose.
    if let Some(root) = view.root
        && let Ok(cam) = cam.single()
        && let Ok(mut t) = transforms.get_mut(root)
    {
        *t = cam.mul_transform(model.local);
    }
    view.updates += 1;
}

/// Lowest-indexed active channel, else the lowest-indexed channel, else `None`.
fn sample_weapon_channel<'m>(
    model: &'m CachedWeapon,
    anim: &xiii_script::ActorAnimation,
) -> Option<(&'m Clip, f32, bool, String)> {
    let set = model.loaded.anims.as_ref()?;
    let channel = anim
        .channels
        .iter()
        .filter(|c| c.active)
        .min_by_key(|c| c.channel)
        .or_else(|| anim.channels.iter().min_by_key(|c| c.channel))?;
    let clip = set.clip(&channel.sequence)?;
    Some((
        clip,
        channel.frame,
        channel.looping,
        channel.sequence.clone(),
    ))
}

struct WeaponEntities {
    root: Entity,
    joints: Vec<Entity>,
}

#[allow(clippy::too_many_arguments)]
fn build_weapon(
    commands: &mut Commands,
    game_dir: &Path,
    mesh_path: &str,
    meshes: &mut Assets<Mesh>,
    materials: &mut Assets<StandardMaterial>,
    images: &mut Assets<Image>,
    bindposes: &mut Assets<SkinnedMeshInverseBindposes>,
) -> Result<(CachedWeapon, WeaponEntities), String> {
    let mut cache = PackageCache::open(game_dir)?;
    let loaded = Arc::new(load_model(&mut cache, mesh_path)?);
    let mut resolver = TextureResolver::new();
    let mut counters: BTreeMap<String, usize> = BTreeMap::new();
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
        &mut counters,
    );
    let bone_map = loaded
        .anims
        .as_ref()
        .map(|a| a.bone_map(&loaded.skeleton))
        .unwrap_or_default();
    let (min, max) = decoded_bounds(&loaded);
    let size = (max - min).max_element().max(1e-4);
    let scale = WEAPON_SIZE_M / size;
    let center = (max + min) * 0.5;
    let root_transform = Transform {
        translation: WEAPON_OFFSET - center * scale,
        rotation: Quat::IDENTITY,
        scale: Vec3::splat(scale),
    };
    let spawned = spawn_skinned_entities(
        commands,
        &loaded.skeleton,
        &assets,
        Transform::IDENTITY,
        format!("weapon {mesh_path}"),
    );
    let cached = CachedWeapon {
        loaded,
        bone_map,
        scale,
        local: root_transform,
    };
    // The weapon root is a root-level entity transformed from the camera every frame (the play
    // camera has no `Visibility`, so `ChildOf(camera)` would inherit-invisible).
    Ok((
        cached,
        WeaponEntities {
            root: spawned.root,
            joints: spawned.joints,
        },
    ))
}

fn decoded_bounds(model: &LoadedModel) -> (Vec3, Vec3) {
    let mut min = [f32::INFINITY; 3];
    let mut max = [f32::NEG_INFINITY; 3];
    for p in &model.decoded.positions {
        let b = to_bevy_position([p.x, p.y, p.z]);
        for k in 0..3 {
            min[k] = min[k].min(b[k]);
            max[k] = max[k].max(b[k]);
        }
    }
    if !min.iter().all(|v| v.is_finite()) || !max.iter().all(|v| v.is_finite()) {
        (Vec3::ZERO, Vec3::ZERO)
    } else {
        (Vec3::from_array(min), Vec3::from_array(max))
    }
}

/// The overlay line for the first-person weapon view.
pub(crate) fn overlay_line(view: &WeaponView) -> String {
    match (&view.mesh, &view.failed) {
        (Some(m), None) => format!(
            "weapon view {m} seq {} ({} pose updates)",
            view.sequence, view.updates
        ),
        (_, Some(e)) => format!("weapon view failed: {e}"),
        _ => format!("weapon view {}", view.status),
    }
}
