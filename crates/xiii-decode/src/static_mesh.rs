//! `Engine.StaticMesh` payload for package version 100 (XIII licensees 57/58).
//!
//! UModel refuses static meshes below version 112 ("old version"), so this layout was
//! established from the bytes of the GOG corpus (see README for evidence and counts). Field
//! names follow UModel's `UStaticMesh` (UnMesh2.h/.cpp @a0bfb468) where the data matches;
//! everything else is named by role and documented as inferred.
//!
//! ```text
//! UPrimitive         FBox, FSphere, XIII extension (4 bytes + f32)
//! Sections           TArray<{i32 f4; u16 FirstIndex, FirstVertex, LastVertex, fE, NumFaces}>
//! BoundingBox        FBox (second copy)
//! VertexStream       TArray<{FVector Position; FVector Normal}>, i32 revision
//! UVStreams          TArray<{TArray<{f32 U, V}>; i32 channel; i32 unknown (=1)}>
//! IndexBuffer        TArray<u16>, i32 revision          (triangle list)
//! WireframeIndices   TArray<u16>, i32 revision          (line list)
//! Collision[2]       { TArray<FVector> vertices; i32 unknown;
//!                      TArray<{u16 v0, v1, v2; i16 material}> triangles;
//!                      TArray<{u16 triangle; i16 coplanar, front, back; u8[4] unknown}> nodes }
//!                    [0] = per-triangle collision, [1] = simplified collision (empty unless
//!                    the mesh has `SimplifiedColMaterial`)
//! SimpleRawTris      TLazyArray<FStaticMeshTriangle>   (source triangles of [1])
//! SimpleWireframe    TArray<u16>, i32 revision
//! SimpleMaterial     compact object ref
//! Unknown            5 x i32 (licensee 58; 4 x i32 for licensee 57) + 2 bytes; the
//!                    second-to-last i32 = total section faces in 6060/6128 meshes
//! RawTriangles       TLazyArray<FStaticMeshTriangle>   (editor source triangles)
//! InternalVersion    i32 (13; 12 in 68 meshes)
//! Materials          TArray<{compact Material; u8 EnableCollision; u8 unknown}>
//! ```
//!
//! There are **no** colour/alpha streams in this version (vertex lighting lives in the
//! map's `StaticMeshInstance` objects).

use xiii_package::{ObjectRef, Package};

use crate::common::{
    BoundingBox, DecodeError, DecodeErrorKind, DecodeResult, PayloadReader, PayloadReport,
    PrimitiveHeader, Props, read_properties,
};

/// Class path of static meshes.
pub const STATIC_MESH_CLASS: &str = "Engine.StaticMesh";

/// Render section: a contiguous range of the index buffer using one material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Section {
    /// Unknown (always 0 in the corpus; UModel `f4`).
    pub f4: i32,
    /// First index in the index buffer.
    pub first_index: u16,
    /// Lowest vertex used.
    pub first_vertex: u16,
    /// Highest vertex used.
    pub last_vertex: u16,
    /// Always equal to `num_faces` in the corpus (UModel `fE`).
    pub fe: u16,
    /// Triangle count.
    pub num_faces: u16,
}

/// Render vertex.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Vertex {
    /// Position (source coordinates).
    pub position: [f32; 3],
    /// Normal (source coordinates).
    pub normal: [f32; 3],
}

/// One UV channel.
#[derive(Debug, Clone, PartialEq)]
pub struct UvStream {
    /// Per-vertex UVs.
    pub uvs: Vec<[f32; 2]>,
    /// Channel number (0, 1, 2 observed in stream order).
    pub channel: i32,
    /// Unknown (always 1).
    pub unknown: i32,
}

/// Collision triangle.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollisionTriangle {
    /// Indices into [`CollisionSet::vertices`].
    pub vertices: [u16; 3],
    /// Material slot (-1 for simplified collision).
    pub material: i16,
}

/// Node of the collision BSP built over the triangles (one node per triangle).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CollisionNode {
    /// Triangle index.
    pub triangle: u16,
    /// Next coplanar node or -1.
    pub coplanar: i16,
    /// Front child or -1.
    pub front: i16,
    /// Back child or -1.
    pub back: i16,
    /// Four bytes with no stable meaning (values look like uninitialized padding).
    pub unknown: [u8; 4],
}

/// One collision representation.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct CollisionSet {
    /// Vertices (source coordinates).
    pub vertices: Vec<[f32; 3]>,
    /// Unknown i32 (0 for set 0; 1..12 for set 1).
    pub unknown: i32,
    /// Triangles.
    pub triangles: Vec<CollisionTriangle>,
    /// BSP nodes.
    pub nodes: Vec<CollisionNode>,
}

/// Editor/source triangle (`FStaticMeshTriangle`, layout as UModel's version >= 112 reader).
#[derive(Debug, Clone, PartialEq)]
pub struct RawTriangle {
    /// Corner positions.
    pub vertices: [[f32; 3]; 3],
    /// Per-channel corner UVs.
    pub uvs: Vec<[[f32; 2]; 3]>,
    /// Corner colours as stored.
    pub colors: [[u8; 4]; 3],
    /// Material index (-1 for simplified collision).
    pub material: i32,
    /// Smoothing mask.
    pub smoothing_mask: u32,
}

/// Material slot from the native tail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MeshMaterial {
    /// Material object.
    pub material: ObjectRef,
    /// Collision enabled for this slot.
    pub enable_collision: bool,
    /// Unknown byte (0 or 1).
    pub unknown: u8,
}

/// Decoded static mesh.
#[derive(Debug, Clone, PartialEq)]
pub struct StaticMesh {
    /// UPrimitive bounds.
    pub primitive: PrimitiveHeader,
    /// Render sections, one per material slot.
    pub sections: Vec<Section>,
    /// Second bounding box.
    pub bounding_box: BoundingBox,
    /// Render vertices.
    pub vertices: Vec<Vertex>,
    /// Vertex stream revision.
    pub vertex_revision: i32,
    /// UV channels.
    pub uv_streams: Vec<UvStream>,
    /// Triangle-list indices.
    pub indices: Vec<u16>,
    /// Line-list indices.
    pub wireframe_indices: Vec<u16>,
    /// Collision sets: `[0]` per triangle, `[1]` simplified.
    pub collision: [CollisionSet; 2],
    /// Source triangles of the simplified collision.
    pub simple_triangles: Vec<RawTriangle>,
    /// Line list of the simplified collision.
    pub simple_wireframe: Vec<u16>,
    /// Material reference of the simplified collision.
    pub simple_material: ObjectRef,
    /// Unknown block: i32 values and two bytes.
    /// Licensee 58: five i32 (`[3]` = face count in 6060/6127 meshes); licensee 57 (one
    /// mesh in the corpus, `XIIICamp.utx`): four i32 (face count at `[2]`).
    pub unknown_block: (Vec<i32>, [u8; 2]),
    /// Editor source triangles.
    pub raw_triangles: Vec<RawTriangle>,
    /// Internal version (13 or 12).
    pub internal_version: i32,
    /// Material slots (native copy).
    pub materials: Vec<MeshMaterial>,
    /// `SimplifiedColMaterial` property, if any.
    pub simplified_col_material_prop: Option<ObjectRef>,
    /// Byte accounting.
    pub report: PayloadReport,
}

fn invalid(r: &PayloadReader<'_>, field: &'static str, msg: String) -> DecodeError {
    DecodeError::at(DecodeErrorKind::Invalid(msg), r.pos()).in_field(field)
}

fn read_u16_buffer(
    r: &mut PayloadReader<'_>,
    field: &'static str,
) -> DecodeResult<(Vec<u16>, i32)> {
    let v = r.array(field, 2, |r| r.u16())?;
    let rev = r.i32().map_err(|e| e.in_field(field))?;
    Ok((v, rev))
}

fn read_collision(r: &mut PayloadReader<'_>) -> DecodeResult<CollisionSet> {
    let vertices = r.array("collision.vertices", 12, |r| r.vec3())?;
    let unknown = r.i32().map_err(|e| e.in_field("collision.unknown"))?;
    let triangles = r.array("collision.triangles", 8, |r| {
        Ok(CollisionTriangle {
            vertices: [r.u16()?, r.u16()?, r.u16()?],
            material: r.i16()?,
        })
    })?;
    let nodes = r.array("collision.nodes", 12, |r| {
        let triangle = r.u16()?;
        let coplanar = r.i16()?;
        let front = r.i16()?;
        let back = r.i16()?;
        let b = r.bytes(4)?;
        Ok(CollisionNode {
            triangle,
            coplanar,
            front,
            back,
            unknown: [b[0], b[1], b[2], b[3]],
        })
    })?;
    for t in &triangles {
        if t.vertices.iter().any(|&v| v as usize >= vertices.len()) {
            return Err(invalid(
                r,
                "collision.triangles",
                format!("vertex index {:?} >= {}", t.vertices, vertices.len()),
            ));
        }
    }
    let nn = nodes.len() as i32;
    for n in &nodes {
        let bad = |c: i16| c < -1 || i32::from(c) >= nn;
        if n.triangle as usize >= triangles.len() || bad(n.coplanar) || bad(n.front) || bad(n.back)
        {
            return Err(invalid(
                r,
                "collision.nodes",
                format!("node {n:?} out of range"),
            ));
        }
    }
    Ok(CollisionSet {
        vertices,
        unknown,
        triangles,
        nodes,
    })
}

fn read_raw_triangles(
    r: &mut PayloadReader<'_>,
    field: &'static str,
) -> DecodeResult<Vec<RawTriangle>> {
    // minimum: 3 vectors + uv count + colours + material + smoothing = 64 bytes
    let (n, skip) = r.lazy_array_header(field, 64)?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let vertices = [r.vec3()?, r.vec3()?, r.vec3()?];
        let at = r.pos();
        let nuv = r.i32().map_err(|e| e.in_field(field))?;
        if !(0..=8).contains(&nuv) {
            return Err(DecodeError::at(
                DecodeErrorKind::Invalid(format!("raw triangle UV count {nuv}")),
                at,
            )
            .in_field(field));
        }
        let mut uvs = Vec::with_capacity(nuv as usize);
        for _ in 0..nuv {
            uvs.push([
                [r.f32()?, r.f32()?],
                [r.f32()?, r.f32()?],
                [r.f32()?, r.f32()?],
            ]);
        }
        let c = r.bytes(12)?;
        let colors = [
            [c[0], c[1], c[2], c[3]],
            [c[4], c[5], c[6], c[7]],
            [c[8], c[9], c[10], c[11]],
        ];
        let material = r.i32()?;
        let smoothing_mask = r.u32()?;
        out.push(RawTriangle {
            vertices,
            uvs,
            colors,
            material,
            smoothing_mask,
        });
    }
    r.expect_at(field, skip)?;
    Ok(out)
}

/// Decodes an `Engine.StaticMesh` export.
pub fn decode_static_mesh(
    package: &Package,
    data: &[u8],
    export: usize,
) -> DecodeResult<StaticMesh> {
    let props = read_properties(package, data, export, STATIC_MESH_CLASS)?;
    let p = Props::new(package, &props);
    let simplified_col_material_prop = p.object("SimplifiedColMaterial");
    let ctx = |e: DecodeError| e.in_export(package, export);
    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    decode_body(package, &mut r, simplified_col_material_prop)
        .map_err(ctx)
        .and_then(|mut m| {
            m.report = r.finish(props.block.span.end).map_err(ctx)?;
            Ok(m)
        })
}

fn decode_body(
    package: &Package,
    r: &mut PayloadReader<'_>,
    simplified_col_material_prop: Option<ObjectRef>,
) -> DecodeResult<StaticMesh> {
    let primitive = PrimitiveHeader::read(r)?;
    let sections = r.array("sections", 14, |r| {
        Ok(Section {
            f4: r.i32()?,
            first_index: r.u16()?,
            first_vertex: r.u16()?,
            last_vertex: r.u16()?,
            fe: r.u16()?,
            num_faces: r.u16()?,
        })
    })?;
    let bounding_box = r.bbox().map_err(|e| e.in_field("bounding_box"))?;
    let vertices = r.array("vertices", 24, |r| {
        Ok(Vertex {
            position: r.vec3()?,
            normal: r.vec3()?,
        })
    })?;
    let vertex_revision = r.i32().map_err(|e| e.in_field("vertices.revision"))?;
    let uv_streams = r.array("uv_streams", 9, |r| {
        let uvs = r.array("uv_streams.uvs", 8, |r| Ok([r.f32()?, r.f32()?]))?;
        Ok(UvStream {
            uvs,
            channel: r.i32()?,
            unknown: r.i32()?,
        })
    })?;
    let (indices, _) = read_u16_buffer(r, "indices")?;
    let (wireframe_indices, _) = read_u16_buffer(r, "wireframe_indices")?;
    let collision = [read_collision(r)?, read_collision(r)?];
    let simple_triangles = read_raw_triangles(r, "simple_triangles")?;
    let (simple_wireframe, _) = read_u16_buffer(r, "simple_wireframe")?;
    let simple_material = r
        .object_ref(package)
        .map_err(|e| e.in_field("simple_material"))?;
    let int_count = if package.summary().licensee >= 58 {
        5
    } else {
        4
    };
    let mut ints = Vec::with_capacity(int_count);
    for _ in 0..int_count {
        ints.push(r.i32().map_err(|e| e.in_field("unknown_block"))?);
    }
    let tail = r.unknown("static_mesh.unknown_block_bytes", 2)?;
    let unknown_block = (ints, [tail[0], tail[1]]);
    let raw_triangles = read_raw_triangles(r, "raw_triangles")?;
    let internal_version = r.i32().map_err(|e| e.in_field("internal_version"))?;
    let materials = r.array("materials", 3, |r| {
        let material = r.object_ref(package)?;
        let enable_collision = r.u8()? != 0;
        let unknown = r.u8()?;
        Ok(MeshMaterial {
            material,
            enable_collision,
            unknown,
        })
    })?;

    // Cross-checks that make a misaligned read fail loudly.
    for uv in &uv_streams {
        if uv.uvs.len() != vertices.len() {
            return Err(invalid(
                r,
                "uv_streams",
                format!("{} UVs for {} vertices", uv.uvs.len(), vertices.len()),
            ));
        }
    }
    if indices.len() % 3 != 0 {
        return Err(invalid(r, "indices", format!("{} indices", indices.len())));
    }
    if let Some(&i) = indices.iter().find(|&&i| i as usize >= vertices.len()) {
        return Err(invalid(
            r,
            "indices",
            format!("index {i} >= {}", vertices.len()),
        ));
    }
    for s in &sections {
        let end = s.first_index as usize + 3 * s.num_faces as usize;
        if end > indices.len() {
            return Err(invalid(
                r,
                "sections",
                format!("section {s:?} beyond {} indices", indices.len()),
            ));
        }
    }
    Ok(StaticMesh {
        primitive,
        sections,
        bounding_box,
        vertices,
        vertex_revision,
        uv_streams,
        indices,
        wireframe_indices,
        collision,
        simple_triangles,
        simple_wireframe,
        simple_material,
        unknown_block,
        raw_triangles,
        internal_version,
        materials,
        simplified_col_material_prop,
        report: PayloadReport {
            payload: r.payload(),
            properties_end: 0,
            unknown: Vec::new(),
            unsupported_tail: None,
        },
    })
}

impl StaticMesh {
    /// Indices of section `i` (triangle list, source winding).
    pub fn section_indices(&self, i: usize) -> &[u16] {
        let s = &self.sections[i];
        let start = s.first_index as usize;
        &self.indices[start..start + 3 * s.num_faces as usize]
    }

    /// Fraction of render triangles whose numeric normal `(b-a) x (c-a)` in **source**
    /// coordinates points against the averaged stored vertex normal (clockwise front faces).
    /// Returns `(against, along, degenerate)`.
    pub fn winding_statistics(&self) -> (usize, usize, usize) {
        let (mut against, mut along, mut degenerate) = (0, 0, 0);
        for t in self.indices.as_chunks::<3>().0 {
            let v = [
                self.vertices[t[0] as usize],
                self.vertices[t[1] as usize],
                self.vertices[t[2] as usize],
            ];
            let n = crate::common::triangle_cross(v[0].position, v[1].position, v[2].position);
            let s: [f32; 3] =
                std::array::from_fn(|k| v[0].normal[k] + v[1].normal[k] + v[2].normal[k]);
            let dot: f32 = (0..3).map(|k| n[k] * s[k]).sum();
            let len = (0..3).map(|k| n[k] * n[k]).sum::<f32>().sqrt();
            if len < 1e-6 || dot.abs() < 1e-6 * len {
                degenerate += 1;
            } else if dot < 0.0 {
                against += 1;
            } else {
                along += 1;
            }
        }
        (against, along, degenerate)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_package::{Builder, Bytes, parse};

    /// Native payload of a synthetic one-quad mesh (layout of the module docs). `start` is
    /// the absolute payload offset (needed for the lazy-array skip offsets).
    fn quad(start: usize, index3: u16) -> Vec<u8> {
        let corners = [
            [32.0, 32.0, 1.0],
            [32.0, -32.0, 1.0],
            [-32.0, 32.0, 1.0],
            [-32.0, -32.0, 1.0],
        ];
        let bbox = |b: Bytes| b.v3([-32.0, -32.0, 1.0]).v3([32.0, 32.0, 1.0]).u8(1);
        let mut b = Bytes::default().c(0); // property block: None
        b = bbox(b).v3([0.0, 0.0, 1.0]).f32(45.25).raw(&[0; 8]);
        b = b.c(1).i32(0).u16(0).u16(0).u16(3).u16(2).u16(2);
        b = bbox(b).c(4);
        for c in corners {
            b = b.v3(c).v3([0.0, 0.0, 1.0]);
        }
        b = b.i32(5).c(1).c(4);
        for uv in [[0.0, 0.0], [0.0, 1.0], [1.0, 0.0], [1.0, 1.0]] {
            b = b.f32(uv[0]).f32(uv[1]);
        }
        b = b.i32(0).i32(1);
        b = b.c(6).u16(0).u16(1).u16(2).u16(2).u16(1).u16(index3).i32(5);
        b = b.c(0).i32(5);
        // collision set 0 and an empty set 1
        b = b.c(4);
        for c in corners {
            b = b.v3(c);
        }
        b = b
            .i32(0)
            .c(2)
            .u16(0)
            .u16(1)
            .u16(2)
            .i16(0)
            .u16(3)
            .u16(2)
            .u16(1)
            .i16(0);
        b = b
            .c(2)
            .u16(0)
            .i16(1)
            .i16(-1)
            .i16(-1)
            .raw(&[0xfe, 0xff, 0, 0]);
        b = b.u16(1).i16(-1).i16(-1).i16(-1).raw(&[0xfe, 0xff, 0, 0]);
        b = b.c(0).i32(3).c(0).c(0);
        // empty simplified raw triangles (lazy), wireframe, material, unknown block
        let skip = start + b.len() + 4 + 1;
        b = b.i32(skip as i32).c(0).c(0).i32(5).c(0);
        b = b.i32(0).i32(1).i32(0).i32(2).i32(0).u8(0).u8(0);
        // one raw triangle
        let tri_len = 36 + 4 + 24 + 12 + 8;
        let skip = start + b.len() + 4 + 1 + tri_len;
        b = b
            .i32(skip as i32)
            .c(1)
            .v3(corners[0])
            .v3(corners[1])
            .v3(corners[2])
            .i32(1);
        for v in [0.0, 0.0, 0.0, 1.0, 1.0, 0.0] {
            b = b.f32(v);
        }
        b = b.raw(&[0xff; 12]).i32(0).i32(1);
        b = b.i32(13).c(1).c(0).u8(1).u8(1);
        b.0
    }

    fn package(index3: u16, cut: usize) -> (Vec<u8>, usize) {
        let mut b = Builder::new();
        let i = b.export("StaticMesh", "Quad", quad(0, index3));
        let start = b.payload_offset(i);
        let mut payload = quad(start, index3);
        payload.truncate(payload.len() - cut);
        b.set_payload(i, payload);
        (b.build(), i)
    }

    #[test]
    fn synthetic_quad_decodes_exactly() {
        let (bytes, i) = package(3, 0);
        let p = parse(&bytes);
        let m = decode_static_mesh(&p, &bytes, i).unwrap();
        assert_eq!(m.vertices.len(), 4);
        assert_eq!(m.section_indices(0), &[0, 1, 2, 2, 1, 3]);
        assert_eq!(m.collision[0].triangles.len(), 2);
        assert_eq!(m.collision[0].nodes[0].coplanar, 1);
        assert_eq!(m.raw_triangles.len(), 1);
        assert_eq!(m.internal_version, 13);
        assert_eq!(m.materials.len(), 1);
        assert!(m.report.unsupported_tail.is_none());
        // Stored normals are +Z while (b-a)x(c-a) is -Z: clockwise source winding.
        assert_eq!(m.winding_statistics(), (2, 0, 0));
    }

    #[test]
    fn synthetic_quad_truncation_and_bad_index_fail_cleanly() {
        let full = quad(0, 3).len();
        for cut in 1..full - 1 {
            let (bytes, i) = package(3, cut);
            let p = parse(&bytes);
            assert!(
                decode_static_mesh(&p, &bytes, i).is_err(),
                "cut {cut} must fail"
            );
        }
        let (bytes, i) = package(9, 0);
        let p = parse(&bytes);
        let e = decode_static_mesh(&p, &bytes, i).unwrap_err();
        assert!(e.to_string().contains("index 9"), "{e}");
    }
}
