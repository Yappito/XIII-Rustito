//! Non-visual plausibility checks over normalized products.

use super::math::Vec3;
use super::normalize::{AnimSet, Clip, Skeleton, SkinnedMesh, evaluate_pose};

/// Skeleton/bind-pose checks.
#[derive(Debug, Clone, PartialEq)]
pub struct SkeletonReport {
    /// Bone count.
    pub bones: usize,
    /// Root count.
    pub roots: usize,
    /// Deepest chain.
    pub max_depth: usize,
    /// All bind transforms finite.
    pub bind_finite: bool,
    /// Largest deviation of a stored quaternion from unit length (before normalization).
    pub max_quat_norm_error: f32,
    /// Bind joint bounds (mesh space).
    pub joint_min: Vec3,
    /// Bind joint bounds (mesh space).
    pub joint_max: Vec3,
}

impl SkeletonReport {
    /// Vertical (Z) extent of the bind joints.
    pub fn height(&self) -> f32 {
        self.joint_max.z - self.joint_min.z
    }
}

/// Computes [`SkeletonReport`]. `stored_quats` are the raw stored orientations.
pub fn skeleton_report(skeleton: &Skeleton, stored_quats: &[[f32; 4]]) -> SkeletonReport {
    let mut min = Vec3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY);
    let mut max = -min;
    let mut finite = true;
    for b in &skeleton.bones {
        finite &= b.bind_global.is_finite() && b.bind_local.is_finite();
        min = min.min(b.bind_global.translation);
        max = max.max(b.bind_global.translation);
    }
    let max_quat_norm_error = stored_quats
        .iter()
        .map(|q| ((q[0] * q[0] + q[1] * q[1] + q[2] * q[2] + q[3] * q[3]).sqrt() - 1.0).abs())
        .fold(0.0f32, f32::max);
    SkeletonReport {
        bones: skeleton.bones.len(),
        roots: skeleton.root_count(),
        max_depth: skeleton.max_depth(),
        bind_finite: finite,
        max_quat_norm_error,
        joint_min: min,
        joint_max: max,
    }
}

/// Skinned-mesh checks.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshReport {
    /// Vertices (wedges).
    pub vertices: usize,
    /// Triangles.
    pub triangles: usize,
    /// Sections.
    pub sections: usize,
    /// Smallest normalized weight sum.
    pub weight_sum_min: f32,
    /// Largest normalized weight sum.
    pub weight_sum_max: f32,
    /// Degenerate triangles (repeated point or zero area).
    pub degenerate_triangles: usize,
    /// Bind-pose bounds.
    pub bind_min: Vec3,
    /// Bind-pose bounds.
    pub bind_max: Vec3,
    /// Fraction of vertex normals pointing away from the centroid (winding evidence).
    pub outward_normal_fraction: f32,
    /// Largest distance between bind vertices and the identity-pose skinned vertices.
    pub bind_skin_max_error: f32,
}

/// Computes [`MeshReport`].
pub fn mesh_report(mesh: &SkinnedMesh, skeleton: &Skeleton) -> MeshReport {
    let mut wmin = f32::INFINITY;
    let mut wmax = f32::NEG_INFINITY;
    for w in &mesh.weights {
        let s: f32 = w.iter().sum();
        wmin = wmin.min(s);
        wmax = wmax.max(s);
    }
    let mut degenerate = 0;
    for t in mesh.indices.as_chunks::<3>().0 {
        let [a, b, c] = t.map(|i| i as usize);
        let pa = mesh.point_of_vertex[a];
        let pb = mesh.point_of_vertex[b];
        let pc = mesh.point_of_vertex[c];
        let area = (mesh.positions[b] - mesh.positions[a])
            .cross(mesh.positions[c] - mesh.positions[a])
            .length();
        if pa == pb || pb == pc || pa == pc || area <= 1e-9 {
            degenerate += 1;
        }
    }
    let (bmin, bmax) = bounds(&mesh.positions);
    let n = mesh.positions.len().max(1) as f32;
    let centroid = mesh.positions.iter().fold(Vec3::ZERO, |a, &p| a + p) * (1.0 / n);
    let outward = mesh
        .positions
        .iter()
        .zip(&mesh.normals)
        .filter(|&(&p, &nrm)| nrm.dot(p - centroid) > 0.0)
        .count() as f32
        / n;
    let skinned = mesh.skin(skeleton, &skeleton.bind_globals());
    let err = skinned
        .iter()
        .zip(&mesh.positions)
        .map(|(&a, &b)| (a - b).length())
        .fold(0.0f32, f32::max);
    MeshReport {
        vertices: mesh.positions.len(),
        triangles: mesh.indices.len() / 3,
        sections: mesh.sections.len(),
        weight_sum_min: wmin,
        weight_sum_max: wmax,
        degenerate_triangles: degenerate,
        bind_min: bmin,
        bind_max: bmax,
        outward_normal_fraction: outward,
        bind_skin_max_error: err,
    }
}

/// Axis-aligned bounds of a point set (inverted infinities for an empty set).
pub fn bounds(points: &[Vec3]) -> (Vec3, Vec3) {
    let mut min = Vec3::new(f32::INFINITY, f32::INFINITY, f32::INFINITY);
    let mut max = -min;
    for &p in points {
        min = min.min(p);
        max = max.max(p);
    }
    (min, max)
}

/// Clip structure checks.
#[derive(Debug, Clone, PartialEq)]
pub struct ClipReport {
    /// Clip name.
    pub name: String,
    /// Tracks equal the animation bone count.
    pub track_count_ok: bool,
    /// Key times strictly increasing within every track.
    pub times_monotone: bool,
    /// Every key time in `0..track_time`.
    pub times_in_range: bool,
    /// Every track starts at time 0.
    pub starts_at_zero: bool,
    /// Position counts are 1 or equal to the rotation count.
    pub position_counts_ok: bool,
    /// All rotation keys finite.
    pub quats_finite: bool,
    /// Total rotation keys.
    pub rotation_keys: usize,
    /// Notifies whose time is outside 0..=1.
    pub notifies_out_of_range: usize,
}

/// Computes [`ClipReport`].
pub fn clip_report(set: &AnimSet, clip: &Clip) -> ClipReport {
    let mut r = ClipReport {
        name: clip.name.clone(),
        track_count_ok: clip.tracks.len() == set.bone_names.len(),
        times_monotone: true,
        times_in_range: true,
        starts_at_zero: true,
        position_counts_ok: true,
        quats_finite: true,
        rotation_keys: 0,
        notifies_out_of_range: clip
            .notifies
            .iter()
            .filter(|n| !(0.0..=1.0).contains(&n.time))
            .count(),
    };
    for t in &clip.tracks {
        r.rotation_keys += t.rotations.len();
        r.times_monotone &= t.times.windows(2).all(|w| w[1] > w[0]);
        r.times_in_range &= t.times.iter().all(|&x| x >= 0.0 && x < clip.track_time);
        r.starts_at_zero &= t.times.first().is_some_and(|&x| x == 0.0);
        r.position_counts_ok &= t.positions.len() == 1 || t.positions.len() == t.times.len();
        r.quats_finite &= t.rotations.iter().all(|q| q.is_finite());
    }
    r
}

/// Skinned bounds of one sampled frame.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct FrameBounds {
    /// Frame time.
    pub frame: f32,
    /// Minimum corner.
    pub min: Vec3,
    /// Maximum corner.
    pub max: Vec3,
    /// Root-relative position of a chosen probe bone (e.g. a foot), for periodicity checks.
    pub probe: Vec3,
}

/// Samples `clip` every `step` frames over `0..=track_time` and records skinned bounds and the
/// mesh-space position of bone `probe`.
pub fn skinned_bounds_over_time(
    mesh: &SkinnedMesh,
    skeleton: &Skeleton,
    clip: &Clip,
    map: &[Option<usize>],
    step: f32,
    probe: usize,
) -> Vec<FrameBounds> {
    let mut out = Vec::new();
    let step = if step > 0.0 { step } else { 1.0 };
    let mut f = 0.0f32;
    while f <= clip.track_time + 1e-4 {
        let pose = evaluate_pose(skeleton, clip, map, f, true);
        let pts = mesh.skin(skeleton, &pose);
        let (min, max) = bounds(&pts);
        let probe = pose.get(probe).map(|t| t.translation).unwrap_or_default();
        out.push(FrameBounds {
            frame: f,
            min,
            max,
            probe,
        });
        f += step;
    }
    out
}
