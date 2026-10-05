//! `Engine.Model` (BSP) and `Engine.Polys` for package version 100.
//!
//! No reference reader handles XIII's version-100 BSP, so the layout was established from
//! Plage00/Plage01 bytes (see README). Decoded part of `Model` (after the property block):
//!
//! ```text
//! UPrimitive        FBox, FSphere, XIII extension (4 bytes + f32)
//! Vectors           TArray<FVector>              (normals and texture axes)
//! Points            TArray<FVector>
//! Nodes             TArray<FBspNode>, 70 bytes each:
//!                     FPlane plane; u64 zone mask; u8 flags; i32 iVertPool;
//!                     i16 iSurf, iBack, iFront, iPlane (coplanar), iCollisionBound(?),
//!                         iRenderBound(?), unknown;
//!                     FSphere bound; u8 iZone[2]; u8 NumVertices;
//!                     i16 iLeaf[2]; i16 unknown; u16 first render vertex(?)
//! Surfs             TArray<FBspSurf>:
//!                     compact Material; u32 PolyFlags;
//!                     i16 pBase, vNormal, vTextureU, vTextureV, iLightMap, iBrushPoly;
//!                     u8 unknown (0 or 255); compact Actor (a Brush); FPlane
//! Verts             TArray<{u16 pVertex; i16 iSide}>
//! NumSharedSides    i32
//! NumZones          i32
//! reserved          u32 (always 0 in the corpus; meaning unknown)
//! Zones             NumZones x { compact ZoneActor; u64 Connectivity; u64 Visibility;
//!                                 f32 LastRenderTime }
//! Polys             compact object reference (an Engine.Polys export in the corpus)
//! ```
//!
//! The zone record is variable-length because `ZoneActor` is a compact object index (1 to 5
//! bytes); the remaining 20 bytes per zone are fixed. `Connectivity` is proven to be a zone
//! bitmask: every zone `z` has bit `z` set and no bit at or above `NumZones` is set, in all
//! 1,350 zone records of the 64 zoned GOG maps. The position and size of the following
//! `Visibility` (u64) and `LastRenderTime` (f32) are forced by the record size, but their
//! semantics are **not** verified (Visibility is not itself a `NumZones`-bit bitmask).
//!
//! The tail after `Polys` was decoded from `UModel::Serialize` in `Engine.dll` (RVA 0x9C240)
//! and cross-checked on the GOG corpus: `LightMap` (`TArray<FLightMapIndex>`), a second
//! per-lightmap byte array, `Bounds` (`TArray<FBox>`, 25 B), `LeafHulls` (`TArray<i32>`),
//! `Leaves` (`TArray<FConvexVolumeLeaf>`), `Lights` (`TArray<ref>`), `RootOutside`, `Linked`,
//! `MoverLink` and the 32-byte `FBspVertexStream` plus its revision. For 6,396 of the 7,194
//! Models the payload is consumed exactly to the byte. The remaining build variants (all
//! seven `Engine.Model` brushes with a non-empty `LightMap` array in the map packages, plus a
//! few others) use a different lightmap-record layout; those keep the decoded prefix and
//! report the remainder as the explicit label `model.after_linked`.
//!
//! The lightmap texels themselves are **not** decoded: the `LightMap` array is empty in
//! 7,192 of 7,194 Models and the meaning of `FLightMapIndex.DataOffset` relative to the
//! texture data is not established. Zone records do not have a uniform size, which is why
//! earlier revisions stopped at `NumZones`.
//!
//! `Polys` is decoded completely: `i32 Num, i32 Max`, then per polygon: compact vertex
//! count, Base, Normal, TextureU, TextureV, vertices, u32 PolyFlags, compact Actor, compact
//! Material, compact ItemName (name index), compact iLink, compact iBrushPoly.

pub mod level;

use std::fmt::Write as _;

use xiii_package::{ObjectRef, Package, PropertyValue};

use crate::common::{
    BoundingBox, DecodeError, DecodeErrorKind, DecodeResult, PayloadReader, PayloadReport,
    PrimitiveHeader, read_properties,
};

/// Class path of BSP models.
pub const MODEL_CLASS: &str = "Engine.Model";
/// Class path of polygon lists.
pub const POLYS_CLASS: &str = "Engine.Polys";

/// `PolyFlags` bits used by the viewer/collision notes (UE1/UE2 values).
pub mod poly_flags {
    /// Not rendered.
    pub const INVISIBLE: u32 = 0x0000_0001;
    /// Masked texture (palette index 0 / alpha test).
    pub const MASKED: u32 = 0x0000_0002;
    /// Translucent.
    pub const TRANSLUCENT: u32 = 0x0000_0004;
    /// Not solid: no collision.
    pub const NOT_SOLID: u32 = 0x0000_0008;
    /// Semi-solid brush.
    pub const SEMISOLID: u32 = 0x0000_0020;
    /// Modulated.
    pub const MODULATED: u32 = 0x0000_0040;
    /// Sky ("fake backdrop") surface.
    pub const FAKE_BACKDROP: u32 = 0x0000_0080;
    /// Two-sided.
    pub const TWO_SIDED: u32 = 0x0000_0100;
    /// Zone portal.
    pub const PORTAL: u32 = 0x0400_0000;
}

/// BSP node.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BspNode {
    /// Splitting plane (normal, distance: `dot(n, p) = w`).
    pub plane: [f32; 4],
    /// Zone mask.
    pub zone_mask: u64,
    /// Node flags.
    pub flags: u8,
    /// First entry in [`Model::verts`].
    pub vert_pool: i32,
    /// Surface index.
    pub surf: i16,
    /// Back child (-1 none).
    pub back: i16,
    /// Front child (-1 none).
    pub front: i16,
    /// Next coplanar node (-1 none).
    pub coplanar: i16,
    /// Collision bound index (meaning inferred from position).
    pub collision_bound: i16,
    /// Render bound index (meaning inferred from position).
    pub render_bound: i16,
    /// Unknown i16 (set on nodes with children).
    pub unknown_a: i16,
    /// Bounding sphere.
    pub sphere: [f32; 4],
    /// Zones on each side.
    pub zone: [u8; 2],
    /// Polygon vertex count.
    pub num_vertices: u8,
    /// Leaves on each side.
    pub leaf: [i16; 2],
    /// Unknown i16 (0 in the inspected nodes).
    pub unknown_b: i16,
    /// Increases by NumVertices between consecutive nodes: likely a render vertex offset.
    pub first_vertex: u16,
}

/// BSP surface.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BspSurf {
    /// Material object.
    pub material: ObjectRef,
    /// Poly flags.
    pub poly_flags: u32,
    /// Texture origin (index into points).
    pub base: i16,
    /// Normal (index into vectors).
    pub normal: i16,
    /// Texture U axis (index into vectors; length = texels per unit).
    pub texture_u: i16,
    /// Texture V axis (index into vectors).
    pub texture_v: i16,
    /// Lightmap index (-1 none).
    pub light_map: i16,
    /// Brush polygon index.
    pub brush_poly: i16,
    /// Unknown byte (0 or 255).
    pub unknown: u8,
    /// Brush actor that produced the surface.
    pub actor: ObjectRef,
    /// Plane.
    pub plane: [f32; 4],
}

/// Vertex-pool entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BspVert {
    /// Index into [`Model::points`].
    pub point: u16,
    /// Shared-side index.
    pub side: i16,
}

/// One BSP zone (`FZoneProperties`): the `ZoneInfo` actor and its masks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Zone {
    /// Zone actor (`ZoneInfo`/`SkyZoneInfo`/`WarpZoneInfo` or a subclass); null for zone 0 in
    /// every inspected map.
    pub actor: ObjectRef,
    /// Zone connectivity bitmask. Verified: zone `z` has bit `z`, bits >= `NumZones` are clear.
    pub connectivity: u64,
    /// Second mask. Position and size verified by the record length; meaning not established.
    pub visibility: u64,
    /// Trailing `f32` in the 20-byte zone record. Position verified; meaning not established
    /// (upstream UE2 serializes `LastRenderTime` only above a version gate).
    pub last_render_time: f32,
}

/// Decoded (prefix of a) BSP model.
#[derive(Debug, Clone, PartialEq)]
pub struct Model {
    /// UPrimitive fields.
    pub primitive: PrimitiveHeader,
    /// Vectors.
    pub vectors: Vec<[f32; 3]>,
    /// Points.
    pub points: Vec<[f32; 3]>,
    /// Nodes.
    pub nodes: Vec<BspNode>,
    /// Surfaces.
    pub surfs: Vec<BspSurf>,
    /// Vertex pool.
    pub verts: Vec<BspVert>,
    /// Shared side count.
    pub num_shared_sides: i32,
    /// Zone count.
    pub num_zones: i32,
    /// Reserved `u32` between `NumZones` and the zone records (always 0 in the corpus).
    pub reserved: u32,
    /// Zone records (length equals a non-negative `num_zones`).
    pub zones: Vec<Zone>,
    /// `Polys` object reference stored after the zone records.
    pub polys: Option<ObjectRef>,
    /// Per-surface lightmap indices (`FLightMapIndex`), decoded after `Polys`.
    pub light_maps: Vec<LightMapIndex>,
    /// Per-surface light bits, one byte per node reference (`TArray<u8>`).
    pub light_bits: Vec<u8>,
    /// Node bounds (`FBox`, 25 bytes each).
    pub bounds: Vec<BoundingBox>,
    /// `LeafHulls` (`TArray<i32>`).
    pub leaf_hulls: Vec<i32>,
    /// Per-leaf convex-volume records (`FConvexVolumeLeaf`).
    pub leaves: Vec<ConvexVolumeLeaf>,
    /// `Lights` actor references.
    pub lights: Vec<ObjectRef>,
    /// `RootOutside` flag serialized after `Lights`.
    pub root_outside: i32,
    /// `Linked` flag serialized after `Lights`.
    pub linked: i32,
    /// `MoverLink` (third i32 after `Lights`).
    pub mover_link: i32,
    /// BSP vertex stream (`TArray<FBspVertex>`, 32 bytes each): position, normal, two UV pairs.
    pub vertex_stream: Vec<[f32; 8]>,
    /// Revision i32 that follows the vertex stream.
    pub vertex_stream_revision: i32,
    /// Byte accounting (with the unsupported tail).
    pub report: PayloadReport,
}

/// One `FLightMapIndex` entry (`TArray<FLightMapIndex>` after `Polys`).
///
/// The engine stores each entry as a fixed 178-byte record (verified on the 13 GOG Models with
/// a non-empty array; see the task report). The field names follow the upstream `FLightMapIndex`
/// (`DataOffset`, `PanX/Y/Z`, `UClamp`, `VClamp`, `UScale`, `VScale`, `LightActors`) plus the
/// matrices and per-bitmap fields the XIII build serializes.
#[derive(Debug, Clone, PartialEq)]
pub struct LightMapIndex {
    /// First i32 field (upstream `DataOffset`); meaning not established.
    pub data_offset: i32,
    /// Second i32 field (upstream `PanX`); meaning not established.
    pub pan: f32,
    /// First 4x4 matrix (world to lightmap), row-major.
    pub matrix_a: [f32; 16],
    /// Second 4x4 matrix, row-major.
    pub matrix_b: [f32; 16],
    /// Nine i32 fields (counts and the `UClamp`/`VClamp` group); one is always 0x01000000.
    pub ints: [i32; 9],
    /// Two bytes (0xac/0xad); value 1 and 0 in the inspected records.
    pub unknown: [u8; 2],
    /// Four trailing compact indices; meanings not established.
    pub tail_indices: [i32; 4],
}

/// One `FConvexVolumeLeaf` entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConvexVolumeLeaf {
    /// Zone index.
    pub zone: i32,
    /// Permeating flag.
    pub permeating: i32,
    /// Volumetric flag.
    pub volumetric: i32,
    /// Visible-zone bitmask.
    pub visible_zones: u64,
}

/// Polygon from `Engine.Polys`.
#[derive(Debug, Clone, PartialEq)]
pub struct Poly {
    /// Texture origin.
    pub base: [f32; 3],
    /// Normal.
    pub normal: [f32; 3],
    /// Texture U axis.
    pub texture_u: [f32; 3],
    /// Texture V axis.
    pub texture_v: [f32; 3],
    /// Vertices.
    pub vertices: Vec<[f32; 3]>,
    /// Poly flags.
    pub poly_flags: u32,
    /// Actor.
    pub actor: ObjectRef,
    /// Material.
    pub material: ObjectRef,
    /// Item name (name-table index).
    pub item_name: i32,
    /// Link.
    pub link: i32,
    /// Brush poly.
    pub brush_poly: i32,
}

/// Decoded polygon list.
#[derive(Debug, Clone, PartialEq)]
pub struct Polys {
    /// Stored `Max` (capacity).
    pub max: i32,
    /// Polygons.
    pub polys: Vec<Poly>,
    /// Byte accounting.
    pub report: PayloadReport,
}

fn invalid(at: usize, field: &'static str, msg: String) -> DecodeError {
    DecodeError::at(DecodeErrorKind::Invalid(msg), at).in_field(field)
}

/// Short text of a reference for summaries: `Class'Path'` for exports and imports, `None`
/// for null and no object.
fn ref_text(package: &Package, r: Option<ObjectRef>) -> String {
    match r {
        None | Some(ObjectRef::Null) => "None".to_owned(),
        Some(ObjectRef::Import(i)) => {
            let class = package
                .imports()
                .get(i as usize)
                .map_or("?", |o| package.name(o.class_name));
            format!(
                "{class}'{}'",
                package.object_path(ObjectRef::Import(i)).unwrap_or("?")
            )
        }
        Some(ObjectRef::Export(i)) => {
            let class = package.export_class_path(i as usize).unwrap_or("?");
            let class = class.rsplit('.').next().unwrap_or(class);
            format!(
                "{class}'{}' (export {i})",
                package.object_path(ObjectRef::Export(i)).unwrap_or("?")
            )
        }
    }
}

/// Decodes a Model including the lightmap/lights tail; any bytes after `Linked` that the
/// serializer writes for this build but whose layout is not established remain an explicit,
/// labelled unsupported tail.
pub fn decode_model(package: &Package, data: &[u8], export: usize) -> DecodeResult<Model> {
    let props = read_properties(package, data, export, MODEL_CLASS)?;
    let ctx = |e: DecodeError| e.in_export(package, export);
    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    let mut m = decode_model_body(package, &mut r).map_err(ctx)?;
    // The tail is decoded best-effort: a build variant whose element layout differs keeps the
    // decoded prefix and reports the remainder as a labelled unsupported tail rather than
    // failing the whole export.
    let tail_start = r.pos();
    match decode_model_tail(package, &mut r) {
        Ok(t) => {
            m.light_maps = t.light_maps;
            m.light_bits = t.light_bits;
            m.bounds = t.bounds;
            m.leaf_hulls = t.leaf_hulls;
            m.leaves = t.leaves;
            m.lights = t.lights;
            m.root_outside = t.root_outside;
            m.linked = t.linked;
            m.mover_link = t.mover_link;
            m.vertex_stream = t.vertex_stream;
            m.vertex_stream_revision = t.vertex_stream_revision;
        }
        // A build variant whose tail layout differs surfaces as a count that does not fit, an
        // out-of-range reference, or a compact index that overflows (`BadCount`/`Invalid`/
        // package `CompactIndexOverflow`); those keep the decoded prefix and become the
        // labelled unsupported tail. Any other package failure (e.g. a genuine EOF) is a real
        // error and is propagated rather than hidden.
        Err(e) if is_tail_variant(&e) => {
            r.seek(tail_start).map_err(ctx)?;
        }
        Err(e) => return Err(ctx(e)),
    }
    let report = r
        .finish_with_unsupported_tail("model.after_linked", props.block.span.end)
        .map_err(ctx)?;
    Ok(Model { report, ..m })
}

/// True when a tail error indicates a build variant whose element layout is not established
/// (a count that cannot fit, an out-of-range reference, an overflowing compact index, or a
/// tail that runs past the export's declared serialized size). The caller keeps the decoded
/// prefix and reports the remainder as an explicit labelled unsupported tail, so no bytes are
/// silently dropped. Other package failures (e.g. a malformed compact index stream at a known
/// offset) are propagated.
fn is_tail_variant(e: &DecodeError) -> bool {
    match &e.kind {
        DecodeErrorKind::BadCount { .. } | DecodeErrorKind::Invalid(_) => true,
        DecodeErrorKind::Package(p) => matches!(
            p.kind,
            xiii_package::ErrorKind::CompactIndexOverflow
                | xiii_package::ErrorKind::CompactIndexOutOfRange { .. }
                | xiii_package::ErrorKind::UnexpectedEof { .. }
        ),
        _ => false,
    }
}

fn decode_model_body(package: &Package, r: &mut PayloadReader<'_>) -> DecodeResult<Model> {
    let primitive = PrimitiveHeader::read(r)?;
    let vectors = r.array("vectors", 12, |r| r.vec3())?;
    let points = r.array("points", 12, |r| r.vec3())?;
    let nodes = r.array("nodes", 70, |r| {
        let plane = r.vec4()?;
        let zl = r.u32()?;
        let zh = r.u32()?;
        let flags = r.u8()?;
        let vert_pool = r.i32()?;
        let s = [
            r.i16()?,
            r.i16()?,
            r.i16()?,
            r.i16()?,
            r.i16()?,
            r.i16()?,
            r.i16()?,
        ];
        let sphere = r.vec4()?;
        let zone = [r.u8()?, r.u8()?];
        let num_vertices = r.u8()?;
        let leaf = [r.i16()?, r.i16()?];
        let unknown_b = r.i16()?;
        let first_vertex = r.u16()?;
        Ok(BspNode {
            plane,
            zone_mask: u64::from(zl) | (u64::from(zh) << 32),
            flags,
            vert_pool,
            surf: s[0],
            back: s[1],
            front: s[2],
            coplanar: s[3],
            collision_bound: s[4],
            render_bound: s[5],
            unknown_a: s[6],
            sphere,
            zone,
            num_vertices,
            leaf,
            unknown_b,
            first_vertex,
        })
    })?;
    let surfs = r.array("surfs", 1 + 4 + 12 + 1 + 1 + 16, |r| {
        let material = r.object_ref(package)?;
        let poly_flags = r.u32()?;
        let base = r.i16()?;
        let normal = r.i16()?;
        let texture_u = r.i16()?;
        let texture_v = r.i16()?;
        let light_map = r.i16()?;
        let brush_poly = r.i16()?;
        let unknown = r.u8()?;
        let actor = r.object_ref(package)?;
        let plane = r.vec4()?;
        Ok(BspSurf {
            material,
            poly_flags,
            base,
            normal,
            texture_u,
            texture_v,
            light_map,
            brush_poly,
            unknown,
            actor,
            plane,
        })
    })?;
    let verts = r.array("verts", 4, |r| {
        Ok(BspVert {
            point: r.u16()?,
            side: r.i16()?,
        })
    })?;
    let num_shared_sides = r.i32().map_err(|e| e.in_field("num_shared_sides"))?;
    let at = r.pos();
    let num_zones = r.i32().map_err(|e| e.in_field("num_zones"))?;
    if !(0..=64).contains(&num_zones) {
        return Err(invalid(at, "num_zones", format!("{num_zones}")));
    }
    let reserved = r.u32().map_err(|e| e.in_field("reserved"))?;
    let zones = decode_zones(package, r, num_zones)?;
    let polys = decode_polys_ref(package, r)?;
    // Cross-checks: every node polygon must reference valid verts/points/surfs.
    let at = r.pos();
    for (i, n) in nodes.iter().enumerate() {
        let nn = nodes.len() as i32;
        let child_ok = |c: i16| c == -1 || (0..nn).contains(&i32::from(c));
        if !child_ok(n.back) || !child_ok(n.front) || !child_ok(n.coplanar) {
            return Err(invalid(
                at,
                "nodes",
                format!("node {i} child out of range: {n:?}"),
            ));
        }
        if n.num_vertices > 0 {
            let start = n.vert_pool as usize;
            let end = start + n.num_vertices as usize;
            if n.vert_pool < 0 || end > verts.len() {
                return Err(invalid(
                    at,
                    "nodes",
                    format!("node {i} vert pool {start}..{end}"),
                ));
            }
            if verts[start..end]
                .iter()
                .any(|v| v.point as usize >= points.len())
            {
                return Err(invalid(
                    at,
                    "nodes",
                    format!("node {i} references a point >= {}", points.len()),
                ));
            }
            if n.surf < 0 || n.surf as usize >= surfs.len() {
                return Err(invalid(at, "nodes", format!("node {i} surf {}", n.surf)));
            }
        }
    }
    for (i, s) in surfs.iter().enumerate() {
        let vec_ok = |v: i16| v >= 0 && (v as usize) < vectors.len();
        if !vec_ok(s.normal)
            || !vec_ok(s.texture_u)
            || !vec_ok(s.texture_v)
            || s.base < 0
            || s.base as usize >= points.len()
        {
            return Err(invalid(
                at,
                "surfs",
                format!("surf {i} index out of range: {s:?}"),
            ));
        }
    }
    Ok(Model {
        primitive,
        vectors,
        points,
        nodes,
        surfs,
        verts,
        num_shared_sides,
        num_zones,
        reserved,
        zones,
        polys,
        light_maps: Vec::new(),
        light_bits: Vec::new(),
        bounds: Vec::new(),
        leaf_hulls: Vec::new(),
        leaves: Vec::new(),
        lights: Vec::new(),
        root_outside: 0,
        linked: 0,
        mover_link: 0,
        vertex_stream: Vec::new(),
        vertex_stream_revision: 0,
        report: PayloadReport {
            payload: r.payload(),
            properties_end: 0,
            unknown: Vec::new(),
            unsupported_tail: None,
        },
    })
}

/// Reads the `FLightMapIndex` array that follows `Polys`.
///
/// Each record is a fixed 178 bytes in this build (licensee 58, package version 100): an i32
/// `DataOffset`, three `f32` (`Pan`), two 4x4 `f32` matrices, two unknown bytes, nine `i32`,
/// and four trailing compact indices. The size is verified byte-exactly on all 13 GOG Models
/// with a non-empty array.
fn decode_light_maps(r: &mut PayloadReader<'_>) -> DecodeResult<Vec<LightMapIndex>> {
    let n = r
        .count("light_maps", 1)
        .map_err(|e| e.in_field("light_maps"))?;
    let mut out = Vec::with_capacity(n);
    for _ in 0..n {
        let data_offset = r.i32().map_err(|e| e.in_field("light_maps.data_offset"))?;
        let pan = r.f32().map_err(|e| e.in_field("light_maps.pan"))?;
        let mut matrix_a = [0.0f32; 16];
        for v in &mut matrix_a {
            *v = r.f32().map_err(|e| e.in_field("light_maps.matrix_a"))?;
        }
        let mut matrix_b = [0.0f32; 16];
        for v in &mut matrix_b {
            *v = r.f32().map_err(|e| e.in_field("light_maps.matrix_b"))?;
        }
        let mut ints = [0i32; 9];
        for v in &mut ints {
            *v = r.i32().map_err(|e| e.in_field("light_maps.ints"))?;
        }
        let unknown = [r.u8()?, r.u8()?];
        let mut tail_indices = [0i32; 4];
        for v in &mut tail_indices {
            *v = r
                .compact()
                .map_err(|e| e.in_field("light_maps.tail_indices"))?;
        }
        out.push(LightMapIndex {
            data_offset,
            pan,
            matrix_a,
            matrix_b,
            ints,
            unknown,
            tail_indices,
        });
    }
    Ok(out)
}

/// Decoded tail fields (after `Polys`).
struct ModelTail {
    light_maps: Vec<LightMapIndex>,
    light_bits: Vec<u8>,
    bounds: Vec<BoundingBox>,
    leaf_hulls: Vec<i32>,
    leaves: Vec<ConvexVolumeLeaf>,
    lights: Vec<ObjectRef>,
    root_outside: i32,
    linked: i32,
    mover_link: i32,
    vertex_stream: Vec<[f32; 8]>,
    vertex_stream_revision: i32,
}

/// Reads the tail after `Polys`: `LightMap`, `LightBits`, `Bounds`, `LeafHulls`, `Leaves`,
/// `Lights`, `RootOutside`, `Linked`, `MoverLink` and the `FBspVertexStream`. Field order and
/// element sizes are taken from `UModel::Serialize` in `Engine.dll` (RVA 0x9C240) and verified
/// against the GOG Models; see the report. `RootOutside`/`Linked` and the vertex stream are
/// proven by the 6,396 Models whose whole payload is consumed exactly.
fn decode_model_tail(package: &Package, r: &mut PayloadReader<'_>) -> DecodeResult<ModelTail> {
    let light_maps = decode_light_maps(r)?;
    // Second array after LightMap (edi+0xbc). Its per-element record is 15 bytes for this
    // build (two raw bytes, one compact index, then three i32); it is empty in the 6,396
    // Models whose tail decodes end-to-end. Kept as raw bytes because the meaning is not
    // established.
    let light_bits = {
        let n = r
            .count("light_bits", 1)
            .map_err(|e| e.in_field("light_bits"))?;
        r.bytes(n).map_err(|e| e.in_field("light_bits"))?.to_vec()
    };
    let bounds = r.array("bounds", 25, |r| r.bbox())?;
    let leaf_hulls = r.array("leaf_hulls", 4, |r| r.i32())?;
    let leaves = {
        let n = r.count("leaves", 1).map_err(|e| e.in_field("leaves"))?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(ConvexVolumeLeaf {
                zone: r.compact().map_err(|e| e.in_field("leaves.zone"))?,
                permeating: r.compact().map_err(|e| e.in_field("leaves.permeating"))?,
                volumetric: r.compact().map_err(|e| e.in_field("leaves.volumetric"))?,
                visible_zones: r.u64().map_err(|e| e.in_field("leaves.visible_zones"))?,
            });
        }
        out
    };
    let lights = {
        let n = r.count("lights", 1).map_err(|e| e.in_field("lights"))?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            out.push(r.object_ref(package).map_err(|e| e.in_field("lights"))?);
        }
        out
    };
    let root_outside = r.i32().map_err(|e| e.in_field("root_outside"))?;
    let linked = r.i32().map_err(|e| e.in_field("linked"))?;
    let mover_link = r.i32().map_err(|e| e.in_field("mover_link"))?;
    // FBspVertexStream: `TArray<FBspVertex>` (32 bytes each) then a revision i32.
    let vertex_stream = {
        let n = r
            .count("vertex_stream", 32)
            .map_err(|e| e.in_field("vertex_stream"))?;
        let mut out = Vec::with_capacity(n);
        for _ in 0..n {
            let mut v = [0.0f32; 8];
            for f in &mut v {
                *f = r.f32().map_err(|e| e.in_field("vertex_stream"))?;
            }
            out.push(v);
        }
        out
    };
    let vertex_stream_revision = r.i32().map_err(|e| e.in_field("vertex_stream_revision"))?;
    // A final `TArray` count closes the payload; it is 0 on the models whose payload is
    // consumed exactly. It is consumed when zero; a non-zero or unreadable count is left in
    // place for the caller to report as an explicit unsupported tail (this build variant).
    let before_final = r.pos();
    match r.compact() {
        Ok(0) => {}
        _ => r
            .seek(before_final)
            .map_err(|e| e.in_field("vertex_stream_revision"))?,
    }
    Ok(ModelTail {
        light_maps,
        light_bits,
        bounds,
        leaf_hulls,
        leaves,
        lights,
        root_outside,
        linked,
        mover_link,
        vertex_stream,
        vertex_stream_revision,
    })
}

/// Reads the variable-length zone records.
fn decode_zones(
    package: &Package,
    r: &mut PayloadReader<'_>,
    num_zones: i32,
) -> DecodeResult<Vec<Zone>> {
    let mut zones = Vec::with_capacity(num_zones.max(0) as usize);
    for zone in 0..num_zones.max(0) as usize {
        let start = r.pos();
        let actor = r
            .object_ref(package)
            .map_err(|e| e.in_field("zones.actor"))?;
        let connectivity = r.u64().map_err(|e| e.in_field("zones.connectivity"))?;
        let visibility = r.u64().map_err(|e| e.in_field("zones.visibility"))?;
        let last_render_time = r.u32().map_err(|e| e.in_field("zones.last_render_time"))?;
        let last_render_time = f32::from_bits(last_render_time);
        // Invariant (all 1,350 measured zone records): the connectivity mask is a zone
        // bitmask; zone `z` connects to itself and no out-of-range zone is set.
        let in_range = if num_zones >= 64 {
            true
        } else {
            connectivity >> num_zones == 0
        };
        if connectivity & (1u64 << zone) == 0 || !in_range {
            return Err(invalid(
                start,
                "zones.connectivity",
                format!("zone {zone} connectivity 0x{connectivity:x} with num_zones {num_zones}"),
            ));
        }
        zones.push(Zone {
            actor,
            connectivity,
            visibility,
            last_render_time,
        });
    }
    Ok(zones)
}

/// Reads the `Polys` object reference stored after the zone records. `None` when the payload
/// ends there (the reference is required by the layout, so a missing reference is reported).
fn decode_polys_ref(
    package: &Package,
    r: &mut PayloadReader<'_>,
) -> DecodeResult<Option<ObjectRef>> {
    let at = r.pos();
    let raw = r.compact().map_err(|e| e.in_field("polys"))?;
    match package.resolve(raw) {
        Some(ObjectRef::Null) => Ok(None),
        Some(r) => Ok(Some(r)),
        None => Err(invalid(
            at,
            "polys",
            format!("polys reference {raw} out of range"),
        )),
    }
}

/// Decodes a Polys export completely.
pub fn decode_polys(package: &Package, data: &[u8], export: usize) -> DecodeResult<Polys> {
    let props = read_properties(package, data, export, POLYS_CLASS)?;
    let ctx = |e: DecodeError| e.in_export(package, export);
    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    let at = r.pos();
    let num = r.i32().map_err(|e| ctx(e.in_field("num")))?;
    let max = r.i32().map_err(|e| ctx(e.in_field("max")))?;
    let n = r
        .check_count("polys", i64::from(num), 1 + 48 + 12 + 4 + 5, at)
        .map_err(ctx)?;
    if max < num {
        return Err(ctx(invalid(at, "max", format!("max {max} < num {num}"))));
    }
    let mut polys = Vec::with_capacity(n);
    for _ in 0..n {
        let p = (|| -> DecodeResult<Poly> {
            let at = r.pos();
            let nv = r.compact()?;
            if !(0..=64).contains(&nv) {
                return Err(invalid(at, "polys.num_vertices", format!("{nv}")));
            }
            let base = r.vec3()?;
            let normal = r.vec3()?;
            let texture_u = r.vec3()?;
            let texture_v = r.vec3()?;
            let mut vertices = Vec::with_capacity(nv as usize);
            for _ in 0..nv {
                vertices.push(r.vec3()?);
            }
            Ok(Poly {
                base,
                normal,
                texture_u,
                texture_v,
                vertices,
                poly_flags: r.u32()?,
                actor: r.object_ref(package)?,
                material: r.object_ref(package)?,
                item_name: r.compact()?,
                link: r.compact()?,
                brush_poly: r.compact()?,
            })
        })()
        .map_err(|e| ctx(e.in_field("polys")))?;
        polys.push(p);
    }
    let report = r.finish(props.block.span.end).map_err(ctx)?;
    Ok(Polys { max, polys, report })
}

/// Finds the level BSP: the `Engine.Model` export that no actor's `Brush` property points
/// at (the brushes' own models are referenced that way). Fails unless exactly one remains.
pub fn find_level_model(package: &Package, data: &[u8]) -> DecodeResult<usize> {
    let mut referenced = std::collections::BTreeSet::new();
    for i in 0..package.exports().len() {
        if package.exports()[i].serial_size == 0 {
            continue;
        }
        let class = package.export_class_path(i).unwrap_or("");
        if class.eq_ignore_ascii_case(MODEL_CLASS) || class.eq_ignore_ascii_case(POLYS_CLASS) {
            continue;
        }
        let Ok(props) = package.read_object_properties(data, i, &xiii_package::Limits::default())
        else {
            continue;
        };
        for p in &props.block.properties {
            if package.property_name(p).eq_ignore_ascii_case("Brush")
                && let PropertyValue::Object(ObjectRef::Export(e)) = p.value
            {
                referenced.insert(e as usize);
            }
        }
    }
    let candidates: Vec<usize> = (0..package.exports().len())
        .filter(|&i| {
            package
                .export_class_path(i)
                .is_some_and(|c| c.eq_ignore_ascii_case(MODEL_CLASS))
                && !referenced.contains(&i)
                && package.exports()[i].serial_size > 0
        })
        .collect();
    match candidates.as_slice() {
        [one] => Ok(*one),
        _ => Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
            "expected one unreferenced Model export, found {candidates:?}"
        )))),
    }
}

/// Triangulated surface polygon of a BSP model (source coordinates, source winding).
#[derive(Debug, Clone, PartialEq)]
pub struct BspPolygon {
    /// Node index.
    pub node: usize,
    /// Surface index.
    pub surf: usize,
    /// Polygon vertices in node order.
    pub vertices: Vec<[f32; 3]>,
}

impl Model {
    /// Polygons of all nodes that carry vertices.
    pub fn polygons(&self) -> Vec<BspPolygon> {
        self.nodes
            .iter()
            .enumerate()
            .filter(|(_, n)| n.num_vertices >= 3)
            .map(|(i, n)| {
                let start = n.vert_pool as usize;
                BspPolygon {
                    node: i,
                    surf: n.surf as usize,
                    vertices: self.verts[start..start + n.num_vertices as usize]
                        .iter()
                        .map(|v| self.points[v.point as usize])
                        .collect(),
                }
            })
            .collect()
    }

    /// Counters for coverage reports.
    pub fn counters(&self) -> Vec<(&'static str, u64)> {
        let polys = self.nodes.iter().filter(|n| n.num_vertices >= 3).count() as u64;
        let tail = self
            .report
            .unsupported_tail
            .map_or(0, |(_, s)| s.len() as u64);
        let zone_actors = self.zones.iter().filter(|z| !z.actor.is_null()).count() as u64;
        vec![
            ("nodes", self.nodes.len() as u64),
            ("surfs", self.surfs.len() as u64),
            ("node_polygons", polys),
            ("zones", self.zones.len() as u64),
            ("zone_actors", zone_actors),
            ("polys_ref", u64::from(self.polys.is_some())),
            ("light_maps", self.light_maps.len() as u64),
            ("light_bits_bytes", self.light_bits.len() as u64),
            ("bounds", self.bounds.len() as u64),
            ("leaf_hulls", self.leaf_hulls.len() as u64),
            ("leaves", self.leaves.len() as u64),
            ("lights", self.lights.len() as u64),
            ("vertex_stream", self.vertex_stream.len() as u64),
            ("unsupported_tail_bytes", tail),
        ]
    }

    /// Class path of the zone actor's export, if the actor is an export of this package.
    pub fn zone_actor_class<'p>(&self, package: &'p Package, zone: &Zone) -> Option<&'p str> {
        match zone.actor {
            ObjectRef::Export(e) => package.export_class_path(e as usize),
            _ => None,
        }
    }

    /// True when the zone actor's class path contains `zone` (case-insensitive): a
    /// `ZoneInfo`, `SkyZoneInfo` or `WarpZoneInfo` (or subclass). Null actors are `false`.
    pub fn zone_actor_is_zone_info(&self, package: &Package, zone: &Zone) -> bool {
        self.zone_actor_class(package, zone)
            .is_some_and(|c| c.to_ascii_lowercase().contains("zone"))
    }

    /// True when the zone actor's class path ends in `skyzoneinfo` (case-insensitive).
    pub fn zone_actor_is_sky(&self, package: &Package, zone: &Zone) -> bool {
        self.zone_actor_class(package, zone)
            .is_some_and(|c| c.to_ascii_lowercase().ends_with("skyzoneinfo"))
    }

    /// Map of BSP leaf index -> zone index, derived from the two `iLeaf`/`iZone` pairs of
    /// each node. Returns `(zone_of_leaf, conflicts)`; an entry is `None` when no node
    /// references that leaf. Two nodes that reference the same leaf with different zones
    /// increment `conflicts` (0 in the whole GOG corpus).
    pub fn leaf_zones(&self) -> (Vec<Option<u8>>, u64) {
        let max_leaf = self.nodes.iter().flat_map(|n| n.leaf).max().unwrap_or(-1);
        let n = (max_leaf + 1).max(0) as usize;
        let mut out = vec![None; n];
        let mut conflicts = 0u64;
        for node in &self.nodes {
            for (k, &leaf) in node.leaf.iter().enumerate() {
                if leaf < 0 {
                    continue;
                }
                let leaf = leaf as usize;
                if leaf >= n {
                    continue;
                }
                match out[leaf] {
                    None => out[leaf] = Some(node.zone[k]),
                    Some(z) => {
                        if z != node.zone[k] {
                            conflicts += 1;
                        }
                    }
                }
            }
        }
        (out, conflicts)
    }

    /// Winding of node polygons against their node plane in source coordinates:
    /// `(against, along, degenerate)` for the first fan triangle.
    pub fn winding_statistics(&self) -> (usize, usize, usize) {
        let mut out = (0, 0, 0);
        for p in self.polygons() {
            let n = crate::common::triangle_cross(p.vertices[0], p.vertices[1], p.vertices[2]);
            let pl = self.nodes[p.node].plane;
            let d = n[0] * pl[0] + n[1] * pl[1] + n[2] * pl[2];
            if d.abs() < 1e-4 {
                out.2 += 1;
            } else if d < 0.0 {
                out.0 += 1;
            } else {
                out.1 += 1;
            }
        }
        out
    }
}

/// Text listing of the zones of one model for `xiii-tool zones`.
pub fn zones_text(package: &Package, export: usize, m: &Model) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} zones {} (reserved {} polys {})",
        package
            .object_path(ObjectRef::Export(export as u32))
            .unwrap_or("?"),
        m.zones.len(),
        m.reserved,
        ref_text(package, m.polys)
    );
    let (zone_of_leaf, conflicts) = m.leaf_zones();
    let mut leaf_counts = vec![0u64; m.zones.len()];
    for zone in zone_of_leaf.iter().flatten() {
        if let Some(c) = leaf_counts.get_mut(*zone as usize) {
            *c += 1;
        }
    }
    if conflicts != 0 {
        let _ = writeln!(
            out,
            "  WARNING: {conflicts} leaf/zone conflicts in the nodes"
        );
    }
    for (i, z) in m.zones.iter().enumerate() {
        let class = m.zone_actor_class(package, z).unwrap_or("");
        let sky = if m.zone_actor_is_sky(package, z) {
            " SKY"
        } else {
            ""
        };
        let actor = match z.actor {
            ObjectRef::Null => "None".to_owned(),
            _ => format!(
                "{} {}{}",
                package.object_path(z.actor).unwrap_or("?"),
                class,
                sky
            ),
        };
        let _ = writeln!(
            out,
            "  zone {i:>2} leaves {:>5} connectivity 0x{:<16x} visibility 0x{:<16x} actor {}",
            leaf_counts.get(i).copied().unwrap_or(0),
            z.connectivity,
            z.visibility,
            actor
        );
    }
    let _ = writeln!(
        out,
        "  total leaves {} (zone conflicts {conflicts})",
        zone_of_leaf.len()
    );
    out
}

/// Text summary for `xiii-tool bsp`.
pub fn summary_text(package: &Package, export: usize, m: &Model) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{} vectors {} points {} nodes {} surfs {} verts {} shared sides {} zones {}",
        package
            .object_path(ObjectRef::Export(export as u32))
            .unwrap_or("?"),
        m.vectors.len(),
        m.points.len(),
        m.nodes.len(),
        m.surfs.len(),
        m.verts.len(),
        m.num_shared_sides,
        m.num_zones
    );
    let polys = m.polygons();
    let tris: usize = polys.iter().map(|p| p.vertices.len() - 2).sum();
    let _ = writeln!(out, "  node polygons {} -> triangles {}", polys.len(), tris);
    let w = m.winding_statistics();
    let _ = writeln!(
        out,
        "  winding vs node plane (source coords) against/along/degenerate {w:?}"
    );
    let mut flags = std::collections::BTreeMap::<u32, usize>::new();
    let mut mats = std::collections::BTreeMap::<String, usize>::new();
    for p in &polys {
        let s = &m.surfs[p.surf];
        *flags.entry(s.poly_flags).or_default() += 1;
        *mats
            .entry(package.object_path(s.material).unwrap_or("None").to_owned())
            .or_default() += 1;
    }
    let _ = writeln!(
        out,
        "  poly flags histogram: {:?}",
        flags
            .iter()
            .map(|(k, v)| format!("0x{k:x}:{v}"))
            .collect::<Vec<_>>()
    );
    let _ = writeln!(out, "  materials ({}):", mats.len());
    for (k, v) in &mats {
        let _ = writeln!(out, "    {v:>5} {k}");
    }
    let _ = writeln!(
        out,
        "  lightmaps {} lightbits {} bounds {} leafhulls {} leaves {} lights {} vertexstream {} (rev {})",
        m.light_maps.len(),
        m.light_bits.len(),
        m.bounds.len(),
        m.leaf_hulls.len(),
        m.leaves.len(),
        m.lights.len(),
        m.vertex_stream.len(),
        m.vertex_stream_revision
    );
    let _ = writeln!(
        out,
        "  RootOutside {} Linked {} MoverLink {}",
        m.root_outside, m.linked, m.mover_link
    );
    let _ = writeln!(
        out,
        "  reserved {} polys {} zones:",
        m.reserved,
        ref_text(package, m.polys)
    );
    for (i, z) in m.zones.iter().enumerate() {
        let class = m
            .zone_actor_class(package, z)
            .unwrap_or(if z.actor.is_null() { "(null)" } else { "?" });
        let _ = writeln!(
            out,
            "    zone {i}: actor {} class {} connectivity 0x{:x} visibility 0x{:x} last_render_time {}",
            ref_text(package, Some(z.actor)),
            class,
            z.connectivity,
            z.visibility,
            z.last_render_time
        );
    }
    if let Some((label, span)) = m.report.unsupported_tail {
        let _ = writeln!(
            out,
            "  UNSUPPORTED TAIL {label}: {} bytes at 0x{:x} (payload+{}) of {} payload bytes",
            span.len(),
            span.start,
            span.start - m.report.payload.start,
            m.report.payload.len()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_package::{Builder, Bytes, parse};

    fn model_payload(vert_pool: i32, tail: &[u8]) -> Vec<u8> {
        let mut b = Bytes::default().c(0);
        b = b
            .v3([0.0; 3])
            .v3([1.0; 3])
            .u8(1)
            .v3([0.0; 3])
            .f32(1.0)
            .raw(&[0; 8]);
        b = b
            .c(3)
            .v3([0.0, 0.0, 1.0])
            .v3([1.0, 0.0, 0.0])
            .v3([0.0, 1.0, 0.0]);
        b = b
            .c(3)
            .v3([0.0, 0.0, 0.0])
            .v3([1.0, 0.0, 0.0])
            .v3([0.0, 1.0, 0.0]);
        b = b
            .c(1)
            .v3([0.0, 0.0, 1.0])
            .f32(0.0)
            .i32(1)
            .i32(0)
            .u8(0)
            .i32(vert_pool);
        for v in [0i16, -1, -1, -1, -1, -1, 0] {
            b = b.i16(v);
        }
        b = b
            .v3([0.5, 0.5, 0.0])
            .f32(1.0)
            .u8(0)
            .u8(1)
            .u8(3)
            .i16(-1)
            .i16(-1)
            .i16(0)
            .u16(0);
        b = b
            .c(1)
            .c(0)
            .i32(0)
            .i16(0)
            .i16(0)
            .i16(1)
            .i16(2)
            .i16(-1)
            .i16(0)
            .u8(0)
            .c(0);
        b = b.v3([0.0, 0.0, 1.0]).f32(0.0);
        b = b.c(3).u16(0).i16(-1).u16(1).i16(-1).u16(2).i16(-1);
        // num_shared_sides, num_zones (0), reserved, Polys reference (null), then the tail.
        b = b.i32(0).i32(0).i32(0).c(0).raw(tail);
        b.0
    }

    /// Model bytes with `num_zones` raw zone records and a compact `Polys` reference.
    fn model_with_zones(vert_pool: i32, num_zones: i32, zones: &[u8], polys: &[u8]) -> Vec<u8> {
        let mut b = Bytes::default().c(0);
        b = b
            .v3([0.0; 3])
            .v3([1.0; 3])
            .u8(1)
            .v3([0.0; 3])
            .f32(1.0)
            .raw(&[0; 8]);
        b = b
            .c(3)
            .v3([0.0, 0.0, 1.0])
            .v3([1.0, 0.0, 0.0])
            .v3([0.0, 1.0, 0.0]);
        b = b
            .c(3)
            .v3([0.0, 0.0, 0.0])
            .v3([1.0, 0.0, 0.0])
            .v3([0.0, 1.0, 0.0]);
        b = b
            .c(1)
            .v3([0.0, 0.0, 1.0])
            .f32(0.0)
            .i32(1)
            .i32(0)
            .u8(0)
            .i32(vert_pool);
        for v in [0i16, -1, -1, -1, -1, -1, 0] {
            b = b.i16(v);
        }
        b = b
            .v3([0.5, 0.5, 0.0])
            .f32(1.0)
            .u8(0)
            .u8(1)
            .u8(3)
            .i16(-1)
            .i16(-1)
            .i16(0)
            .u16(0);
        b = b
            .c(1)
            .c(0)
            .i32(0)
            .i16(0)
            .i16(0)
            .i16(1)
            .i16(2)
            .i16(-1)
            .i16(0)
            .u8(0)
            .c(0);
        b = b.v3([0.0, 0.0, 1.0]).f32(0.0);
        b = b.c(3).u16(0).i16(-1).u16(1).i16(-1).u16(2).i16(-1);
        b = b.i32(0).i32(num_zones).i32(0).raw(zones).raw(polys);
        b.0
    }

    /// A zone record with a pre-encoded actor reference (so non-minimal compact encodings
    /// can be exercised), then connectivity, visibility and last render time.
    fn zone_bytes_raw(actor: &[u8], connectivity: u64, visibility: u64, lrt: f32) -> Vec<u8> {
        let mut b = Bytes::default().raw(actor);
        b = b.raw(&connectivity.to_le_bytes());
        b = b.raw(&visibility.to_le_bytes());
        b = b.f32(lrt);
        b.0
    }

    /// A zone record with a minimal compact actor reference.
    fn zone_bytes(actor: i32, connectivity: u64, visibility: u64, lrt: f32) -> Vec<u8> {
        zone_bytes_raw(
            &crate::common::test_package::compact(actor),
            connectivity,
            visibility,
            lrt,
        )
    }

    #[test]
    fn synthetic_model_prefix_and_explicit_tail() {
        let mut b = Builder::new();
        let i = b.export("Model", "M", model_payload(0, &[1, 2, 3]));
        let bytes = b.build();
        let p = parse(&bytes);
        let m = decode_model(&p, &bytes, i).unwrap();
        assert_eq!(m.nodes.len(), 1);
        assert_eq!(m.polygons()[0].vertices.len(), 3);
        assert_eq!(m.zones.len(), 0);
        assert_eq!(m.polys, None);
        // The 3 bogus bytes do not form a decodable tail, so they stay an explicit tail.
        let (label, span) = m.report.unsupported_tail.unwrap();
        assert_eq!((label, span.len()), ("model.after_linked", 3));
        // Node polygon is (0,0,0),(1,0,0),(0,1,0) with plane +Z: numerically along the plane.
        assert_eq!(m.winding_statistics(), (0, 1, 0));
        // A vert pool outside the verts array is rejected.
        let mut b = Builder::new();
        let i = b.export("Model", "M", model_payload(2, &[]));
        let bytes = b.build();
        let p = parse(&bytes);
        assert!(decode_model(&p, &bytes, i).is_err());
    }

    /// Tail bytes after the `Polys` reference: empty LightMap/LightBits/Bounds/LeafHulls/
    /// Leaves/Lights, RootOutside=0, Linked=1, MoverLink=0, one 32-byte vertex, revision 7.
    fn model_tail_payload() -> Vec<u8> {
        let mut b = Bytes::default().c(0).c(0).c(0).c(0).c(0).c(0);
        b = b.i32(0).i32(1).i32(0);
        b = b.c(1);
        for v in 1..=8 {
            b = b.f32(v as f32);
        }
        b = b.i32(7);
        b.0
    }

    #[test]
    fn synthetic_model_tail_decodes_exactly() {
        let mut b = Builder::new();
        let i = b.export("Model", "M", model_payload(0, &model_tail_payload()));
        let bytes = b.build();
        let p = parse(&bytes);
        let m = decode_model(&p, &bytes, i).unwrap();
        assert!(
            m.report.unsupported_tail.is_none(),
            "tail must consume exactly"
        );
        assert_eq!(m.root_outside, 0);
        assert_eq!(m.linked, 1);
        assert_eq!(m.mover_link, 0);
        assert_eq!(m.vertex_stream.len(), 1);
        assert_eq!(m.vertex_stream[0], [1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0]);
        assert_eq!(m.vertex_stream_revision, 7);
    }

    #[test]
    fn synthetic_model_tail_variant_falls_back_to_labelled_tail() {
        // A Bounds count that cannot fit (0x7f = 127 elements x 25 B) must not fail the whole
        // export: the prefix is kept and the remainder is an explicit unsupported tail.
        let mut tail = vec![0u8]; // empty LightMap
        tail.push(0); // empty LightBits
        tail.push(0x7f); // 127 Bounds, far more than the remaining bytes
        let mut b = Builder::new();
        let i = b.export("Model", "M", model_payload(0, &tail));
        let bytes = b.build();
        let p = parse(&bytes);
        let m = decode_model(&p, &bytes, i).unwrap();
        assert!(m.vertex_stream.is_empty());
        let (label, _) = m.report.unsupported_tail.unwrap();
        assert_eq!(label, "model.after_linked");
        // No bytes are dropped: the tail starts right after the Polys reference.
        assert_eq!(m.report.unsupported_tail.unwrap().1.len(), tail.len());
    }

    #[test]
    fn synthetic_zones_variable_size_and_polys() {
        // Two zones whose actor compacts are 1 and 4 bytes, with a Polys export reference
        // after them. The 4-byte form (export 65536) needs a builder with that many exports
        // only for resolution; instead use a non-minimal 2-byte encoding of export 0.
        // A non-minimal 2-byte encoding of export 0 (raw 1), then the minimal 1-byte form
        // (zone 1): the two records are 22 and 21 bytes, proving the variable-size handling.
        let z0 = zone_bytes_raw(&[0x41, 0x00], 0b01, 0, 0.0);
        let z1 = zone_bytes(1, 0b10, 0xffff_ffff_ffff_ffff, 1.5);
        assert_eq!(z0.len(), 22);
        assert_eq!(z1.len(), 21);
        let mut zones = z0.clone();
        zones.extend_from_slice(&z1);
        let mut b = Builder::new();
        let polys_i = b.export("Polys", "P", vec![]);
        let model_i = b.export(
            "Model",
            "M",
            model_with_zones(
                0,
                2,
                &zones,
                &crate::common::test_package::compact(polys_i as i32 + 1),
            ),
        );
        let bytes = b.build();
        let p = parse(&bytes);
        let m = decode_model(&p, &bytes, model_i).unwrap();
        assert_eq!(m.reserved, 0);
        assert_eq!(m.zones.len(), 2);
        assert_eq!(m.zones[0].actor, ObjectRef::Export(0));
        assert_eq!(m.zones[0].connectivity, 0b01);
        assert_eq!(m.zones[1].actor, ObjectRef::Export(0));
        assert_eq!(m.zones[1].visibility, u64::MAX);
        assert_eq!(m.zones[1].last_render_time, 1.5);
        assert_eq!(m.polys, Some(ObjectRef::Export(polys_i as u32)));
        // A wrong connectivity (no self bit) is rejected.
        let bad = zone_bytes(0, 0b10, 0, 0.0);
        let mut b = Builder::new();
        let polys_i = b.export("Polys", "P", vec![]);
        let model_i = b.export(
            "Model",
            "M",
            model_with_zones(
                0,
                1,
                &bad,
                &crate::common::test_package::compact(polys_i as i32 + 1),
            ),
        );
        let bytes = b.build();
        let p = parse(&bytes);
        let e = decode_model(&p, &bytes, model_i).unwrap_err();
        assert_eq!(e.field, Some("zones.connectivity"));
    }

    #[test]
    fn synthetic_model_truncated_zone_tail_errors() {
        let z0 = zone_bytes(0, 0b01, 0, 0.0);
        // claim two zones but only provide one record
        let mut b = Builder::new();
        let model_i = b.export("Model", "M", model_with_zones(0, 2, &z0, &[]));
        let bytes = b.build();
        let p = parse(&bytes);
        let e = decode_model(&p, &bytes, model_i).unwrap_err();
        assert!(matches!(
            e.kind,
            DecodeErrorKind::Package(_) | DecodeErrorKind::Invalid(_)
        ));
    }

    #[test]
    fn local_corpus_model_zones() {
        let Some(dir) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to run the model zone corpus test");
            return;
        };
        let root = std::path::Path::new(&dir);
        let mut files = Vec::new();
        collect_tagged(root, &mut files);
        files.sort();
        let (mut models, mut zoned, mut zones_total, mut actor_exports, mut null_actors) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let mut polys_export = 0u64;
        for (rel, path) in &files {
            let data = std::fs::read(path).unwrap_or_else(|e| panic!("{rel}: {e}"));
            let p = Package::parse(&data, &xiii_package::Limits::default())
                .unwrap_or_else(|e| panic!("{rel}: {e}"));
            for i in 0..p.exports().len() {
                if p.exports()[i].serial_size == 0 {
                    continue;
                }
                let Some(class) = p.export_class_path(i) else {
                    continue;
                };
                if !class.eq_ignore_ascii_case(MODEL_CLASS) {
                    continue;
                }
                let m = decode_model(&p, &data, i).unwrap_or_else(|e| panic!("{rel}: {e}"));
                models += 1;
                assert_eq!(m.zones.len(), m.num_zones.max(0) as usize, "{rel}");
                // Polys reference, when present, resolves to an Engine.Polys export.
                if let Some(ObjectRef::Export(e)) = m.polys {
                    assert!(
                        p.export_class_path(e as usize)
                            .is_some_and(|c| c.eq_ignore_ascii_case(POLYS_CLASS)),
                        "{rel}: polys ref is not Polys"
                    );
                    polys_export += 1;
                }
                // Leaf -> zone derivation is consistent (0 conflicts) in the whole corpus.
                let (_, conflicts) = m.leaf_zones();
                assert_eq!(conflicts, 0, "{rel} export {i}");
                let nz = m.num_zones.max(0) as usize;
                if nz > 0 {
                    zoned += 1;
                    // Zone 0 has a null actor and an empty leaf assignment in every map.
                    assert!(m.zones[0].actor.is_null(), "{rel}: zone 0 actor not null");
                    assert!(
                        m.zones
                            .iter()
                            .enumerate()
                            .all(|(z, zone)| zone.connectivity & (1u64 << z) != 0
                                && zone.connectivity >> nz == 0),
                        "{rel}: connectivity invariant"
                    );
                    zones_total += nz as u64;
                }
                for zone in &m.zones {
                    match zone.actor {
                        ObjectRef::Null => null_actors += 1,
                        ObjectRef::Export(_) => actor_exports += 1,
                        other => panic!("{rel}: unexpected zone actor {other:?}"),
                    }
                }
            }
        }
        assert_eq!(models, 7194, "model count");
        assert_eq!(zoned, 64, "zoned model count");
        assert_eq!(zones_total, 1350, "zone count");
        assert_eq!(polys_export, 7194, "polys references");
        assert_eq!(actor_exports, 369, "zone actor exports");
        assert_eq!(null_actors, 981, "null zone actors");
    }

    fn collect_tagged(dir: &std::path::Path, out: &mut Vec<(String, std::path::PathBuf)>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let path = entry.path();
            if path.is_dir() {
                collect_tagged(&path, out);
            } else if let Ok(prefix) = read_prefix(&path)
                && xiii_package::has_package_tag(&prefix)
            {
                out.push((path.display().to_string(), path));
            }
        }
    }

    fn read_prefix(path: &std::path::Path) -> std::io::Result<[u8; 4]> {
        use std::io::Read;
        let mut f = std::fs::File::open(path)?;
        let mut b = [0u8; 4];
        f.read_exact(&mut b)?;
        Ok(b)
    }

    fn polys_payload(extra: &[u8]) -> Vec<u8> {
        let mut b = Bytes::default().c(0).i32(1).i32(1).c(3);
        b = b
            .v3([0.0; 3])
            .v3([0.0, 0.0, 1.0])
            .v3([1.0, 0.0, 0.0])
            .v3([0.0, 1.0, 0.0]);
        b = b.v3([0.0; 3]).v3([1.0, 0.0, 0.0]).v3([0.0, 1.0, 0.0]);
        b = b.i32(0x1000).c(0).c(0).c(0).c(0).c(-1).raw(extra);
        b.0
    }

    #[test]
    fn local_corpus_model_tail() {
        let Some(dir) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to run the model tail corpus test");
            return;
        };
        let root = std::path::Path::new(&dir);
        let mut files = Vec::new();
        collect_tagged(root, &mut files);
        files.sort();
        let mut models = 0u64;
        let mut with_lm = 0u64;
        let mut lm_total = 0u64;
        let mut leaves_total = 0u64;
        let mut bounds_total = 0u64;
        let mut lights_total = 0u64;
        let mut vertex_total = 0u64;
        for (rel, path) in &files {
            let data = std::fs::read(path).unwrap_or_else(|e| panic!("{rel}: {e}"));
            let p = Package::parse(&data, &xiii_package::Limits::default())
                .unwrap_or_else(|e| panic!("{rel}: {e}"));
            for i in 0..p.exports().len() {
                if p.exports()[i].serial_size == 0 {
                    continue;
                }
                let Some(class) = p.export_class_path(i) else {
                    continue;
                };
                if !class.eq_ignore_ascii_case(MODEL_CLASS) {
                    continue;
                }
                let m = decode_model(&p, &data, i).unwrap_or_else(|e| panic!("{rel}: {e}"));
                models += 1;
                lm_total += m.light_maps.len() as u64;
                leaves_total += m.leaves.len() as u64;
                bounds_total += m.bounds.len() as u64;
                lights_total += m.lights.len() as u64;
                vertex_total += m.vertex_stream.len() as u64;
                if !m.light_maps.is_empty() {
                    with_lm += 1;
                }
                // Invariant: when the tail decoded (the model has at least one LightMap entry
                // or no surface references one), a surface's iLightMap indexes the LightMaps.
                // The 7 build-variant Models whose tail does not decode keep an empty array.
                if !m.light_maps.is_empty() {
                    for s in &m.surfs {
                        if s.light_map >= 0 {
                            assert!(
                                (s.light_map as usize) < m.light_maps.len(),
                                "{rel}: surf iLightMap {} >= {}",
                                s.light_map,
                                m.light_maps.len()
                            );
                        }
                    }
                }

                // Invariant: leaf count equals max node leaf + 1 (Plage00/Plage01 measured 141
                // = 140 + 1, 110 = 109 + 1). Only checked when the whole payload is consumed
                // exactly (no unsupported tail), so a variant tail cannot trigger it.
                if m.report.unsupported_tail.is_none()
                    && let (Some(max_leaf), false) = (
                        m.nodes.iter().flat_map(|n| n.leaf).max(),
                        m.leaves.is_empty(),
                    )
                {
                    assert_eq!(
                        m.leaves.len(),
                        (max_leaf + 1) as usize,
                        "{rel}: leaves vs max leaf"
                    );
                }
                // Invariants below only hold for the fully-consumed exports; variant tails are
                // excluded so a misaligned fallback cannot trigger a false invariant failure.
                if m.report.unsupported_tail.is_none() {
                    // Every node collision bound indexes LeafHulls (upstream
                    // `BspNode::GetCollisionBox` reads `LeafHulls[CollisionBound]`).
                    if !m.leaf_hulls.is_empty() {
                        for n in &m.nodes {
                            if n.collision_bound >= 0 {
                                assert!(
                                    (n.collision_bound as usize) < m.leaf_hulls.len(),
                                    "{rel}: node collision bound {} >= {}",
                                    n.collision_bound,
                                    m.leaf_hulls.len()
                                );
                            }
                        }
                    }
                    // Every node render bound indexes Bounds.
                    if !m.bounds.is_empty() {
                        for n in &m.nodes {
                            if n.render_bound >= 0 {
                                assert!(
                                    (n.render_bound as usize) < m.bounds.len(),
                                    "{rel}: node render bound {} >= {}",
                                    n.render_bound,
                                    m.bounds.len()
                                );
                            }
                        }
                    }
                }
                // Invariant: every reference in the Lights array is either null or an export
                // of this package (no imports in the corpus).
                if m.report.unsupported_tail.is_none() {
                    for l in &m.lights {
                        assert!(
                            matches!(l, ObjectRef::Null | ObjectRef::Export(_)),
                            "{rel}: light reference {l:?} is not null or an export"
                        );
                    }
                }
            }
        }
        assert_eq!(models, 7194, "model count");
        assert_eq!(with_lm, 8, "models with a decoded non-empty LightMap");
        assert_eq!(lm_total, 235, "lightmap index count");
        assert_eq!(leaves_total, 4951, "convex volume leaf count");
        assert_eq!(bounds_total, 14866, "bounds count");
        assert_eq!(lights_total, 7023, "light references");
        assert_eq!(vertex_total, 119639, "vertex stream count");
    }

    #[test]
    fn synthetic_polys_exact_and_tail() {
        let mut b = Builder::new();
        let i = b.export("Polys", "P", polys_payload(&[]));
        let j = b.export("Polys", "Q", polys_payload(&[0]));
        let bytes = b.build();
        let p = parse(&bytes);
        let polys = decode_polys(&p, &bytes, i).unwrap();
        assert_eq!(polys.polys[0].vertices.len(), 3);
        assert_eq!(polys.polys[0].brush_poly, -1);
        assert!(matches!(
            decode_polys(&p, &bytes, j).unwrap_err().kind,
            DecodeErrorKind::UnconsumedTail { bytes: 1 }
        ));
    }
}
