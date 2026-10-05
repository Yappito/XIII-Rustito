//! Minimal vector/quaternion/rigid-transform math for skeleton evaluation.
//!
//! Values stay in the source (Unreal, Z-up) coordinate space; conversion to the runtime space is
//! a separate policy (see `docs/DESIGN.md`). Quaternions are Hamilton `(x, y, z, w)` acting on
//! column vectors (`v' = q v q*`). Candidates for a shared `common` math module later.

use std::ops::{Add, Mul, Neg, Sub};

/// 3-component vector.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Vec3 {
    /// X.
    pub x: f32,
    /// Y.
    pub y: f32,
    /// Z.
    pub z: f32,
}

impl Vec3 {
    /// Zero vector.
    pub const ZERO: Self = Self::new(0.0, 0.0, 0.0);

    /// Creates a vector.
    pub const fn new(x: f32, y: f32, z: f32) -> Self {
        Self { x, y, z }
    }

    /// From an array.
    pub const fn from_array(a: [f32; 3]) -> Self {
        Self::new(a[0], a[1], a[2])
    }

    /// To an array.
    pub const fn to_array(self) -> [f32; 3] {
        [self.x, self.y, self.z]
    }

    /// Dot product.
    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z
    }

    /// Cross product.
    pub fn cross(self, o: Self) -> Self {
        Self::new(
            self.y * o.z - self.z * o.y,
            self.z * o.x - self.x * o.z,
            self.x * o.y - self.y * o.x,
        )
    }

    /// Euclidean length.
    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// Unit vector, or zero for a (near) zero vector.
    pub fn normalized(self) -> Self {
        let l = self.length();
        if l > 1e-12 {
            self * (1.0 / l)
        } else {
            Self::ZERO
        }
    }

    /// Component-wise minimum.
    pub fn min(self, o: Self) -> Self {
        Self::new(self.x.min(o.x), self.y.min(o.y), self.z.min(o.z))
    }

    /// Component-wise maximum.
    pub fn max(self, o: Self) -> Self {
        Self::new(self.x.max(o.x), self.y.max(o.y), self.z.max(o.z))
    }

    /// Linear interpolation.
    pub fn lerp(self, o: Self, t: f32) -> Self {
        self + (o - self) * t
    }

    /// True when all components are finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite()
    }
}

impl Add for Vec3 {
    type Output = Self;
    fn add(self, o: Self) -> Self {
        Self::new(self.x + o.x, self.y + o.y, self.z + o.z)
    }
}

impl Sub for Vec3 {
    type Output = Self;
    fn sub(self, o: Self) -> Self {
        Self::new(self.x - o.x, self.y - o.y, self.z - o.z)
    }
}

impl Mul<f32> for Vec3 {
    type Output = Self;
    fn mul(self, s: f32) -> Self {
        Self::new(self.x * s, self.y * s, self.z * s)
    }
}

impl Neg for Vec3 {
    type Output = Self;
    fn neg(self) -> Self {
        Self::new(-self.x, -self.y, -self.z)
    }
}

/// Rotation quaternion `(x, y, z, w)`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Quat {
    /// X.
    pub x: f32,
    /// Y.
    pub y: f32,
    /// Z.
    pub z: f32,
    /// W (scalar part).
    pub w: f32,
}

impl Default for Quat {
    fn default() -> Self {
        Self::IDENTITY
    }
}

impl Quat {
    /// Identity rotation.
    pub const IDENTITY: Self = Self::new(0.0, 0.0, 0.0, 1.0);

    /// Creates a quaternion (not normalized).
    pub const fn new(x: f32, y: f32, z: f32, w: f32) -> Self {
        Self { x, y, z, w }
    }

    /// From `[x, y, z, w]`.
    pub const fn from_array(a: [f32; 4]) -> Self {
        Self::new(a[0], a[1], a[2], a[3])
    }

    /// To `[x, y, z, w]`.
    pub const fn to_array(self) -> [f32; 4] {
        [self.x, self.y, self.z, self.w]
    }

    /// Rotation of `angle` radians about a unit `axis`.
    pub fn from_axis_angle(axis: Vec3, angle: f32) -> Self {
        let (s, c) = (angle * 0.5).sin_cos();
        let a = axis.normalized();
        Self::new(a.x * s, a.y * s, a.z * s, c)
    }

    /// Conjugate (inverse for unit quaternions).
    pub fn conjugate(self) -> Self {
        Self::new(-self.x, -self.y, -self.z, self.w)
    }

    /// 4D dot product.
    pub fn dot(self, o: Self) -> f32 {
        self.x * o.x + self.y * o.y + self.z * o.z + self.w * o.w
    }

    /// Norm.
    pub fn length(self) -> f32 {
        self.dot(self).sqrt()
    }

    /// Unit quaternion; identity for a (near) zero quaternion.
    pub fn normalized(self) -> Self {
        let l = self.length();
        if l > 1e-12 {
            let s = 1.0 / l;
            Self::new(self.x * s, self.y * s, self.z * s, self.w * s)
        } else {
            Self::IDENTITY
        }
    }

    /// Rotates a vector (`q v q*`; assumes a unit quaternion).
    pub fn rotate(self, v: Vec3) -> Vec3 {
        let u = Vec3::new(self.x, self.y, self.z);
        let t = u.cross(v) * 2.0;
        v + t * self.w + u.cross(t)
    }

    /// Normalized linear/spherical interpolation along the shorter arc.
    pub fn slerp(self, o: Self, t: f32) -> Self {
        let mut d = self.dot(o);
        let o = if d < 0.0 {
            d = -d;
            Self::new(-o.x, -o.y, -o.z, -o.w)
        } else {
            o
        };
        let (a, b) = if d > 0.9995 {
            (1.0 - t, t)
        } else {
            let theta = d.clamp(-1.0, 1.0).acos();
            let s = theta.sin();
            (((1.0 - t) * theta).sin() / s, (t * theta).sin() / s)
        };
        Self::new(
            self.x * a + o.x * b,
            self.y * a + o.y * b,
            self.z * a + o.z * b,
            self.w * a + o.w * b,
        )
        .normalized()
    }

    /// True when all components are finite.
    pub fn is_finite(self) -> bool {
        self.x.is_finite() && self.y.is_finite() && self.z.is_finite() && self.w.is_finite()
    }
}

impl Mul for Quat {
    type Output = Self;
    /// Hamilton product: `(a * b).rotate(v) == a.rotate(b.rotate(v))`.
    fn mul(self, b: Self) -> Self {
        let a = self;
        Self::new(
            a.w * b.x + a.x * b.w + a.y * b.z - a.z * b.y,
            a.w * b.y - a.x * b.z + a.y * b.w + a.z * b.x,
            a.w * b.z + a.x * b.y - a.y * b.x + a.z * b.w,
            a.w * b.w - a.x * b.x - a.y * b.y - a.z * b.z,
        )
    }
}

/// Rigid transform: rotate, then translate (`p' = r p + t`). Skeletal data has no scale.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Transform {
    /// Rotation.
    pub rotation: Quat,
    /// Translation.
    pub translation: Vec3,
}

impl Transform {
    /// Identity.
    pub const IDENTITY: Self = Self {
        rotation: Quat::IDENTITY,
        translation: Vec3::ZERO,
    };

    /// Creates a transform.
    pub fn new(rotation: Quat, translation: Vec3) -> Self {
        Self {
            rotation,
            translation,
        }
    }

    /// Applies to a point.
    pub fn transform_point(&self, p: Vec3) -> Vec3 {
        self.rotation.rotate(p) + self.translation
    }

    /// Applies to a direction (rotation only).
    pub fn transform_vector(&self, v: Vec3) -> Vec3 {
        self.rotation.rotate(v)
    }

    /// Composition: `(self * child).transform_point(p) == self.transform_point(child.transform_point(p))`.
    pub fn mul(&self, child: &Self) -> Self {
        Self {
            rotation: (self.rotation * child.rotation).normalized(),
            translation: self.transform_point(child.translation),
        }
    }

    /// Inverse of a rigid transform.
    pub fn inverse(&self) -> Self {
        let r = self.rotation.conjugate();
        Self {
            rotation: r,
            translation: -r.rotate(self.translation),
        }
    }

    /// True when rotation and translation are finite.
    pub fn is_finite(&self) -> bool {
        self.rotation.is_finite() && self.translation.is_finite()
    }
}
