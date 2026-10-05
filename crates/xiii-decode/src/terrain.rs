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

/// One rectangular heightfield region of a [`TerrainInfo`] (source coordinates).
///
/// A `TerrainInfo` stores more than one region when its native `Vertices` array is longer than
/// `HeightmapX * HeightmapY`: the array is a sequence of concatenated row-major grids at
/// different spacings, e.g. Hual04c `TerrainInfo1` is a 64x64 grid (spacing 250) followed by a
/// 128x96 grid (spacing 100). Treating the whole array as one grid produced the reported
/// `heightmap WxH does not match N vertices` failures.
#[derive(Debug, Clone, PartialEq)]
pub struct TerrainMesh {
    /// Grid width (vertices).
    pub width: usize,
    /// Grid height in rows.
    pub height: usize,
    /// Base region index (0) or a detail region (1..). Only region 0 is described by the
    /// `TerrainMap` texture, the sectors and the visibility/edge bitmaps.
    pub region: usize,
    /// Vertices (world space, source coordinates), `width * height`.
    pub positions: Vec<[f32; 3]>,
    /// Normalized grid coordinates (0..1) for alpha maps.
    pub grid_uv: Vec<[f32; 2]>,
    /// Triangle list, source winding (front faces have clockwise numeric order like
    /// static meshes; see [`TerrainInfo::mesh`]).
    pub indices: Vec<u32>,
    /// Quads skipped because `QuadVisibilityBitmap` marks them invisible (holes).
    pub hidden_quads: usize,
}

/// Tolerance for grouping vertices into a rectangular grid. Vertex spacing is at least tens of
/// Unreal units, so a sub-unit tolerance cannot merge distinct rows/columns.
const GRID_EPS: f32 = 1e-3;

impl TerrainInfo {
    /// Builds the heightfield as its concatenated rectangular regions.
    ///
    /// Each region is a row-major grid: every row has a constant `Y`, strictly increasing `X`
    /// with a constant step, and the row `Y` advances by a constant step. A region ends when the
    /// next row's first `X`, constant `Y` or row step no longer matches (verified byte-exactly on
    /// every campaign terrain). The first (base) region is the one the `TerrainMap` texture, the
    /// sectors and the bitmaps describe; later regions are extra detail geometry.
    ///
    /// `width` x `height` is the base region's size (from the `TerrainMap` texture). A base
    /// mismatch is an error, never a silent skip.
    ///
    /// For the base region, quad `(x, y)` uses bit `y * width + x` of `QuadVisibilityBitmap`
    /// (holes when clear) and of `EdgeTurnBitmap` (diagonal choice); the bit convention is
    /// inferred from UE2 naming (Plage00's visibility bitmap is all ones, so holes are not yet
    /// verified against the original). Detail regions have no bitmap and are fully drawn.
    pub fn mesh(&self, width: usize, height: usize) -> DecodeResult<Vec<TerrainMesh>> {
        let v = &self.vertices;
        let n = v.len();
        if n == 0 {
            return Err(DecodeError::new(DecodeErrorKind::Invalid(
                "terrain has no vertices".into(),
            )));
        }
        let bit = |bits: &[u32], i: usize| -> Option<bool> {
            bits.get(i / 32).map(|w| (w >> (i % 32)) & 1 == 1)
        };
        let mut regions = Vec::new();
        let mut i = 0usize;
        while i < n {
            // Width of the first row: constant Y, strictly increasing X.
            let (y0, x0) = (v[i][1], v[i][0]);
            let mut w = 1usize;
            while i + w < n
                && (v[i + w][1] - y0).abs() <= GRID_EPS
                && v[i + w][0] > v[i + w - 1][0] + GRID_EPS
            {
                w += 1;
            }
            if w < 2 {
                return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
                    "terrain region at vertex {i} has width {w}; not a heightfield row"
                ))));
            }
            // Rows: the step from row 0 to row 1 fixes the expected `Y` of every later row.
            let mut rows = 1usize;
            if i + w < n && v[i + w][1] - y0 > GRID_EPS {
                let dy = v[i + w][1] - y0;
                loop {
                    let base = i + rows * w;
                    if base + w > n {
                        break;
                    }
                    let ry = v[base][1];
                    if (ry - (y0 + rows as f32 * dy)).abs() > GRID_EPS
                        || (v[base][0] - x0).abs() > GRID_EPS
                    {
                        break;
                    }
                    let mut ok = true;
                    for c in 0..w {
                        if (v[base + c][1] - ry).abs() > GRID_EPS
                            || (c > 0 && v[base + c][0] <= v[base + c - 1][0] + GRID_EPS)
                        {
                            ok = false;
                            break;
                        }
                    }
                    if !ok {
                        break;
                    }
                    rows += 1;
                }
            }
            if rows < 2 {
                return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
                    "terrain region at vertex {i} is {w}x{rows}; a heightfield needs 2 rows"
                ))));
            }
            let region = regions.len();
            regions.push(build_region(self, &bit, i, w, rows, region, width));
            i += w * rows;
        }
        let Some(base) = regions.first() else {
            return Err(DecodeError::new(DecodeErrorKind::Invalid(
                "terrain has no regions".into(),
            )));
        };
        if base.width != width || base.height != height {
            return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
                "heightmap {width}x{height} does not match base region {}x{} ({} vertices)",
                base.width, base.height, n
            ))));
        }
        Ok(regions)
    }
}

/// Builds one region's triangles, UVs and hidden-quad count. `start` is the first vertex index;
/// `base_width` is the base region's width, used only to index the bitmaps for region 0.
fn build_region(
    info: &TerrainInfo,
    bit: &impl Fn(&[u32], usize) -> Option<bool>,
    start: usize,
    width: usize,
    rows: usize,
    region: usize,
    base_width: usize,
) -> TerrainMesh {
    let positions = info.vertices[start..start + width * rows].to_vec();
    let mut indices = Vec::new();
    let mut hidden = 0;
    for y in 0..rows - 1 {
        for x in 0..width - 1 {
            if region == 0 {
                let q = y * base_width + x;
                if bit(&info.quad_visibility, q) == Some(false) {
                    hidden += 1;
                    continue;
                }
            }
            let (a, b, c, d) = (
                y * width + x,
                y * width + x + 1,
                (y + 1) * width + x,
                (y + 1) * width + x + 1,
            );
            let (a, b, c, d) = (a as u32, b as u32, c as u32, d as u32);
            // Rows run along +Y and columns along +X (checked on Plage00/01). The order makes
            // (b-a)x(c-a) point to -Z, the same convention as static-mesh front faces (numeric
            // normal against the visible side in source coordinates).
            let turn = region == 0 && bit(&info.edge_turn, y * base_width + x) == Some(true);
            if turn {
                indices.extend_from_slice(&[a, c, b, b, c, d]);
            } else {
                indices.extend_from_slice(&[a, d, b, a, c, d]);
            }
        }
    }
    let grid_uv = (0..rows)
        .flat_map(|y| {
            (0..width).map(move |x| [x as f32 / (width - 1) as f32, y as f32 / (rows - 1) as f32])
        })
        .collect();
    TerrainMesh {
        width,
        height: rows,
        region,
        positions,
        grid_uv,
        indices,
        hidden_quads: hidden,
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

/// Per-vertex colour grid assembled from the sectors of one terrain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerrainColorGrid {
    /// `width * height` colours as stored (BGRA), row-major with X fastest.
    pub colors: Vec<[u8; 4]>,
    /// Vertices written by two sectors with **different** colours (shared borders that agree
    /// are not counted).
    pub conflicts: usize,
    /// Vertices no sector covered (`None` cells left at the end).
    pub missing: usize,
}

impl TerrainColorGrid {
    /// Colour `i` converted to RGBA (blue and red swapped).
    pub fn rgba(&self, i: usize) -> [u8; 4] {
        let c = self.colors[i];
        [c[2], c[1], c[0], c[3]]
    }
}

/// Assembles the `width x height` per-vertex colour grid of `info` from its sectors.
///
/// Sector `offset` is a vertex offset into the heightmap grid; the sector's colours are
/// `(QuadsX+1) x (QuadsY+1)` row-major with X fastest. The measured Plage00/Plage01 sectors
/// tile the grid: the quad counts sum to `width - 1` / `height - 1`. Non-export sector
/// references and out-of-range cells are errors, never dropped.
pub fn color_grid(
    package: &Package,
    data: &[u8],
    info: &TerrainInfo,
    width: usize,
    height: usize,
) -> DecodeResult<TerrainColorGrid> {
    // Sectors index the base heightmap grid only; a `Vertices` array longer than `width *
    // height` carries an additional detail region (see [`TerrainInfo::mesh`]) and is not an
    // error here.
    if width == 0 || height == 0 || width * height > info.vertices.len() {
        return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
            "heightmap {width}x{height} needs {} vertices, terrain has {}",
            width * height,
            info.vertices.len()
        ))));
    }
    let mut cells: Vec<Option<[u8; 4]>> = vec![None; width * height];
    let mut conflicts = 0usize;
    for (si, r) in info.sectors.iter().enumerate() {
        let export = match r {
            ObjectRef::Export(e) => *e as usize,
            other => {
                return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
                    "sector {si} is {other:?}, not an export of this package"
                ))));
            }
        };
        let s = decode_sector(package, data, export)?;
        let cols = (s.quads[0] + 1) as usize;
        let rows = (s.quads[1] + 1) as usize;
        if s.colors.len() != cols * rows {
            return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
                "sector {si} has {} colours for {cols}x{rows}",
                s.colors.len()
            ))));
        }
        for y in 0..rows {
            for x in 0..cols {
                let gx = s.offset[0] as i64 + x as i64;
                let gy = s.offset[1] as i64 + y as i64;
                if gx < 0 || gy < 0 || gx as usize >= width || gy as usize >= height {
                    return Err(DecodeError::new(DecodeErrorKind::Invalid(format!(
                        "sector {si} vertex ({gx}, {gy}) outside {width}x{height}"
                    ))));
                }
                let idx = gy as usize * width + gx as usize;
                let c = s.colors[y * cols + x];
                match cells[idx] {
                    None => cells[idx] = Some(c),
                    // Adjacent sectors share their border vertex column/row; a repeated write
                    // is only a conflict when it disagrees.
                    Some(prev) if prev != c => conflicts += 1,
                    Some(_) => {}
                }
            }
        }
    }
    let missing = cells.iter().filter(|c| c.is_none()).count();
    Ok(TerrainColorGrid {
        colors: cells.into_iter().map(|c| c.unwrap_or([0; 4])).collect(),
        conflicts,
        missing,
    })
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
        let m = &info.mesh(3, 3).unwrap()[0];
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

    /// Sector payload with configurable quads/offset and per-vertex BGRA colours (offset by
    /// `bias`, so overlapping sectors can be made to agree or disagree).
    fn sector_grid(quads: [i32; 2], offset: [i32; 2], bias: u8) -> Vec<u8> {
        let cols = (quads[0] + 1) as usize;
        let rows = (quads[1] + 1) as usize;
        let mut b = Bytes::default().c(0).c(0).i32(quads[0]).i32(quads[1]);
        b = b.i32(offset[0]).i32(offset[1]);
        for k in 0..8 {
            b = b.v3([k as f32, 0.0, 0.0]);
        }
        b = b.c(0).c((cols * rows) as i32);
        for y in 0..rows {
            for x in 0..cols {
                b = b.raw(&[
                    (x as u8).wrapping_add(bias),
                    (y as u8).wrapping_add(bias),
                    (x + y) as u8,
                    255,
                ]);
            }
        }
        b.0
    }

    fn empty_info(sectors: Vec<ObjectRef>, width: usize, height: usize) -> TerrainInfo {
        TerrainInfo {
            terrain_map: None,
            terrain_scale: None,
            location: None,
            layers: Vec::new(),
            quad_visibility: Vec::new(),
            edge_turn: Vec::new(),
            sectors,
            vertices: vec![[0.0; 3]; width * height],
            sectors_xy: [1, 1],
            report: PayloadReport {
                payload: Span { start: 0, end: 0 },
                properties_end: 0,
                unknown: Vec::new(),
                unsupported_tail: None,
            },
        }
    }

    /// Builds a `TerrainInfo` from explicit vertices and no properties, for mesh-region tests.
    fn info_with(vertices: Vec<[f32; 3]>) -> TerrainInfo {
        TerrainInfo {
            terrain_map: None,
            terrain_scale: None,
            location: None,
            layers: Vec::new(),
            quad_visibility: Vec::new(),
            edge_turn: Vec::new(),
            sectors: Vec::new(),
            vertices,
            sectors_xy: [0, 0],
            report: PayloadReport {
                payload: Span { start: 0, end: 0 },
                properties_end: 0,
                unknown: Vec::new(),
                unsupported_tail: None,
            },
        }
    }

    /// Row-major grid at `step`, top-left at `origin`.
    fn grid(
        origin: [f32; 3],
        w: usize,
        h: usize,
        step: f32,
        z: impl Fn(usize, usize) -> f32,
    ) -> Vec<[f32; 3]> {
        let mut v = Vec::with_capacity(w * h);
        for y in 0..h {
            for x in 0..w {
                v.push([
                    origin[0] + x as f32 * step,
                    origin[1] + y as f32 * step,
                    z(x, y),
                ]);
            }
        }
        v
    }

    /// The key decode bug: a `TerrainInfo` whose `Vertices` hold a base region plus a
    /// differently-spaced detail region must split, not fail with a vertex-count mismatch.
    #[test]
    fn mesh_splits_concatenated_regions() {
        let mut vertices = grid([0.0, 0.0, 0.0], 4, 3, 250.0, |_, _| 0.0);
        vertices.extend(grid([0.0, 0.0, 0.0], 8, 6, 100.0, |x, y| (x + y) as f32));
        let info = info_with(vertices);
        let regions = info.mesh(4, 3).unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(
            (regions[0].width, regions[0].height, regions[0].region),
            (4, 3, 0)
        );
        assert_eq!(
            (regions[1].width, regions[1].height, regions[1].region),
            (8, 6, 1)
        );
        // Triangles per region: 2*(w-1)*(h-1) quads * 3 indices.
        assert_eq!(regions[0].indices.len(), 2 * 3 * 2 * 3);
        assert_eq!(regions[1].indices.len(), 2 * 7 * 5 * 3);
        assert_eq!(regions[0].positions[0], [0.0, 0.0, 0.0]);
        // A base-region size that does not match the texture is still an error.
        assert!(info.mesh(4, 4).is_err());
        assert!(info.mesh(3, 3).is_err());
    }

    /// A single region still decodes; and the base region's hidden-quad bits are read from the
    /// bitmap while a detail region (with no bitmap) is fully drawn.
    #[test]
    fn mesh_region_bitmap_applies_only_to_base() {
        let mut vertices = grid([0.0, 0.0, 0.0], 3, 3, 10.0, |_, _| 0.0);
        vertices.extend(grid([100.0, 0.0, 0.0], 3, 3, 10.0, |_, _| 0.0));
        let mut info = info_with(vertices);
        // Hide quad 0 of the base (bit 0 clear) and flip quad 1's diagonal (bit 1 set).
        info.quad_visibility = vec![!0b1];
        info.edge_turn = vec![0b10];
        let regions = info.mesh(3, 3).unwrap();
        assert_eq!(regions.len(), 2);
        assert_eq!(regions[0].hidden_quads, 1);
        assert_eq!(regions[1].hidden_quads, 0);
        assert_eq!(regions[0].indices.len(), (4 - 1) * 2 * 3);
        assert_eq!(regions[1].indices.len(), 4 * 2 * 3);
    }

    /// A row whose `Y` does not advance, or a truncated array, must be an error, not a silent
    /// partial grid.
    #[test]
    fn mesh_rejects_degenerate_and_truncated_regions() {
        // All vertices share one row: no second row.
        let flat = info_with(grid([0.0, 0.0, 0.0], 4, 1, 10.0, |_, _| 0.0));
        assert!(flat.mesh(4, 1).is_err());
        // Region claims 4x3 but the tail is cut to two rows.
        let mut cut = grid([0.0, 0.0, 0.0], 4, 3, 10.0, |_, _| 0.0);
        cut.truncate(8);
        assert!(info_with(cut).mesh(3, 2).is_err());
        // Empty vertex list.
        assert!(info_with(Vec::new()).mesh(2, 2).is_err());
    }

    #[test]
    fn color_grid_maps_sector_cells() {
        // 4x2 vertex grid; sector covers quads (0..2)x(0..2) at offset (2,0).
        let mut b = Builder::new();
        let s = b.export("TerrainSector", "S", sector_grid([1, 1], [2, 0], 0));
        let bytes = b.build();
        let p = parse(&bytes);
        let info = empty_info(vec![ObjectRef::Export(s as u32)], 4, 2);
        let grid = color_grid(&p, &bytes, &info, 4, 2).unwrap();
        assert_eq!(grid.colors.len(), 8);
        assert_eq!(grid.conflicts, 0);
        // Uncovered cells stay zero.
        assert!(grid.colors[..2].iter().all(|c| *c == [0, 0, 0, 0]));
        // Global (2,0) = local (0,0) = [0,0,0,255]; (3,1) = local (1,1) = [1,1,2,255].
        assert_eq!(grid.colors[2], [0, 0, 0, 255]);
        assert_eq!(grid.colors[7], [1, 1, 2, 255]);
        // A sector that runs past the grid is rejected, not clipped.
        let mut b = Builder::new();
        let s = b.export("TerrainSector", "S", sector_grid([1, 1], [3, 0], 0));
        let bytes = b.build();
        let p = parse(&bytes);
        let info = empty_info(vec![ObjectRef::Export(s as u32)], 4, 2);
        assert!(color_grid(&p, &bytes, &info, 4, 2).is_err());
    }

    #[test]
    fn color_grid_reports_conflicts_and_missing() {
        // Two sectors over the same cells with different colours disagree at every vertex.
        let mut b = Builder::new();
        let s0 = b.export("TerrainSector", "A", sector_grid([1, 1], [0, 0], 0));
        let s1 = b.export("TerrainSector", "B", sector_grid([1, 1], [0, 0], 7));
        let bytes = b.build();
        let p = parse(&bytes);
        let info = empty_info(
            vec![ObjectRef::Export(s0 as u32), ObjectRef::Export(s1 as u32)],
            4,
            2,
        );
        let grid = color_grid(&p, &bytes, &info, 4, 2).unwrap();
        assert_eq!(grid.conflicts, 4);
        assert_eq!(grid.missing, 4);
    }
}
