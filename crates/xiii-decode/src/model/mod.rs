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
//! ```
//!
//! What follows (zones, Polys reference, lightmaps, bounds, leaves, ...) is **not** decoded
//! and is reported as an explicit unsupported tail (`PayloadReport::unsupported_tail`).
//! Zone records do not have a uniform size in the inspected maps.
//!
//! `Polys` is decoded completely: `i32 Num, i32 Max`, then per polygon: compact vertex
//! count, Base, Normal, TextureU, TextureV, vertices, u32 PolyFlags, compact Actor, compact
//! Material, compact ItemName (name index), compact iLink, compact iBrushPoly.

pub mod level;

use std::fmt::Write as _;

use xiii_package::{ObjectRef, Package, PropertyValue};

use crate::common::{
    DecodeError, DecodeErrorKind, DecodeResult, PayloadReader, PayloadReport, PrimitiveHeader,
    read_properties,
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
    /// Byte accounting (with the unsupported tail).
    pub report: PayloadReport,
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

/// Decodes a Model up to NumZones; the rest is reported as an unsupported tail.
pub fn decode_model(package: &Package, data: &[u8], export: usize) -> DecodeResult<Model> {
    let props = read_properties(package, data, export, MODEL_CLASS)?;
    let ctx = |e: DecodeError| e.in_export(package, export);
    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    let m = decode_model_body(package, &mut r).map_err(ctx)?;
    let report = r
        .finish_with_unsupported_tail("model.zones_and_after", props.block.span.end)
        .map_err(ctx)?;
    Ok(Model { report, ..m })
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
        report: PayloadReport {
            payload: r.payload(),
            properties_end: 0,
            unknown: Vec::new(),
            unsupported_tail: None,
        },
    })
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
        vec![
            ("nodes", self.nodes.len() as u64),
            ("surfs", self.surfs.len() as u64),
            ("node_polygons", polys),
            ("unsupported_tail_bytes", tail),
        ]
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
        b = b.i32(0).i32(0).raw(tail);
        b.0
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
        let (label, span) = m.report.unsupported_tail.unwrap();
        assert_eq!((label, span.len()), ("model.zones_and_after", 3));
        // Node polygon is (0,0,0),(1,0,0),(0,1,0) with plane +Z: numerically along the plane.
        assert_eq!(m.winding_statistics(), (0, 1, 0));
        // A vert pool outside the verts array is rejected.
        let mut b = Builder::new();
        let i = b.export("Model", "M", model_payload(2, &[]));
        let bytes = b.build();
        let p = parse(&bytes);
        assert!(decode_model(&p, &bytes, i).is_err());
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
