//! Renderer-independent products: skeleton, skinned mesh, clips, pose evaluation and skinning.
//!
//! All values stay in source coordinates (Unreal units, Z-up). Rotation convention (measured,
//! see README): the stored root-bone orientation is used as is and every other bone's stored
//! orientation is conjugated, for mesh reference bones and animation keys alike. With this rule
//! the XIII bind poses come out as upright T-poses (feet at z ~ 0, head joint ~ 145 units) whose
//! joints sit inside the reference point cloud; using the stored values unchanged folds the legs
//! sideways. The same rule appears in UE2-derived PSK/PSA tooling; UModel is not cited for it.

use xiii_package::{ObjectRef, Package};

use super::anim::{RawMeshAnimation, TrackRange};
use super::math::{Quat, Transform, Vec3};
use super::mesh::RawSkeletalMesh;

/// Maximum influences kept per vertex.
pub const MAX_INFLUENCES: usize = 4;

/// Applies the XIII/UE2 storage convention to a stored bone orientation.
pub fn local_rotation(stored: [f32; 4], is_root: bool) -> Quat {
    let q = Quat::from_array(stored).normalized();
    if is_root { q } else { q.conjugate() }
}

/// A structural problem found while building normalized products.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizeError(pub String);

impl std::fmt::Display for NormalizeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for NormalizeError {}

/// One skeleton bone.
#[derive(Debug, Clone, PartialEq)]
pub struct Bone {
    /// Name.
    pub name: String,
    /// Parent bone; `None` for roots.
    pub parent: Option<usize>,
    /// Raw flags.
    pub flags: u32,
    /// Bind transform relative to the parent (convention applied).
    pub bind_local: Transform,
    /// Bind transform in mesh space.
    pub bind_global: Transform,
}

/// Bone hierarchy with bind transforms. Parents always precede children.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Skeleton {
    /// Bones in file order.
    pub bones: Vec<Bone>,
}

impl Skeleton {
    /// Builds the skeleton from a mesh's reference bones. A bone whose parent index equals its
    /// own index is a root (bone 0 is stored with parent 0); any parent index greater than the
    /// bone's own index or out of range is rejected, which also rules out cycles.
    pub fn from_mesh(raw: &RawSkeletalMesh) -> Result<Self, NormalizeError> {
        let mut bones: Vec<Bone> = Vec::with_capacity(raw.ref_skeleton.len());
        for (i, b) in raw.ref_skeleton.iter().enumerate() {
            let parent = match usize::try_from(b.parent) {
                Ok(p) if p == i => None,
                Ok(p) if p < i => Some(p),
                _ => {
                    return Err(NormalizeError(format!(
                        "bone {i} '{}' has parent index {} (must be < {i} or == {i})",
                        b.name, b.parent
                    )));
                }
            };
            let bind_local = Transform::new(
                local_rotation(b.orientation, parent.is_none()),
                Vec3::from_array(b.position),
            );
            let bind_global = match parent {
                Some(p) => bones[p].bind_global.mul(&bind_local),
                None => bind_local,
            };
            bones.push(Bone {
                name: b.name.clone(),
                parent,
                flags: b.flags,
                bind_local,
                bind_global,
            });
        }
        Ok(Self { bones })
    }

    /// Case-insensitive bone lookup.
    pub fn find(&self, name: &str) -> Option<usize> {
        self.bones
            .iter()
            .position(|b| b.name.eq_ignore_ascii_case(name))
    }

    /// Number of root bones.
    pub fn root_count(&self) -> usize {
        self.bones.iter().filter(|b| b.parent.is_none()).count()
    }

    /// Depth of the deepest bone (a root has depth 1).
    pub fn max_depth(&self) -> usize {
        let mut depth = vec![0usize; self.bones.len()];
        for (i, b) in self.bones.iter().enumerate() {
            depth[i] = b.parent.map_or(1, |p| depth[p] + 1);
        }
        depth.into_iter().max().unwrap_or(0)
    }

    /// Mesh-space transforms from local transforms (one per bone).
    pub fn globals(&self, locals: &[Transform]) -> Vec<Transform> {
        let mut out: Vec<Transform> = Vec::with_capacity(self.bones.len());
        for (i, b) in self.bones.iter().enumerate() {
            let l = locals.get(i).copied().unwrap_or(b.bind_local);
            out.push(match b.parent {
                Some(p) => out[p].mul(&l),
                None => l,
            });
        }
        out
    }

    /// Bind-pose global transforms.
    pub fn bind_globals(&self) -> Vec<Transform> {
        self.bones.iter().map(|b| b.bind_global).collect()
    }
}

/// Material slot of a skinned mesh.
#[derive(Debug, Clone, PartialEq)]
pub struct MaterialSlot {
    /// Texture reference (from `FMeshMaterial::TextureIndex` into the skin list).
    pub texture: Option<ObjectRef>,
    /// Object path of the texture, when resolvable.
    pub texture_path: Option<String>,
    /// Unreal poly flags.
    pub poly_flags: u32,
}

/// Triangles that share one material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshSection {
    /// Material slot.
    pub material: u16,
    /// First index in [`SkinnedMesh::indices`].
    pub first_index: u32,
    /// Number of indices (3 per triangle).
    pub index_count: u32,
}

/// Influence statistics gathered while building a [`SkinnedMesh`].
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct InfluenceStats {
    /// Points that no influence references (given full weight on bone 0).
    pub points_without_influences: usize,
    /// Points with more than [`MAX_INFLUENCES`] influences (truncated to the largest).
    pub points_truncated: usize,
    /// Influences whose bone index is outside the skeleton (dropped, weights renormalized).
    pub invalid_bone_influences: usize,
    /// Largest influence count of any point.
    pub max_influences: usize,
    /// Smallest raw weight sum (before normalization) over influenced points.
    pub raw_sum_min: f32,
    /// Largest raw weight sum (before normalization).
    pub raw_sum_max: f32,
}

/// Skinned triangle mesh, one vertex per source wedge.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SkinnedMesh {
    /// Bind-pose positions (mesh space).
    pub positions: Vec<Vec3>,
    /// Smooth outward normals computed from the faces (area weighted per point).
    pub normals: Vec<Vec3>,
    /// Texture coordinates.
    pub uvs: Vec<[f32; 2]>,
    /// Source point of each vertex.
    pub point_of_vertex: Vec<u32>,
    /// Bone indices (unused slots are 0 with weight 0).
    pub joints: Vec<[u16; MAX_INFLUENCES]>,
    /// Weights, summing to 1.
    pub weights: Vec<[f32; MAX_INFLUENCES]>,
    /// Triangle list, sorted by section.
    pub indices: Vec<u32>,
    /// Sections by material.
    pub sections: Vec<MeshSection>,
    /// Material slots.
    pub materials: Vec<MaterialSlot>,
    /// `MeshScale` (applied by the actor, not baked).
    pub mesh_scale: Vec3,
    /// `MeshOrigin` (applied by the actor, not baked).
    pub mesh_origin: Vec3,
    /// `RotOrigin` (pitch, yaw, roll; 65536 = full turn).
    pub rot_origin: [i32; 3],
    /// Influence statistics.
    pub influence_stats: InfluenceStats,
}

impl SkinnedMesh {
    /// Builds the skinned mesh. Influences come from the version-1 `WeightIndices` groups
    /// (group `g` lists points with `g + 1` influences, read sequentially from
    /// `BoneInfluences` starting at the group's `start`), as in UModel `UpgradeMesh`.
    pub fn from_raw(
        raw: &RawSkeletalMesh,
        skeleton: &Skeleton,
        package: Option<&Package>,
    ) -> Result<Self, NormalizeError> {
        let n_points = raw.points.len();
        let n_bones = skeleton.bones.len();
        let err = |s: String| Err(NormalizeError(s));
        if raw.vertex_count as usize != n_points {
            return err(format!(
                "vertex count {} != {} points",
                raw.vertex_count, n_points
            ));
        }
        // Collect influences per point.
        let mut infl: Vec<Vec<(u16, f32)>> = vec![Vec::new(); n_points];
        let mut next = 0usize;
        let mut invalid_bone_influences = 0usize;
        for (g, wi) in raw.weight_indices.iter().enumerate() {
            if usize::try_from(wi.start).ok() != Some(next) {
                return err(format!(
                    "weight group {g} starts at {} but {next} influences were consumed",
                    wi.start
                ));
            }
            for &p in &wi.points {
                let p = usize::from(p);
                if p >= n_points {
                    return err(format!("weight group {g} point {p} >= {n_points}"));
                }
                for _ in 0..=g {
                    let Some(bi) = raw.bone_influences.get(next) else {
                        return err(format!(
                            "weight group {g} needs influence {next} of {}",
                            raw.bone_influences.len()
                        ));
                    };
                    if usize::from(bi.bone) >= n_bones {
                        // Observed once in the corpus (SlaterM: 10 influences on bone 0xFFFF
                        // with small weights). Dropped and counted; the remaining weights of
                        // the point are renormalized.
                        invalid_bone_influences += 1;
                    } else {
                        infl[p].push((bi.bone, f32::from(bi.weight) / 65535.0));
                    }
                    next += 1;
                }
            }
        }
        if next != raw.bone_influences.len() {
            return err(format!(
                "{} of {} influences referenced by weight groups",
                next,
                raw.bone_influences.len()
            ));
        }
        let mut stats = InfluenceStats {
            invalid_bone_influences,
            raw_sum_min: f32::INFINITY,
            raw_sum_max: f32::NEG_INFINITY,
            ..Default::default()
        };
        let mut point_joints = vec![[0u16; MAX_INFLUENCES]; n_points];
        let mut point_weights = vec![[0f32; MAX_INFLUENCES]; n_points];
        for (p, list) in infl.iter_mut().enumerate() {
            stats.max_influences = stats.max_influences.max(list.len());
            if list.is_empty() {
                stats.points_without_influences += 1;
                point_weights[p][0] = 1.0;
                continue;
            }
            let sum: f32 = list.iter().map(|x| x.1).sum();
            stats.raw_sum_min = stats.raw_sum_min.min(sum);
            stats.raw_sum_max = stats.raw_sum_max.max(sum);
            if list.len() > MAX_INFLUENCES {
                stats.points_truncated += 1;
                list.sort_by(|a, b| b.1.total_cmp(&a.1));
                list.truncate(MAX_INFLUENCES);
            }
            let kept: f32 = list.iter().map(|x| x.1).sum();
            for (k, &(bone, w)) in list.iter().enumerate() {
                point_joints[p][k] = bone;
                point_weights[p][k] = if kept > 0.0 {
                    w / kept
                } else {
                    1.0 / list.len() as f32
                };
            }
        }
        if stats.raw_sum_min > stats.raw_sum_max {
            stats.raw_sum_min = 0.0;
            stats.raw_sum_max = 0.0;
        }

        // Vertices = wedges.
        let mut positions = Vec::with_capacity(raw.wedges.len());
        let mut uvs = Vec::with_capacity(raw.wedges.len());
        let mut point_of_vertex = Vec::with_capacity(raw.wedges.len());
        let mut joints = Vec::with_capacity(raw.wedges.len());
        let mut weights = Vec::with_capacity(raw.wedges.len());
        for (w, wedge) in raw.wedges.iter().enumerate() {
            let p = usize::from(wedge.point);
            if p >= n_points {
                return err(format!("wedge {w} point {p} >= {n_points}"));
            }
            positions.push(Vec3::from_array(raw.points[p]));
            uvs.push(wedge.uv);
            point_of_vertex.push(p as u32);
            joints.push(point_joints[p]);
            weights.push(point_weights[p]);
        }

        // Faces -> sections (stable order by material).
        let n_mat = raw.materials.len();
        let mut by_mat: Vec<Vec<u32>> = vec![Vec::new(); n_mat.max(1)];
        for (f, face) in raw.faces.iter().enumerate() {
            let m = usize::from(face.material);
            if m >= n_mat {
                return err(format!("face {f} material {m} >= {n_mat}"));
            }
            for &wi in &face.wedges {
                if usize::from(wi) >= raw.wedges.len() {
                    return err(format!("face {f} wedge {wi} >= {}", raw.wedges.len()));
                }
                by_mat[m].push(u32::from(wi));
            }
        }
        let mut indices = Vec::with_capacity(raw.faces.len() * 3);
        let mut sections = Vec::new();
        for (m, list) in by_mat.into_iter().enumerate() {
            if list.is_empty() {
                continue;
            }
            sections.push(MeshSection {
                material: m as u16,
                first_index: indices.len() as u32,
                index_count: list.len() as u32,
            });
            indices.extend(list);
        }

        // Smooth normals per point, area weighted.
        let mut pn = vec![Vec3::ZERO; n_points];
        for face in &raw.faces {
            let p: [usize; 3] = face
                .wedges
                .map(|w| usize::from(raw.wedges[usize::from(w)].point));
            let [a, b, c] = p.map(|i| Vec3::from_array(raw.points[i]));
            // Stored winding is clockwise seen from outside when read as right-handed
            // coordinates (Unreal is left-handed), so the reversed cross product points out.
            let n = (c - a).cross(b - a);
            for i in p {
                pn[i] = pn[i] + n;
            }
        }
        let normals = point_of_vertex
            .iter()
            .map(|&p| pn[p as usize].normalized())
            .collect();

        let materials = raw
            .materials
            .iter()
            .map(|m| {
                let texture = usize::try_from(m.texture_index)
                    .ok()
                    .and_then(|i| raw.textures.get(i).copied())
                    .filter(|t| !t.is_null());
                MaterialSlot {
                    texture,
                    texture_path: texture
                        .and_then(|t| package.and_then(|p| p.object_path(t)))
                        .map(str::to_owned),
                    poly_flags: m.poly_flags,
                }
            })
            .collect();

        Ok(Self {
            positions,
            normals,
            uvs,
            point_of_vertex,
            joints,
            weights,
            indices,
            sections,
            materials,
            mesh_scale: Vec3::from_array(raw.mesh_scale),
            mesh_origin: Vec3::from_array(raw.mesh_origin),
            rot_origin: raw.rot_origin,
            influence_stats: stats,
        })
    }

    /// Linear-blend skinning of every vertex with mesh-space bone transforms `pose` against
    /// the skeleton's bind pose.
    pub fn skin(&self, skeleton: &Skeleton, pose: &[Transform]) -> Vec<Vec3> {
        let skin: Vec<Transform> = skeleton
            .bones
            .iter()
            .zip(pose)
            .map(|(b, g)| g.mul(&b.bind_global.inverse()))
            .collect();
        self.positions
            .iter()
            .zip(self.joints.iter().zip(&self.weights))
            .map(|(&p, (j, w))| {
                let mut acc = Vec3::ZERO;
                for k in 0..MAX_INFLUENCES {
                    if w[k] > 0.0 {
                        let t = skin.get(usize::from(j[k])).copied().unwrap_or_default();
                        acc = acc + t.transform_point(p) * w[k];
                    }
                }
                acc
            })
            .collect()
    }
}

/// One bone's keys in a clip (times in frames).
#[derive(Debug, Clone, PartialEq)]
pub struct BoneTrack {
    /// Animated bone name (matched to mesh bones case-insensitively).
    pub bone: String,
    /// Key times in frames.
    pub times: Vec<f32>,
    /// Rotation keys (convention applied, normalized), one per time.
    pub rotations: Vec<Quat>,
    /// Position keys: one (constant) or one per time.
    pub positions: Vec<Vec3>,
}

fn key_pair(times: &[f32], frame: f32, track_time: f32, looping: bool) -> (usize, usize, f32) {
    let n = times.len();
    if n <= 1 || frame <= times[0] {
        return (0, 0, 0.0);
    }
    // Last key with time <= frame.
    let k = times.partition_point(|&t| t <= frame) - 1;
    if k + 1 < n {
        let span = times[k + 1] - times[k];
        let a = if span > 0.0 {
            (frame - times[k]) / span
        } else {
            0.0
        };
        (k, k + 1, a.clamp(0.0, 1.0))
    } else if looping && track_time > times[k] {
        // Wrap from the last key back to key 0 over the remainder of the track.
        let a = (frame - times[k]) / (track_time - times[k]);
        (k, 0, a.clamp(0.0, 1.0))
    } else {
        (k, k, 0.0)
    }
}

impl BoneTrack {
    /// Samples the local transform at `frame` (0..=track_time). With `looping`, the interval
    /// after the last key interpolates back to the first key (Engine.dll track sampler
    /// 0x103ef228?0x103ef2c3). Non-loop playback stops at frame N-1 before the wrap interval.
    pub fn sample(&self, frame: f32, track_time: f32, looping: bool) -> Transform {
        let (a, b, t) = key_pair(&self.times, frame, track_time, looping);
        let rotation = match (self.rotations.get(a), self.rotations.get(b)) {
            (Some(&qa), Some(&qb)) => qa.slerp(qb, t),
            (Some(&qa), None) => qa,
            _ => Quat::IDENTITY,
        };
        let translation = if self.positions.len() == self.times.len() && self.positions.len() > 1 {
            self.positions[a].lerp(self.positions[b], t)
        } else {
            self.positions.first().copied().unwrap_or_default()
        };
        Transform::new(rotation, translation)
    }
}

/// A notify event.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipNotify {
    /// Normalized time (stored value; 0..1 expected).
    pub time: f32,
    /// Script function name.
    pub function: String,
}

/// One animation sequence.
#[derive(Debug, Clone, PartialEq)]
pub struct Clip {
    /// Sequence name.
    pub name: String,
    /// Groups.
    pub groups: Vec<String>,
    /// Playback rate (frames per second).
    pub rate: f32,
    /// Frame count.
    pub num_frames: u32,
    /// Track length in frames (equals `num_frames` in the corpus).
    pub track_time: f32,
    /// Duration in seconds (`track_time / rate`).
    pub duration: f32,
    /// Root speed (always zero in the corpus).
    pub root_speed: Vec3,
    /// One track per animation bone, in animation-bone order.
    pub tracks: Vec<BoneTrack>,
    /// Notifies.
    pub notifies: Vec<ClipNotify>,
}

/// Animation set: animated skeleton (names/parents) and clips.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AnimSet {
    /// Animation bone names.
    pub bone_names: Vec<String>,
    /// Animation bone parents (`None` for roots).
    pub bone_parents: Vec<Option<usize>>,
    /// Clips (sequence order).
    pub clips: Vec<Clip>,
}

fn track_from(
    range: &TrackRange,
    raw: &super::anim::MotionChunk,
    name: &str,
    root: bool,
) -> BoneTrack {
    let qs = usize::from(range.quat_start);
    let qn = usize::from(range.quat_count);
    let ps = usize::from(range.pos_start);
    let pn = usize::from(range.pos_count);
    BoneTrack {
        bone: name.to_owned(),
        times: raw.quat_times[qs..qs + qn].to_vec(),
        rotations: raw.quats[qs..qs + qn]
            .iter()
            .map(|q| local_rotation(q.map(|c| f32::from(c) / 32767.0), root))
            .collect(),
        positions: raw.positions[ps..ps + pn]
            .iter()
            .map(|&p| Vec3::from_array(p))
            .collect(),
    }
}

impl AnimSet {
    /// Builds clips from a decoded MeshAnimation. Requires one motion chunk per sequence and
    /// one track per reference bone (ranges were bounds-checked by the decoder).
    pub fn from_raw(raw: &RawMeshAnimation) -> Result<Self, NormalizeError> {
        if raw.moves.len() != raw.sequences.len() {
            return Err(NormalizeError(format!(
                "{} motion chunks for {} sequences",
                raw.moves.len(),
                raw.sequences.len()
            )));
        }
        let n = raw.ref_bones.len();
        let bone_names: Vec<String> = raw.ref_bones.iter().map(|b| b.name.clone()).collect();
        let mut bone_parents = Vec::with_capacity(n);
        for (i, b) in raw.ref_bones.iter().enumerate() {
            bone_parents.push(match usize::try_from(b.parent) {
                Ok(p) if p == i => None,
                Ok(p) if p < i => Some(p),
                _ => {
                    return Err(NormalizeError(format!(
                        "animation bone {i} '{}' has parent {}",
                        b.name, b.parent
                    )));
                }
            });
        }
        let mut clips = Vec::with_capacity(raw.sequences.len());
        for (s, (seq, mv)) in raw.sequences.iter().zip(&raw.moves).enumerate() {
            if mv.tracks.len() != n {
                return Err(NormalizeError(format!(
                    "sequence {s} '{}' has {} tracks for {n} bones",
                    seq.name,
                    mv.tracks.len()
                )));
            }
            let tracks = mv
                .tracks
                .iter()
                .enumerate()
                .map(|(k, r)| track_from(r, mv, &bone_names[k], bone_parents[k].is_none()))
                .collect();
            let num_frames = u32::try_from(seq.num_frames).map_err(|_| {
                NormalizeError(format!("sequence {s} frame count {}", seq.num_frames))
            })?;
            let duration = if seq.rate > 0.0 {
                mv.track_time / seq.rate
            } else {
                0.0
            };
            clips.push(Clip {
                name: seq.name.clone(),
                groups: seq.groups.clone(),
                rate: seq.rate,
                num_frames,
                track_time: mv.track_time,
                duration,
                root_speed: Vec3::from_array(mv.root_speed),
                tracks,
                notifies: seq
                    .notifies
                    .iter()
                    .map(|n| ClipNotify {
                        time: n.time,
                        function: n.function.clone(),
                    })
                    .collect(),
            });
        }
        Ok(Self {
            bone_names,
            bone_parents,
            clips,
        })
    }

    /// Case-insensitive clip lookup.
    pub fn clip(&self, name: &str) -> Option<&Clip> {
        self.clips
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(name))
    }

    /// For each skeleton bone, the index of the animation track with the same name.
    pub fn bone_map(&self, skeleton: &Skeleton) -> Vec<Option<usize>> {
        skeleton
            .bones
            .iter()
            .map(|b| {
                self.bone_names
                    .iter()
                    .position(|n| n.eq_ignore_ascii_case(&b.name))
            })
            .collect()
    }
}

/// Mesh-space pose of `skeleton` for `clip` at `frame`. Bones without a track keep their bind
/// local transform. Track positions replace bind positions (no retargeting).
pub fn evaluate_pose(
    skeleton: &Skeleton,
    clip: &Clip,
    map: &[Option<usize>],
    frame: f32,
    looping: bool,
) -> Vec<Transform> {
    let locals: Vec<Transform> = skeleton
        .bones
        .iter()
        .enumerate()
        .map(|(i, b)| match map.get(i).copied().flatten() {
            Some(t) => clip.tracks[t].sample(frame, clip.track_time, looping),
            None => b.bind_local,
        })
        .collect();
    skeleton.globals(&locals)
}
