//! `Engine.TerrainInfo` and `Engine.TerrainSector` (package version 100).
//!
//! Layouts established from Plage00/Plage01 bytes (no reference reader covers XIII terrain):
//!
//! ```text
//! TerrainInfo (after the actor's tagged properties):
//!   TArray<compact TerrainSector ref> Sectors
//!   TArray<FVector>                   Vertices   (world space, HeightmapX * HeightmapY,
//!                                                 row-major, X fastest)
//!   i32 SectorsX, i32 SectorsY
//!   ... matrices / heightmap size / per-vertex data: NOT decoded (explicit unsupported tail)
//!
//! TerrainSector:
//!   compact TerrainInfo ref; i32 QuadsX, QuadsY, OffsetX, OffsetY
//!   FVector[8]                         bounding-box corners
//!   TArray<{i16 light index; TArray<u8> visibility bits}> light infos (the index is not an
//!                                      object reference: values up to 0x42 seen with a
//!                                      zero high byte; target list unknown)
//!   TArray<FColor>                     vertex colours ((QuadsX+1) * (QuadsY+1))
//!
//! TerrainLayer struct property (raw members, 37 bytes with two-byte references):
//!   compact Texture; compact AlphaMap; f32 UScale; f32 VScale; 25 bytes (all zero in the
//!   inspected maps; UPan/VPan/axis/rotation by analogy, unverified)
//! ```

use std::fmt::Write as _;

use xiii_package::{ObjectRef, Package, Span};

use crate::common::{
    DecodeError, DecodeErrorKind, DecodeResult, PayloadReader, PayloadReport, Props,
    read_properties,
};

/// Class path of terrain infos.
pub const TERRAIN_INFO_CLASS: &str = "Engine.TerrainInfo";
/// Class path of terrain sectors.
pub const TERRAIN_SECTOR_CLASS: &str = "Engine.TerrainSector";

/// One texture layer.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainLayer {
    /// Static-array index in `Layers`.
    pub index: u32,
    /// Layer material/texture.
    pub texture: ObjectRef,
    /// Alpha (weight) map texture.
    pub alpha_map: ObjectRef,
    /// U scale.
    pub u_scale: f32,
    /// V scale.
    pub v_scale: f32,
    /// Remaining bytes (meaning unverified).
    pub rest: Vec<u8>,
}

/// Decoded terrain info.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainInfo {
    /// `TerrainMap` heightmap texture (G16).
    pub terrain_map: Option<ObjectRef>,
    /// `TerrainScale`.
    pub terrain_scale: Option<[f32; 3]>,
    /// Actor `Location`.
    pub location: Option<[f32; 3]>,
    /// Layers.
    pub layers: Vec<TerrainLayer>,
    /// `QuadVisibilityBitmap` (u32 words; bit = visible).
    pub quad_visibility: Vec<u32>,
    /// `EdgeTurnBitmap` (u32 words; bit = flip the quad diagonal).
    pub edge_turn: Vec<u32>,
    /// Sector objects.
    pub sectors: Vec<ObjectRef>,
    /// World-space vertices.
    pub vertices: Vec<[f32; 3]>,
    /// Sector grid.
    pub sectors_xy: [i32; 2],
    /// Byte accounting (with the unsupported tail).
    pub report: PayloadReport,
}

/// Light visibility of a sector.
#[derive(Debug, Clone, PartialEq)]
pub struct SectorLight {
    /// Light index (target list not identified).
    pub light: i16,
    /// Per-vertex visibility bits.
    pub bits: Vec<u8>,
}

/// Decoded terrain sector.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainSector {
    /// Owning TerrainInfo.
    pub info: ObjectRef,
    /// Quads in X / Y.
    pub quads: [i32; 2],
    /// Offset of the first quad in the heightmap.
    pub offset: [i32; 2],
    /// Bounding-box corners.
    pub corners: [[f32; 3]; 8],
    /// Light visibility.
    pub lights: Vec<SectorLight>,
    /// Vertex colours as stored.
    pub colors: Vec<[u8; 4]>,
    /// Byte accounting.
    pub report: PayloadReport,
}

fn words(data: &[u8], span: Span, count: u32) -> Option<Vec<u32>> {
    let bytes = data.get(span.start..span.end)?;
    if bytes.len() != count as usize * 4 {
        return None;
    }
    Some(
        bytes
            .as_chunks::<4>()
            .0
            .iter()
            .map(|c| u32::from_le_bytes(*c))
            .collect(),
    )
}

/// Decodes a TerrainInfo (sectors, vertices, sector grid; the rest is an explicit tail).
pub fn decode_terrain_info(
    package: &Package,
    data: &[u8],
    export: usize,
) -> DecodeResult<TerrainInfo> {
    let props = read_properties(package, data, export, TERRAIN_INFO_CLASS)?;
    let ctx = |e: DecodeError| e.in_export(package, export);
    let p = Props::new(package, &props);
    let mut layers = Vec::new();
    for prop in &props.block.properties {
        if !package.property_name(prop).eq_ignore_ascii_case("Layers") {
            continue;
        }
        let span = prop.value_span;
        let mut r = PayloadReader::new(data, span, span.start).map_err(ctx)?;
        let layer = (|| -> DecodeResult<TerrainLayer> {
            let texture = r.object_ref(package)?;
            let alpha_map = r.object_ref(package)?;
            let u_scale = r.f32()?;
            let v_scale = r.f32()?;
            let rest = r.bytes(r.remaining())?.to_vec();
            Ok(TerrainLayer {
                index: prop.array_index,
                texture,
                alpha_map,
                u_scale,
                v_scale,
                rest,
            })
        })()
        .map_err(|e| ctx(e.in_field("Layers")))?;
        if layer.rest.len() != 25 {
            return Err(ctx(DecodeError::at(
                DecodeErrorKind::BadProperty {
                    name: "Layers",
                    reason: format!("{} bytes after the scales, expected 25", layer.rest.len()),
                },
                span.start,
            )));
        }
        layers.push(layer);
    }
    let bitmap = |name: &'static str| -> DecodeResult<Vec<u32>> {
        match p.array(name) {
            None => Ok(Vec::new()),
            Some((count, span)) => words(data, span, count).ok_or_else(|| {
                ctx(DecodeError::at(
                    DecodeErrorKind::BadProperty {
                        name,
                        reason: format!("{count} elements in {} bytes", span.len()),
                    },
                    span.start,
                ))
            }),
        }
    };
    let quad_visibility = bitmap("QuadVisibilityBitmap")?;
    let edge_turn = bitmap("EdgeTurnBitmap")?;

    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    let sectors = r
        .array("sectors", 1, |r| r.object_ref(package))
        .map_err(ctx)?;
    let vertices = r.array("vertices", 12, |r| r.vec3()).map_err(ctx)?;
    let sx = r.i32().map_err(|e| ctx(e.in_field("sectors_x")))?;
    let sy = r.i32().map_err(|e| ctx(e.in_field("sectors_y")))?;
    if sx < 0 || sy < 0 || (sx as i64) * (sy as i64) != sectors.len() as i64 {
        return Err(ctx(DecodeError::at(
            DecodeErrorKind::Invalid(format!(
                "sector grid {sx}x{sy} for {} sectors",
                sectors.len()
            )),
            r.pos(),
        )));
    }
    let report = r
        .finish_with_unsupported_tail("terrain_info.after_sector_grid", props.block.span.end)
        .map_err(ctx)?;
    Ok(TerrainInfo {
        terrain_map: p.object("TerrainMap"),
        terrain_scale: p.vector("TerrainScale"),
        location: p.vector("Location"),
        layers,
        quad_visibility,
        edge_turn,
        sectors,
        vertices,
        sectors_xy: [sx, sy],
        report,
    })
}

/// Decodes a TerrainSector completely.
pub fn decode_sector(package: &Package, data: &[u8], export: usize) -> DecodeResult<TerrainSector> {
    let props = read_properties(package, data, export, TERRAIN_SECTOR_CLASS)?;
    let ctx = |e: DecodeError| e.in_export(package, export);
    let mut r = PayloadReader::after_properties(data, &props).map_err(ctx)?;
    let s = (|| -> DecodeResult<TerrainSector> {
        let info = r.object_ref(package)?;
        let quads = [r.i32()?, r.i32()?];
        let offset = [r.i32()?, r.i32()?];
        if quads
            .iter()
            .chain(&offset)
            .any(|&v| !(0..=4096).contains(&v))
        {
            return Err(DecodeError::at(
                DecodeErrorKind::Invalid(format!("quads {quads:?} offset {offset:?}")),
                r.pos(),
            ));
        }
        let mut corners = [[0.0; 3]; 8];
        for c in &mut corners {
            *c = r.vec3()?;
        }
        let lights = r.array("lights", 3, |r| {
            Ok(SectorLight {
                light: r.i16()?,
                bits: r.array("lights.bits", 1, |r| r.u8())?,
            })
        })?;
        let colors = r.array("colors", 4, |r| {
            let b = r.bytes(4)?;
            Ok([b[0], b[1], b[2], b[3]])
        })?;
        let expected = ((quads[0] + 1) * (quads[1] + 1)) as usize;
        if colors.len() != expected && !colors.is_empty() {
            return Err(DecodeError::at(
                DecodeErrorKind::Invalid(format!(
                    "{} vertex colours, expected {expected}",
                    colors.len()
                )),
                r.pos(),
            )
            .in_field("colors"));
        }
        Ok(TerrainSector {
            info,
            quads,
            offset,
            corners,
            lights,
            colors,
            report: PayloadReport {
                payload: r.payload(),
                properties_end: 0,
                unknown: Vec::new(),
                unsupported_tail: None,
            },
        })
    })()
    .map_err(ctx)?;
    let report = r.finish(props.block.span.end).map_err(ctx)?;
    Ok(TerrainSector { report, ..s })
}

/// Heightfield mesh (source coordinates).
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainMesh {
    /// Grid width (vertices).
    pub width: usize,
    /// Grid height (vertices).
    pub height: usize,
    /// Vertices (world space, source coordinates).
    pub positions: Vec<[f32; 3]>,
    /// Normalized grid coordinates (0..1) for alpha maps.
    pub grid_uv: Vec<[f32; 2]>,
    /// Triangle list, source winding (front faces have clockwise numeric order like
    /// static meshes; see [`TerrainInfo::mesh`]).
    pub indices: Vec<u32>,
    /// Quads skipped because `QuadVisibilityBitmap` marks them invisible (holes).
    pub hidden_quads: usize,
}

impl TerrainInfo {
    /// Builds the heightfield triangles from the stored world-space vertices.
    /// `width` x `height` is the heightmap size (from the `TerrainMap` texture).
    ///
    /// Quad `(x, y)` uses bit `y * width + x` of `QuadVisibilityBitmap` (holes when clear)
    /// and of `EdgeTurnBitmap` (diagonal choice). Both bit conventions are inferred from UE2
    /// naming; Plage00's visibility bitmap is all ones, so holes are not yet verified.
    pub fn mesh(&self, width: usize, height: usize) -> DecodeResult<TerrainMesh> {
        if width < 2 || height < 2 || width * height != self.vertices.len() {
            return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
                "heightmap {width}x{height} does not match {} vertices",
                self.vertices.len()
            ))));
        }
        let bit = |bits: &[u32], i: usize| -> Option<bool> {
            bits.get(i / 32).map(|w| (w >> (i % 32)) & 1 == 1)
        };
        let mut indices = Vec::new();
        let mut hidden = 0;
        for y in 0..height - 1 {
            for x in 0..width - 1 {
                let q = y * width + x;
                if bit(&self.quad_visibility, q) == Some(false) {
                    hidden += 1;
                    continue;
                }
                let (a, b, c, d) = (q, q + 1, q + width, q + width + 1);
                let (a, b, c, d) = (a as u32, b as u32, c as u32, d as u32);
                // Rows run along +Y and columns along +X (checked on Plage00/01). The order
                // makes (b-a)x(c-a) point to -Z, the same convention as static-mesh front
                // faces (numeric normal against the visible side in source coordinates).
                if bit(&self.edge_turn, q) == Some(true) {
                    indices.extend_from_slice(&[a, c, b, b, c, d]);
                } else {
                    indices.extend_from_slice(&[a, d, b, a, c, d]);
                }
            }
        }
        let grid_uv = (0..height)
            .flat_map(|y| {
                (0..width).map(move |x| {
                    [
                        x as f32 / (width - 1) as f32,
                        y as f32 / (height - 1) as f32,
                    ]
                })
            })
            .collect();
        Ok(TerrainMesh {
            width,
            height,
            positions: self.vertices.clone(),
            grid_uv,
            indices,
            hidden_quads: hidden,
        })
    }
}

/// Text summary.
pub fn info_summary(package: &Package, export: usize, t: &TerrainInfo) -> String {
    let mut out = String::new();
    let name = |r: ObjectRef| package.object_path(r).unwrap_or("None").to_owned();
    let _ = writeln!(
        out,
        "{} map {:?} scale {:?} location {:?} sectors {} ({}x{}) vertices {}",
        package
            .object_path(ObjectRef::Export(export as u32))
            .unwrap_or("?"),
        t.terrain_map.map(name),
        t.terrain_scale,
        t.location,
        t.sectors.len(),
        t.sectors_xy[0],
        t.sectors_xy[1],
        t.vertices.len()
    );
    for l in &t.layers {
        let _ = writeln!(
            out,
            "  layer {}: texture {} alpha {} scale {}x{} rest-nonzero {}",
            l.index,
            name(l.texture),
            name(l.alpha_map),
            l.u_scale,
            l.v_scale,
            l.rest.iter().any(|&b| b != 0)
        );
    }
    let vis = t
        .quad_visibility
        .iter()
        .map(|w| w.count_zeros())
        .sum::<u32>();
    let turn = t.edge_turn.iter().map(|w| w.count_ones()).sum::<u32>();
    let _ = writeln!(
        out,
        "  QuadVisibilityBitmap {} words ({} zero bits); EdgeTurnBitmap {} words ({} set bits)",
        t.quad_visibility.len(),
        vis,
        t.edge_turn.len(),
        turn
    );
    if let Some((label, span)) = t.report.unsupported_tail {
        let _ = writeln!(
            out,
            "  UNSUPPORTED TAIL {label}: {} bytes at 0x{:x} (payload+{})",
            span.len(),
            span.start,
            span.start - t.report.payload.start
        );
    }
    out
}

/// Text summary.
pub fn sector_summary(package: &Package, export: usize, s: &TerrainSector) -> String {
    format!(
        "{} quads {:?} offset {:?} lights {} colours {}\n",
        package
            .object_path(ObjectRef::Export(export as u32))
            .unwrap_or("?"),
        s.quads,
        s.offset,
        s.lights.len(),
        s.colors.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::test_package::{Builder, Bytes, parse};

    fn sector_payload(colors: i32) -> Vec<u8> {
        let mut b = Bytes::default().c(0).c(0).i32(1).i32(1).i32(0).i32(0);
        for k in 0..8 {
            b = b.v3([k as f32, 0.0, 0.0]);
        }
        b = b.c(1).i16(0x42).c(1).u8(0xff).c(colors);
        for _ in 0..colors {
            b = b.raw(&[1, 2, 3, 255]);
        }
        b.0
    }

    #[test]
    fn synthetic_sector_and_colour_count_check() {
        let mut b = Builder::new();
        let i = b.export("TerrainSector", "S", sector_payload(4));
        let j = b.export("TerrainSector", "T", sector_payload(3));
        let bytes = b.build();
        let p = parse(&bytes);
        let s = decode_sector(&p, &bytes, i).unwrap();
        assert_eq!(
            (s.quads, s.lights[0].light, s.colors.len()),
            ([1, 1], 0x42, 4)
        );
        assert!(
            decode_sector(&p, &bytes, j).is_err(),
            "3 colours for a 2x2 vertex sector"
        );
        let full = sector_payload(4);
        for cut in 1..full.len() - 1 {
            let mut b = Builder::new();
            let k = b.export("TerrainSector", "S", full[..full.len() - cut].to_vec());
            let bytes = b.build();
            let p = parse(&bytes);
            assert!(decode_sector(&p, &bytes, k).is_err(), "cut {cut}");
        }
    }

    #[test]
    fn heightfield_triangles_face_up_after_conversion() {
        let mut vertices = Vec::new();
        for y in 0..3 {
            for x in 0..3 {
                vertices.push([x as f32 * 10.0, y as f32 * 10.0, 0.0]);
            }
        }
        let info = TerrainInfo {
            terrain_map: None,
            terrain_scale: None,
            location: None,
            layers: Vec::new(),
            quad_visibility: vec![!0b10],
            edge_turn: vec![0b100],
            sectors: Vec::new(),
            vertices,
            sectors_xy: [0, 0],
            report: PayloadReport {
                payload: Span { start: 0, end: 0 },
                properties_end: 0,
                unknown: Vec::new(),
                unsupported_tail: None,
            },
        };
        let m = info.mesh(3, 3).unwrap();
        assert_eq!(m.hidden_quads, 1);
        assert_eq!(m.indices.len(), 3 * 6);
        for t in m.indices.as_chunks::<3>().0 {
            let p: Vec<[f32; 3]> = t
                .iter()
                .map(|&i| crate::common::to_bevy_position(m.positions[i as usize]))
                .collect();
            let n = crate::common::triangle_cross(p[0], p[1], p[2]);
            assert!(n[1] > 0.0, "Bevy CCW normal points up (+Y): {n:?}");
        }
        assert!(info.mesh(4, 2).is_err());
    }
}
