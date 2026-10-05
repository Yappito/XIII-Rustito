//! Raw `Engine.SkeletalMesh` payload (XIII, package version 100, ULodMesh version 1).
//!
//! Layout verified on all 129 GOG instances (5 packages); see the crate README section
//! "Skeletal meshes and animation". Field order follows UModel `UnMesh2.cpp`/`UnMesh2.h`
//! (`UPrimitive`, `ULodMesh::Serialize`, `USkeletalMesh::Serialize`, Version <= 1 branch) at
//! gildor2/UEViewer@a0bfb468, with two XIII-specific differences measured on the bytes:
//! `FMeshBone` has no `NumChildren` field (53 bytes with a one-byte name index), and the
//! payload ends with an XIII-only array of `{FBox, u16 bone}` hit boxes instead of UModel's
//! third legacy array.

use xiii_package::ObjectRef;

use super::error::{SkelError, SkelErrorKind};
use super::reader::Reader;

/// `FBox`: min, max, valid flag.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundingBox {
    /// Minimum corner.
    pub min: [f32; 3],
    /// Maximum corner.
    pub max: [f32; 3],
    /// `IsValid` byte.
    pub valid: u8,
}

/// `FMeshFace` (legacy LOD face): three wedge indices and a material index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshFace {
    /// Wedge indices.
    pub wedges: [u16; 3],
    /// Index into [`RawSkeletalMesh::materials`].
    pub material: u16,
}

/// `FMeshWedge`: point index and UV (10 bytes on disk).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MeshWedge {
    /// Index into [`RawSkeletalMesh::points`].
    pub point: u16,
    /// Texture coordinates.
    pub uv: [f32; 2],
}

/// `FMeshMaterial`: poly flags and texture index.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshMaterial {
    /// Unreal poly flags.
    pub poly_flags: u32,
    /// Index into [`RawSkeletalMesh::textures`].
    pub texture_index: i32,
}

/// `FMeshBone` as stored by XIII: no `NumChildren` field.
#[derive(Debug, Clone, PartialEq)]
pub struct MeshBone {
    /// Bone name.
    pub name: String,
    /// Flags (0 or 1 observed).
    pub flags: u32,
    /// Stored orientation `(x, y, z, w)`; see [`super::normalize`] for the convention.
    pub orientation: [f32; 4],
    /// Position relative to the parent.
    pub position: [f32; 3],
    /// `VJointPos::Length` (always 0 observed).
    pub length: f32,
    /// `VJointPos::XSize/YSize/ZSize` (always 0 observed).
    pub size: [f32; 3],
    /// Parent index (0 for the root, which is its own parent).
    pub parent: i32,
}

/// `VWeightIndex`: vertices that all have `group + 1` influences.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeightIndex {
    /// Point indices.
    pub points: Vec<u16>,
    /// First index into [`RawSkeletalMesh::bone_influences`].
    pub start: i32,
}

/// `VBoneInfluence`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoneInfluence {
    /// 0..65535 maps to 0..1.
    pub weight: u16,
    /// Bone index.
    pub bone: u16,
}

/// XIII-only trailing element: a box around a bone (head/torso hit volumes in characters).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoneBox {
    /// Box in the bone's space (centered, e.g. +-11 for heads).
    pub bbox: BoundingBox,
    /// Mesh bone index. Stored as two bytes; read as `u16` (a compact index followed by a zero
    /// byte would be indistinguishable for indices below 64, the only values observed).
    pub bone: u16,
}

/// Decoded `Engine.SkeletalMesh` payload, field for field.
#[derive(Debug, Clone, PartialEq)]
pub struct RawSkeletalMesh {
    /// UPrimitive bounding box.
    pub bounding_box: BoundingBox,
    /// UPrimitive bounding sphere `(x, y, z, radius)`.
    pub bounding_sphere: [f32; 4],
    /// XIII UPrimitive extension (UModel: licensee >= 19): four bytes and a float.
    pub xiii_primitive_bytes: [u8; 4],
    /// XIII UPrimitive extension float.
    pub xiii_primitive_float: f32,
    /// ULodMesh version (only 1 is supported; the only value observed).
    pub lod_version: i32,
    /// ULodMesh vertex count (equals `points.len()`).
    pub vertex_count: i32,
    /// Packed vertex-animation verts (empty for skeletal meshes).
    pub packed_verts: Vec<u32>,
    /// Number of legacy `FMeshTri2` entries (38 bytes each; always 0 observed, skipped).
    pub legacy_tris: usize,
    /// Skins.
    pub textures: Vec<ObjectRef>,
    /// Mesh scale.
    pub mesh_scale: [f32; 3],
    /// Mesh origin.
    pub mesh_origin: [f32; 3],
    /// Rotation origin (pitch, yaw, roll in 65536ths of a turn).
    pub rot_origin: [i32; 3],
    /// Legacy u16 array present for version <= 1 (length = vertex count observed).
    pub legacy_u16: Vec<u16>,
    /// LOD face levels.
    pub face_level: Vec<u16>,
    /// Faces.
    pub faces: Vec<MeshFace>,
    /// LOD collapse table.
    pub collapse_wedge_thus: Vec<u16>,
    /// Wedges.
    pub wedges: Vec<MeshWedge>,
    /// Materials.
    pub materials: Vec<MeshMaterial>,
    /// `MeshScaleMax`.
    pub mesh_scale_max: f32,
    /// `LODHysteresis`.
    pub lod_hysteresis: f32,
    /// `LODStrength`.
    pub lod_strength: f32,
    /// `LODMinVerts`.
    pub lod_min_verts: i32,
    /// `LODMorph`.
    pub lod_morph: f32,
    /// `LODZDisplace`.
    pub lod_z_displace: f32,
    /// Reference-pose points (`Points2` in UModel's version-1 path).
    pub points: Vec<[f32; 3]>,
    /// Reference skeleton.
    pub ref_skeleton: Vec<MeshBone>,
    /// Default `MeshAnimation`.
    pub animation: ObjectRef,
    /// Skeletal depth.
    pub skeletal_depth: i32,
    /// Influence groups (group `i` = vertices with `i + 1` influences).
    pub weight_indices: Vec<WeightIndex>,
    /// Influences, consumed sequentially by the groups.
    pub bone_influences: Vec<BoneInfluence>,
    /// Attachment socket aliases.
    pub attach_aliases: Vec<String>,
    /// Attachment socket bone names.
    pub attach_bone_names: Vec<String>,
    /// Attachment `FCoords` (origin, x, y, z axes).
    pub attach_coords: Vec<[[f32; 3]; 4]>,
    /// Legacy `FLODMeshSection` entries (nine u16; one per material observed).
    pub lod_sections: Vec<[u16; 9]>,
    /// Second legacy section array (always empty observed; elements unsupported).
    pub lod_sections2: usize,
    /// XIII-only hit boxes.
    pub bone_boxes: Vec<BoneBox>,
}

fn bbox(r: &mut Reader<'_>, field: &'static str) -> Result<BoundingBox, SkelError> {
    Ok(BoundingBox {
        min: r.vec3(field)?,
        max: r.vec3(field)?,
        valid: r.u8(field)?,
    })
}

/// Decodes the class-native part of a SkeletalMesh payload (after the property block). The
/// reader must end exactly at the payload end.
pub(crate) fn read(r: &mut Reader<'_>) -> Result<RawSkeletalMesh, SkelError> {
    let bounding_box = bbox(r, "primitive.bounding_box")?;
    let s = r.quat("primitive.bounding_sphere")?;
    let mut xb = [0u8; 4];
    for b in &mut xb {
        *b = r.u8("primitive.xiii_bytes")?;
    }
    let xf = r.f32("primitive.xiii_float")?;
    let ver_pos = r.pos();
    let lod_version = r.i32("lod.version")?;
    if lod_version != 1 {
        return Err(r.err_at(
            "lod.version",
            ver_pos,
            SkelErrorKind::Unsupported(format!(
                "ULodMesh version {lod_version} (only 1 observed in XIII)"
            )),
        ));
    }
    let vertex_count = r.i32("lod.vertex_count")?;
    let packed_verts = r.array("lod.verts", 4, |r| r.u32("lod.verts"))?;
    let legacy_tris = r.count("lod.legacy_tris", 38)?;
    for _ in 0..legacy_tris {
        // FMeshTri2: u16[3], float UV[3][2], u32 flags, i32 texture index.
        for _ in 0..19 {
            r.u16("lod.legacy_tris")?;
        }
    }
    let textures = r.array("lod.textures", 1, |r| r.object("lod.textures"))?;
    let mesh_scale = r.vec3("lod.mesh_scale")?;
    let mesh_origin = r.vec3("lod.mesh_origin")?;
    let rot_origin = [
        r.i32("lod.rot_origin")?,
        r.i32("lod.rot_origin")?,
        r.i32("lod.rot_origin")?,
    ];
    let legacy_u16 = r.array("lod.legacy_u16", 2, |r| r.u16("lod.legacy_u16"))?;
    let face_level = r.array("lod.face_level", 2, |r| r.u16("lod.face_level"))?;
    let faces = r.array("lod.faces", 8, |r| {
        Ok(MeshFace {
            wedges: [
                r.u16("lod.faces")?,
                r.u16("lod.faces")?,
                r.u16("lod.faces")?,
            ],
            material: r.u16("lod.faces")?,
        })
    })?;
    let collapse_wedge_thus = r.array("lod.collapse_wedge_thus", 2, |r| {
        r.u16("lod.collapse_wedge_thus")
    })?;
    let wedges = r.array("lod.wedges", 10, |r| {
        Ok(MeshWedge {
            point: r.u16("lod.wedges")?,
            uv: [r.f32("lod.wedges")?, r.f32("lod.wedges")?],
        })
    })?;
    let materials = r.array("lod.materials", 8, |r| {
        Ok(MeshMaterial {
            poly_flags: r.u32("lod.materials")?,
            texture_index: r.i32("lod.materials")?,
        })
    })?;
    let mesh_scale_max = r.f32("lod.mesh_scale_max")?;
    let lod_hysteresis = r.f32("lod.hysteresis")?;
    let lod_strength = r.f32("lod.strength")?;
    let lod_min_verts = r.i32("lod.min_verts")?;
    let lod_morph = r.f32("lod.morph")?;
    let lod_z_displace = r.f32("lod.z_displace")?;

    let points = r.array("skel.points", 12, |r| r.vec3("skel.points"))?;
    let ref_skeleton = r.array("skel.ref_skeleton", 53, |r| {
        Ok(MeshBone {
            name: r.name("skel.bone.name")?,
            flags: r.u32("skel.bone.flags")?,
            orientation: r.quat("skel.bone.orientation")?,
            position: r.vec3("skel.bone.position")?,
            length: r.f32("skel.bone.length")?,
            size: r.vec3("skel.bone.size")?,
            parent: r.i32("skel.bone.parent")?,
        })
    })?;
    let animation = r.object("skel.animation")?;
    let skeletal_depth = r.i32("skel.depth")?;
    let weight_indices = r.array("skel.weight_indices", 5, |r| {
        Ok(WeightIndex {
            points: r.array("skel.weight_index.points", 2, |r| {
                r.u16("skel.weight_index.points")
            })?,
            start: r.i32("skel.weight_index.start")?,
        })
    })?;
    let bone_influences = r.array("skel.bone_influences", 4, |r| {
        Ok(BoneInfluence {
            weight: r.u16("skel.bone_influences")?,
            bone: r.u16("skel.bone_influences")?,
        })
    })?;
    let attach_aliases = r.array("skel.attach_aliases", 1, |r| r.name("skel.attach_aliases"))?;
    let attach_bone_names = r.array("skel.attach_bone_names", 1, |r| {
        r.name("skel.attach_bone_names")
    })?;
    let attach_coords = r.array("skel.attach_coords", 48, |r| {
        Ok([
            r.vec3("skel.attach_coords")?,
            r.vec3("skel.attach_coords")?,
            r.vec3("skel.attach_coords")?,
            r.vec3("skel.attach_coords")?,
        ])
    })?;
    let lod_sections = r.array("skel.lod_sections", 18, |r| {
        let mut s = [0u16; 9];
        for v in &mut s {
            *v = r.u16("skel.lod_sections")?;
        }
        Ok(s)
    })?;
    let s2_pos = r.pos();
    let lod_sections2 = r.count("skel.lod_sections2", 1)?;
    if lod_sections2 != 0 {
        return Err(r.err_at(
            "skel.lod_sections2",
            s2_pos,
            SkelErrorKind::Unsupported(format!(
                "{lod_sections2} entries in the second legacy section array (always empty in XIII)"
            )),
        ));
    }
    let bone_boxes = r.array("xiii.bone_boxes", 27, |r| {
        Ok(BoneBox {
            bbox: bbox(r, "xiii.bone_boxes")?,
            bone: r.u16("xiii.bone_boxes")?,
        })
    })?;
    r.finish("end")?;
    Ok(RawSkeletalMesh {
        bounding_box,
        bounding_sphere: s,
        xiii_primitive_bytes: xb,
        xiii_primitive_float: xf,
        lod_version,
        vertex_count,
        packed_verts,
        legacy_tris,
        textures,
        mesh_scale,
        mesh_origin,
        rot_origin,
        legacy_u16,
        face_level,
        faces,
        collapse_wedge_thus,
        wedges,
        materials,
        mesh_scale_max,
        lod_hysteresis,
        lod_strength,
        lod_min_verts,
        lod_morph,
        lod_z_displace,
        points,
        ref_skeleton,
        animation,
        skeletal_depth,
        weight_indices,
        bone_influences,
        attach_aliases,
        attach_bone_names,
        attach_coords,
        lod_sections,
        lod_sections2,
        bone_boxes,
    })
}
