//! CPU channel composition in source coordinates. See item47's DLL research report.
//! Tween sources are frozen sampling recipes, so interrupted tweens do not jump.
use super::math::{Quat, Transform, Vec3};
use super::normalize::{Clip, NormalizeError, Skeleton};

/// A frozen channel sample, including an interrupted tween.
#[derive(Clone, Debug, PartialEq)]
pub struct Sample {
    /// Sequence name, resolved by the caller from its linked animation sources.
    pub sequence: String,
    /// Position in authored frames.
    pub frame: f32,
    /// Sample the closing interval of a loop.
    pub looping: bool,
    /// Progress from the frozen source to this sample (one without a tween).
    pub tween_alpha: f32,
    /// Previous channel sample; absent means reference pose.
    pub source: Option<Box<Sample>>,
}

/// One animation stage. Channel zero always covers the entire skeleton at alpha one.
#[derive(Clone, Debug)]
pub struct Channel {
    /// Engine stage index; stages compose in ascending order.
    pub stage: u8,
    /// Playback and tween recipe.
    pub sample: Sample,
    /// Current blend amount, advanced by the VM.
    pub alpha: f32,
    /// Normalized sequence fraction used for blend-in (not seconds).
    pub in_time: f32,
    /// Subtree start; absent or an unknown name selects root as MatchRefBone does.
    pub bone: Option<String>,
}

/// Local rotation/translation controls; alpha scales rotator components before conversion.
#[derive(Clone, Debug, Default)]
pub struct Controller {
    /// Target bone name.
    pub bone: String,
    /// Relative rotator and alpha. Only decoded space zero is supported here.
    pub rotation: Option<([i32; 3], f32)>,
    /// Local translation offset and alpha.
    pub translation: Option<(Vec3, f32)>,
    /// Local basis scale, assigned by slot order before other controls.
    pub scale: Option<Vec3>,
}

/// Affine source-space bone coordinates, including scale for children and attachments.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Coords {
    /// Basis columns.
    pub axes: [Vec3; 3],
    /// Origin.
    pub origin: Vec3,
}

impl Coords {
    /// Rotation/translation with local scale.
    pub fn new(t: Transform, s: Vec3) -> Self {
        Self {
            axes: [
                t.rotation.rotate(Vec3::new(s.x, 0.0, 0.0)),
                t.rotation.rotate(Vec3::new(0.0, s.y, 0.0)),
                t.rotation.rotate(Vec3::new(0.0, 0.0, s.z)),
            ],
            origin: t.translation,
        }
    }

    /// Applies the linear part.
    pub fn vector(self, v: Vec3) -> Vec3 {
        self.axes[0] * v.x + self.axes[1] * v.y + self.axes[2] * v.z
    }

    /// Applies a complete affine transform.
    pub fn point(self, v: Vec3) -> Vec3 {
        self.origin + self.vector(v)
    }

    /// Composes column-vector transforms, including nonuniform scale.
    pub fn compose(self, child: Self) -> Self {
        Self {
            axes: child.axes.map(|a| self.vector(a)),
            origin: self.point(child.origin),
        }
    }
}

/// Pose products for skinning and bone attachments.
#[derive(Clone, Debug)]
pub struct Pose {
    /// Parent-relative transforms.
    pub locals: Vec<Transform>,
    /// Parent-relative scale.
    pub scales: Vec<Vec3>,
    /// Mesh-space bone coordinates.
    pub globals: Vec<Coords>,
}

/// Converts an Unreal rotator using the same yaw/pitch/roll order as common::unreal_rotator_matrix.
pub fn rotator_quat(r: [i32; 3]) -> Quat {
    let k = std::f32::consts::TAU / 65536.0;
    Quat::from_axis_angle(Vec3::new(0.0, 0.0, 1.0), r[1] as f32 * k)
        * Quat::from_axis_angle(Vec3::new(0.0, 1.0, 0.0), -r[0] as f32 * k)
        * Quat::from_axis_angle(Vec3::new(1.0, 0.0, 0.0), -r[2] as f32 * k)
}

fn interpolate(a: Transform, b: Transform, alpha: f32) -> Transform {
    Transform::new(
        a.rotation.slerp(b.rotation, alpha),
        a.translation.lerp(b.translation, alpha),
    )
}

fn sample<'a>(
    skeleton: &Skeleton,
    recipe: &Sample,
    lookup: &impl Fn(&str) -> Option<&'a Clip>,
    depth: usize,
) -> Result<Vec<Option<Transform>>, NormalizeError> {
    if depth > 64 || !recipe.frame.is_finite() || !recipe.tween_alpha.is_finite() {
        return Err(NormalizeError(
            "invalid or excessively nested tween recipe".into(),
        ));
    }
    let clip = lookup(&recipe.sequence)
        .ok_or_else(|| NormalizeError(format!("unresolved animation '{}'", recipe.sequence)))?;
    let previous = if recipe.tween_alpha < 1.0 {
        match &recipe.source {
            Some(s) => Some(sample(skeleton, s, lookup, depth + 1)?),
            None => Some(skeleton.bones.iter().map(|b| Some(b.bind_local)).collect()),
        }
    } else {
        None
    };
    let mut out = Vec::with_capacity(skeleton.bones.len());
    for (i, bone) in skeleton.bones.iter().enumerate() {
        let Some(track) = clip
            .tracks
            .iter()
            .find(|t| t.bone.eq_ignore_ascii_case(&bone.name))
        else {
            out.push(None);
            continue;
        };
        if track.times.is_empty()
            || track.rotations.len() != track.times.len()
            || !(track.positions.len() == 1 || track.positions.len() == track.times.len())
            || track.times.iter().any(|t| !t.is_finite())
            || track.times.windows(2).any(|w| w[0] > w[1])
        {
            return Err(NormalizeError(format!(
                "invalid track '{}.{}'",
                clip.name, bone.name
            )));
        }
        let target = track.sample(recipe.frame, clip.track_time, recipe.looping);
        let local = match &previous {
            Some(p) => interpolate(
                p[i].unwrap_or(bone.bind_local),
                target,
                recipe.tween_alpha.clamp(0.0, 1.0),
            ),
            None => target,
        };
        if !local.is_finite() {
            return Err(NormalizeError(format!(
                "nonfinite pose '{}.{}'",
                clip.name, bone.name
            )));
        }
        out.push(Some(local));
    }
    Ok(out)
}

/// Samples all channels, blends local poses, applies controls, then builds mesh-space coordinates.
/// Missing tracks leave lower stages intact. Invalid requests are reported, never bind-pose success.
pub fn evaluate<'a>(
    skeleton: &Skeleton,
    channels: &[Channel],
    controllers: &[Controller],
    lookup: impl Fn(&str) -> Option<&'a Clip>,
) -> Result<Pose, NormalizeError> {
    let mut locals: Vec<_> = skeleton.bones.iter().map(|b| b.bind_local).collect();
    let mut scales = vec![Vec3::new(1.0, 1.0, 1.0); locals.len()];
    for (i, b) in skeleton.bones.iter().enumerate() {
        if b.parent.is_some_and(|p| p >= i) {
            return Err(NormalizeError(format!(
                "invalid parent of bone '{}'",
                b.name
            )));
        }
    }
    let mut ordered: Vec<_> = channels.iter().collect();
    ordered.sort_by_key(|c| c.stage);
    if ordered.windows(2).any(|w| w[0].stage == w[1].stage) {
        return Err(NormalizeError("duplicate animation stage".into()));
    }
    for c in ordered {
        if !c.alpha.is_finite() || !c.in_time.is_finite() {
            return Err(NormalizeError("nonfinite channel blend".into()));
        }
        let sampled = sample(skeleton, &c.sample, &lookup, 0)?;
        let clip = lookup(&c.sample.sequence).ok_or_else(|| NormalizeError("lost clip".into()))?;
        let mut alpha = if c.stage == 0 { 1.0 } else { c.alpha };
        if c.stage != 0 && c.in_time > 0.0 {
            let fraction = if clip.num_frames == 0 {
                0.0
            } else {
                c.sample.frame / clip.num_frames as f32
            };
            alpha *= (fraction / c.in_time).clamp(0.0, 1.0);
        }
        let root = if c.stage == 0 {
            0
        } else {
            c.bone
                .as_deref()
                .and_then(|n| skeleton.find(n))
                .unwrap_or(0)
        };
        let mut mask = vec![false; locals.len()];
        for (i, b) in skeleton.bones.iter().enumerate() {
            mask[i] = i == root || b.parent.is_some_and(|p| mask[p]);
            if mask[i]
                && let Some(t) = sampled[i]
            {
                locals[i] = interpolate(locals[i], t, alpha.clamp(0.0, 1.0));
            }
        }
    }
    for c in controllers {
        let i = skeleton
            .find(&c.bone)
            .ok_or_else(|| NormalizeError(format!("unresolved controller bone '{}'", c.bone)))?;
        if let Some(s) = c.scale {
            if !s.is_finite() {
                return Err(NormalizeError("nonfinite bone scale".into()));
            }
            scales[i] = s;
        }
        if let Some((_, alpha)) = c.rotation
            && !alpha.is_finite()
        {
            return Err(NormalizeError("nonfinite rotation alpha".into()));
        }
        if let Some((r, alpha)) = c.rotation.filter(|(_, alpha)| *alpha > 0.0) {
            if !alpha.is_finite() {
                return Err(NormalizeError("nonfinite rotation alpha".into()));
            }
            locals[i].rotation = (locals[i].rotation
                * rotator_quat(r.map(|x| (x as f32 * alpha) as i32)))
            .normalized();
        } else if let Some((t, alpha)) = c.translation {
            // GetFrame chooses rotation if its alpha is positive, otherwise translation.
            if !t.is_finite() || !alpha.is_finite() {
                return Err(NormalizeError("nonfinite bone location".into()));
            }
            if alpha > 0.0 {
                locals[i].translation = locals[i].translation + t * alpha;
            }
        }
    }
    let mut globals: Vec<Coords> = Vec::with_capacity(locals.len());
    for (i, b) in skeleton.bones.iter().enumerate() {
        let local = Coords::new(locals[i], scales[i]);
        globals.push(match b.parent {
            Some(p) => globals[p].compose(local),
            None => local,
        });
    }
    Ok(Pose {
        locals,
        scales,
        globals,
    })
}

/// Attachment actor coordinates: mesh-to-world * evaluated bone * relative actor placement.
pub fn attachment(mesh: Coords, bone: Coords, relative: Transform) -> Coords {
    mesh.compose(bone)
        .compose(Coords::new(relative, Vec3::new(1.0, 1.0, 1.0)))
}

#[cfg(test)]
mod tests;
