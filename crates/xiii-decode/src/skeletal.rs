//! Skeletal meshes and animation (M2b).
//!
//! Decodes `Engine.SkeletalMesh` and `Engine.MeshAnimation` exports of XIII packages (version
//! 100, ULodMesh version 1) into field-for-field raw structures ([`mesh::RawSkeletalMesh`],
//! [`anim::RawMeshAnimation`]) and then into renderer-independent products
//! ([`normalize::Skeleton`], [`normalize::SkinnedMesh`], [`normalize::AnimSet`]). Every decoder
//! consumes its payload exactly; leftover bytes, unknown layout variants and index violations
//! are errors carrying the class, export, field and absolute file offset.
//!
//! Layout evidence and references: crate README, section "Skeletal meshes and animation".

pub mod anim;
pub mod blend;
mod error;
pub mod math;
pub mod mesh;
pub mod normalize;
mod reader;
pub mod validate;

#[cfg(test)]
mod tests;

pub use anim::RawMeshAnimation;
pub use error::{SkelError, SkelErrorKind};
pub use mesh::RawSkeletalMesh;
pub use normalize::{AnimSet, Clip, Skeleton, SkinnedMesh};

use xiii_package::{Limits, Package};

/// Class path of skeletal meshes.
pub const SKELETAL_MESH_CLASS: &str = "Engine.SkeletalMesh";
/// Class path of mesh animations.
pub const MESH_ANIMATION_CLASS: &str = "Engine.MeshAnimation";

fn open<'a>(
    package: &'a Package,
    data: &'a [u8],
    export: usize,
    class_path: &str,
    class: &'static str,
) -> Result<reader::Reader<'a>, SkelError> {
    let base_err = |kind| SkelError {
        class,
        export: Some(export as u32),
        offset: None,
        field: "export",
        kind,
    };
    let found = package.export_class_path(export).unwrap_or("");
    if !found.eq_ignore_ascii_case(class_path) {
        return Err(base_err(SkelErrorKind::WrongClass {
            found: found.to_owned(),
        }));
    }
    let props = package
        .read_object_properties(data, export, &Limits::default())
        .map_err(|e| SkelError {
            offset: e.offset,
            field: "properties",
            ..base_err(SkelErrorKind::Package(e.to_string()))
        })?;
    let payload = package
        .export_payload(data, export)
        .map_err(|e| base_err(SkelErrorKind::Package(e.to_string())))?;
    reader::Reader::new(
        package,
        payload,
        props.payload.start,
        props.consumed(),
        class,
        Some(export as u32),
    )
}

/// Decodes one `Engine.SkeletalMesh` export. `data` must be the buffer `package` was parsed
/// from.
pub fn decode_skeletal_mesh(
    package: &Package,
    data: &[u8],
    export: usize,
) -> Result<RawSkeletalMesh, SkelError> {
    let mut r = open(package, data, export, SKELETAL_MESH_CLASS, "SkeletalMesh")?;
    mesh::read(&mut r)
}

/// Decodes one `Engine.MeshAnimation` export.
pub fn decode_mesh_animation(
    package: &Package,
    data: &[u8],
    export: usize,
) -> Result<RawMeshAnimation, SkelError> {
    let mut r = open(package, data, export, MESH_ANIMATION_CLASS, "MeshAnimation")?;
    anim::read(&mut r)
}

/// Decodes a skeletal-mesh payload that starts at `start` within `payload` (no package
/// tables needed except for name/object resolution). Used by the synthetic fixtures.
#[cfg(test)]
pub(crate) fn decode_skeletal_mesh_bytes(
    package: &Package,
    payload: &[u8],
    start: usize,
) -> Result<RawSkeletalMesh, SkelError> {
    let mut r = reader::Reader::new(package, payload, 0, start, "SkeletalMesh", None)?;
    mesh::read(&mut r)
}

/// Animation counterpart of `decode_skeletal_mesh_bytes`.
#[cfg(test)]
pub(crate) fn decode_mesh_animation_bytes(
    package: &Package,
    payload: &[u8],
    start: usize,
) -> Result<RawMeshAnimation, SkelError> {
    let mut r = reader::Reader::new(package, payload, 0, start, "MeshAnimation", None)?;
    anim::read(&mut r)
}
