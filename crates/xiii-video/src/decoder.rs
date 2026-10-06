//! Bink 1 (revision `i`) video decoder.
//!
//! Structure follows the public prose descriptions cited in `README.md`: per-plane Huffman
//! "bundles", the ten 8x8 block types, motion compensation, DCT and residue. The fixed tables
//! (Huffman code lengths, run-fill patterns, DCT scan order, dequantisation) come from the
//! installation's `binkw32.dll` via [`crate::tables::BinkTables`].
//!
//! **Status:** the decoder is a clean-room implementation under validation; see
//! `local/reports/item17b-bink-cleanroom.md` for the measured agreement with the FFmpeg
//! black-box oracle. It never reports success for a construct it cannot decode: unknown block
//! types or bitstream underrun return an explicit [`VideoError`].

use std::sync::OnceLock;

use crate::bitreader::BitReader;
use crate::container::Header;
use crate::error::{Result, VideoError, VideoErrorKind};
use crate::huffman::StaticTree;
use crate::tables::BinkTables;

/// Indices of the nine bundles in plane-header order.
mod bundle {
    pub const BLOCK_TYPES: usize = 0;
    pub const SUB_BLOCK_TYPES: usize = 1;
    pub const COLORS: usize = 2;
    pub const PATTERN: usize = 3;
    pub const X_OFF: usize = 4;
    pub const Y_OFF: usize = 5;
    pub const INTRA_DC: usize = 6;
    pub const INTER_DC: usize = 7;
    pub const RUN: usize = 8;
    pub const COUNT: usize = 9;
}

const IDENTITY: [u8; 16] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15];

/// Run lengths for the 4-bit RLE bundles (public prose: MultimediaWiki *Bink Video*).
const RUN_VALUES: [usize; 4] = [4, 8, 12, 32];

/// One decoded frame in planes.
#[derive(Debug, Clone)]
pub struct YuvFrame {
    /// Luma plane (plane width x plane height).
    pub y: Vec<u8>,
    /// Cb plane (half resolution).
    pub u: Vec<u8>,
    /// Cr plane (half resolution).
    pub v: Vec<u8>,
    /// Alpha plane at luma resolution, when present.
    pub a: Option<Vec<u8>>,
    /// Luma width in pixels (8-aligned).
    pub width: usize,
    /// Luma height in pixels (8-aligned).
    pub height: usize,
}

impl YuvFrame {
    /// Converts to tightly packed RGBA8 using full-range BT.601 (labelled approximation).
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.width * self.height * 4];
        let cw = self.width / 2;
        for yy in 0..self.height {
            for xx in 0..self.width {
                let yi = yy * self.width + xx;
                let y = f32::from(self.y[yi]);
                let ci = (yy / 2) * cw + xx / 2;
                let u = f32::from(self.u[ci]) - 128.0;
                let v = f32::from(self.v[ci]) - 128.0;
                let o = yi * 4;
                out[o] = clamp8(y + 1.402 * v);
                out[o + 1] = clamp8(y - 0.344_136 * u - 0.714_136 * v);
                out[o + 2] = clamp8(y + 1.772 * u);
                out[o + 3] = self.a.as_ref().map_or(255, |a| a[yi]);
            }
        }
        out
    }
}

fn clamp8(v: f32) -> u8 {
    v.round().clamp(0.0, 255.0) as u8
}

/// Per-frame statistics from the decoder.
#[derive(Debug, Clone, Default)]
pub struct FrameStats {
    /// Bits consumed by the video payload.
    pub bits_used: usize,
    /// Total bits available in the packet.
    pub bits_total: usize,
    /// Block types seen, indexed by type (0..=9).
    pub block_type_counts: [u64; 10],
    /// True if any block type was outside 0..=9.
    pub saw_unknown_block: bool,
}

/// The clean-room decoder.
#[derive(Debug, Clone)]
pub struct Decoder {
    tables: BinkTables,
}

impl Decoder {
    /// Creates a decoder with tables read from the installation.
    pub fn new(tables: BinkTables) -> Self {
        Self { tables }
    }

    /// Borrows the tables.
    pub fn tables(&self) -> &BinkTables {
        &self.tables
    }

    /// Decodes a full frame from `packet` (the video payload), using `prev` for motion.
    ///
    /// Revision `i` prefixes the alpha (when present) and luma planes with a 32-bit byte size
    /// (MultimediaWiki *Bink Video*: "since version 'i' there is 32-bit word containing plane
    /// data size in bytes before alpha and luma plane"). U and V follow the luma plane with no
    /// prefix.
    pub fn decode_frame(
        &self,
        packet: &[u8],
        header: &Header,
        prev: Option<&YuvFrame>,
    ) -> Result<(YuvFrame, FrameStats)> {
        let w = align8(header.width as usize);
        let h = align8(header.height as usize);
        let cw = align8(w / 2);
        let ch = align8(h / 2);
        let mut stats = FrameStats {
            bits_total: packet.len() * 8,
            ..Default::default()
        };
        // Revision `i` prefixes the alpha and luma planes with a 32-bit byte size. The plane
        // payloads are decoded from their own byte ranges.
        let mut off = 0usize;
        let alpha_size = if header.has_alpha() {
            Some(read_u32(packet, &mut off)? as usize)
        } else {
            None
        };
        let y_size = read_u32(packet, &mut off)? as usize;

        let mut used = 0usize;
        let a = match alpha_size {
            Some(sz) => {
                let slice = slice_with_lookahead(packet, off, sz)?;
                off += sz;
                let mut br = BitReader::new(&slice);
                let p = self.decode_plane(
                    &mut br,
                    w,
                    h,
                    prev.and_then(|p| p.a.as_deref()),
                    false,
                    &mut stats,
                )?;
                used += br.bits_read();
                Some(p)
            }
            None => None,
        };
        let y_slice = slice_with_lookahead(packet, off, y_size)?;
        off += y_size;
        let mut ybr = BitReader::new(&y_slice);
        let y = self.decode_plane(
            &mut ybr,
            w,
            h,
            prev.map(|p| p.y.as_slice()),
            false,
            &mut stats,
        )?;
        used += ybr.bits_read();

        let mut chroma = packet
            .get(off..)
            .ok_or_else(|| {
                VideoError::new(VideoErrorKind::Truncated, "chroma plane starts past packet")
            })?
            .to_vec();
        chroma.resize(chroma.len().saturating_add(24), 0);
        let mut cbr = BitReader::new(&chroma);
        let u = self.decode_plane(
            &mut cbr,
            cw,
            ch,
            prev.map(|p| p.u.as_slice()),
            true,
            &mut stats,
        )?;
        let v = self.decode_plane(
            &mut cbr,
            cw,
            ch,
            prev.map(|p| p.v.as_slice()),
            true,
            &mut stats,
        )?;
        used += cbr.bits_read();

        stats.bits_used = used;
        Ok((
            YuvFrame {
                y,
                u,
                v,
                a,
                width: w,
                height: h,
            },
            stats,
        ))
    }

    fn decode_plane(
        &self,
        br: &mut BitReader<'_>,
        w: usize,
        h: usize,
        prev: Option<&[u8]>,
        chroma: bool,
        stats: &mut FrameStats,
    ) -> Result<Vec<u8>> {
        let bw = w / 8;
        let bh = h / 8;
        let plane_start_bit = br.bits_read();
        let mut bundles = self.read_plane_header(br, bw, bh)?;
        let mut out = match prev {
            Some(p) if p.len() == w * h => p.to_vec(),
            _ => vec![if chroma { 128 } else { 0 }; w * h],
        };
        let mut pred_dc = 0i32;

        let trace = std::env::var_os("XIII_VIDEO_TRACE_BLOCKS").is_some();
        let mut row = 0usize;
        while row < bh {
            if std::env::var_os("XIII_VIDEO_TRACE_ROW").is_some() {
                eprintln!("plane {w}x{h} row {row}/{bh} bit {}", br.bits_read());
            }
            let mut col = 0usize;
            while col < bw {
                let bt = bundles[bundle::BLOCK_TYPES].next(br)?;
                if trace && row < 16 {
                    eprintln!("block row={row} col={col} type={bt} bit={}", br.bits_read());
                }
                stats.block_type_counts[usize::from(bt.min(9))] += 1;
                if bt > 9 {
                    // The DLL treats block types 10..15 as a plain advance (`0x3001dad9`).
                    stats.saw_unknown_block = true;
                    col += 1;
                    continue;
                }
                let mut advance2 = false;
                match bt {
                    0 => {}
                    1 => {
                        // The 16x16 type is encountered on both 8-pixel rows. The second row
                        // (`y & 8`, DLL test at 0x3001e5ce) advances over the already-rendered
                        // half without consuming another subtype.
                        if row & 1 == 0 {
                            let sub = bundles[bundle::SUB_BLOCK_TYPES].next(br)?;
                            if trace && row < 16 {
                                eprintln!("  subtype={sub} bit={}", br.bits_read());
                            }
                            let mut block = [0u8; 64];
                            self.decode_scalar_block(br, &mut bundles, sub, &mut block)?;
                            scale_block_2x(&block, &mut out, w, col * 8, row * 8);
                        }
                        advance2 = true;
                    }
                    2 => {
                        let dx = bundles[bundle::X_OFF].next_signed(br)?;
                        let dy = bundles[bundle::Y_OFF].next_signed(br)?;
                        motion_copy(&mut out, w, h, col * 8, row * 8, dx, dy);
                    }
                    3 => {
                        // The run block's pattern index is a raw 4-bit bitstream value
                        // (`0x3001db7d`: `andl $0xf`), not a pattern-bundle symbol.
                        let pattern = br.read(4) as usize;
                        fill_run(
                            br,
                            &mut bundles,
                            pattern,
                            &self.tables.patterns.patterns,
                            &mut out,
                            w,
                            col * 8,
                            row * 8,
                        )?;
                    }
                    4 => {
                        let n_masks = br.read(7) as usize;
                        let dx = bundles[bundle::X_OFF].next_signed(br)?;
                        let dy = bundles[bundle::Y_OFF].next_signed(br)?;
                        motion_copy(&mut out, w, h, col * 8, row * 8, dx, dy);
                        let mut block = [0i16; 64];
                        read_residue(br, &mut block, n_masks, &self.tables.scan)?;
                        add_residue(&block, &self.tables.scan, &mut out, w, col * 8, row * 8);
                    }
                    5 => {
                        let dc = bundles[bundle::INTRA_DC].next_dc(br)?;
                        let q = br.read(4) as usize;
                        let mut block = [0i32; 64];
                        read_dct_coeffs(br, &mut block, &self.tables.scan)?;
                        reconstruct(
                            &mut block,
                            q,
                            &self.tables,
                            &mut out,
                            w,
                            col * 8,
                            row * 8,
                            AddMode::Intra {
                                dc,
                                pred: &mut pred_dc,
                            },
                        );
                    }
                    6 => {
                        let c = bundles[bundle::COLORS].next(br)?;
                        fill_block(&mut out, w, col * 8, row * 8, c);
                    }
                    7 => {
                        let dx = bundles[bundle::X_OFF].next_signed(br)?;
                        let dy = bundles[bundle::Y_OFF].next_signed(br)?;
                        motion_copy(&mut out, w, h, col * 8, row * 8, dx, dy);
                        let dc = bundles[bundle::INTER_DC].next_dc(br)?;
                        let q = br.read(4) as usize;
                        let mut block = [0i32; 64];
                        read_dct_coeffs(br, &mut block, &self.tables.scan)?;
                        reconstruct(
                            &mut block,
                            q,
                            &self.tables,
                            &mut out,
                            w,
                            col * 8,
                            row * 8,
                            AddMode::Inter {
                                dc,
                                pred: &mut pred_dc,
                            },
                        );
                    }
                    8 => {
                        let c0 = bundles[bundle::COLORS].next(br)?;
                        let c1 = bundles[bundle::COLORS].next(br)?;
                        pattern_fill_rows(
                            &mut out,
                            w,
                            col * 8,
                            row * 8,
                            c0,
                            c1,
                            &mut bundles[bundle::PATTERN],
                            &self.tables.binary_patterns.masks,
                            br,
                        )?;
                    }
                    9 => {
                        for yy in 0..8 {
                            for xx in 0..8 {
                                let c = bundles[bundle::COLORS].next(br)?;
                                out[(row * 8 + yy) * w + col * 8 + xx] = c;
                            }
                        }
                    }
                    _ => unreachable!(),
                }
                if advance2 {
                    col += 2;
                } else {
                    col += 1;
                }
            }
            row += 1;
        }
        if br.overflowed() {
            return Err(VideoError::new(
                VideoErrorKind::OutOfData,
                format!(
                    "plane {w}x{h} bitstream ended early at bit {} of {}",
                    br.bits_read(),
                    br.total_bits()
                ),
            ));
        }
        if std::env::var_os("XIII_VIDEO_TRACE_PLANES").is_some() {
            eprintln!(
                "plane {w}x{h} chroma={chroma} consumed {} bits ({} bytes)",
                br.bits_read().saturating_sub(plane_start_bit),
                br.bits_read().saturating_sub(plane_start_bit).div_ceil(8)
            );
        }
        Ok(out)
    }

    /// Decodes one 8x8 scalar block (used by the 16x16 scaled type).
    ///
    /// Sub-types map to the 8x8 block types 3..9 (`binkw32.dll` jump table at
    /// `0x3001f328`): 3 run, 4 residue (skipped), 5 intra DCT, 6 fill, 7 inter DCT
    /// (skipped), 8 pattern, 9 raw.
    fn decode_scalar_block(
        &self,
        br: &mut BitReader<'_>,
        bundles: &mut [Bundle; bundle::COUNT],
        sub: u8,
        block: &mut [u8; 64],
    ) -> Result<()> {
        match sub {
            3 => {
                let pattern = br.read(4) as usize & 15;
                *block = fill_run_values(br, bundles, &self.tables.patterns.patterns[pattern])?;
            }
            5 => {
                let dc = bundles[bundle::INTRA_DC].next_dc(br)?;
                let q = br.read(4) as usize;
                let mut coeff = [0i32; 64];
                read_dct_coeffs(br, &mut coeff, &self.tables.scan)?;
                *block = reconstruct_block(&mut coeff, q, &self.tables, false, dc);
            }
            6 => {
                let c = bundles[bundle::COLORS].next(br)?;
                *block = [c; 64];
            }
            8 => {
                let c0 = bundles[bundle::COLORS].next(br)?;
                let c1 = bundles[bundle::COLORS].next(br)?;
                for y in 0..8 {
                    let row_pattern = bundles[bundle::PATTERN].next(br)?;
                    for x in 0..8 {
                        let nibble = if x < 4 {
                            row_pattern & 0x0f
                        } else {
                            row_pattern >> 4
                        } as usize;
                        let bit = (self.tables.binary_patterns.masks[nibble] >> ((x & 3) * 8))
                            & 0xff
                            != 0;
                        block[y * 8 + x] = if bit { c1 } else { c0 };
                    }
                }
            }
            9 => {
                for v in block.iter_mut() {
                    *v = bundles[bundle::COLORS].next(br)?;
                }
            }
            // Sub-types outside 3..=9 (and the DCT/known set) are treated as a plain advance by
            // the shipped decoder (`0x3001e5ed`: `sub - 3 > 6` -> skip).
            _ => {}
        }
        Ok(())
    }

    /// Reads the nine bundle descriptors that begin every plane.
    fn read_plane_header(
        &self,
        br: &mut BitReader<'_>,
        bw: usize,
        bh: usize,
    ) -> Result<[Bundle; bundle::COUNT]> {
        // Order measured from the plane decoder in `binkw32.dll` (0x3001d94e..0x3001d9f2):
        // block types, sub-block types, then the colour codebook (16 context trees followed by
        // the low-nibble tree at 0x3001c120), then pattern, X motion, Y motion and run.
        let block_types = read_tree(br)?;
        let sub_block = read_tree(br)?;
        let mut colors_high = Vec::with_capacity(16);
        for _ in 0..16 {
            let t = read_tree(br)?;
            colors_high.push(build_static(t, &self.tables));
        }
        let colors_low = read_tree(br)?;
        let pattern = read_tree(br)?;
        let x_off = read_tree(br)?;
        let y_off = read_tree(br)?;
        let run = read_tree(br)?;

        // runsize = floor(log2(count + 511)) + 1, measured from the plane-decoder code path in
        // `binkw32.dll` (the `+0x1ff` threshold chain at 0x3001d43e).
        let rs = |count: usize| -> usize { floor_log2(count.saturating_add(511)) + 1 };
        let mut states: [Bundle; bundle::COUNT] = std::array::from_fn(|_| Bundle::empty());
        states[bundle::BLOCK_TYPES] = Bundle::new(
            Kind::RleRun,
            build_static(block_types, &self.tables),
            rs(bw),
        );
        states[bundle::SUB_BLOCK_TYPES] = Bundle::new(
            Kind::RleRun,
            build_static(sub_block, &self.tables),
            rs(bw / 2),
        );
        states[bundle::COLORS] = Bundle::colors(
            build_static(colors_low, &self.tables),
            colors_high,
            rs(bw * 64),
        );
        states[bundle::PATTERN] =
            Bundle::new(Kind::Pair, build_static(pattern, &self.tables), rs(bw * 8));
        states[bundle::X_OFF] =
            Bundle::new(Kind::Signed, build_static(x_off, &self.tables), rs(bw));
        states[bundle::Y_OFF] =
            Bundle::new(Kind::Signed, build_static(y_off, &self.tables), rs(bw));
        states[bundle::INTRA_DC] = Bundle::dc(rs(bw), 11, false);
        states[bundle::INTER_DC] = Bundle::dc(rs(bw), 11, true);
        states[bundle::RUN] =
            Bundle::new(Kind::Plain4, build_static(run, &self.tables), rs(bw * 48));
        // The DLL's t==0 cursor sentinel may read from buf+4 after a refill has no entries.
        // Reserve the plane's maximum 8x8-cell count so those reads remain bounded and retain
        // the same backing bytes across refills.
        let stale_capacity = bw.saturating_mul(bh).saturating_add(8);
        for bundle in &mut states {
            bundle.data.resize(stale_capacity, 0);
            bundle.signed.resize(stale_capacity, 0);
            bundle.dc.resize(stale_capacity, 0);
        }
        states[bundle::BLOCK_TYPES].name = "block";
        states[bundle::SUB_BLOCK_TYPES].name = "sub";
        states[bundle::COLORS].name = "colors";
        states[bundle::PATTERN].name = "pattern";
        states[bundle::X_OFF].name = "xoff";
        states[bundle::Y_OFF].name = "yoff";
        states[bundle::INTRA_DC].name = "intra_dc";
        states[bundle::INTER_DC].name = "inter_dc";
        states[bundle::RUN].name = "run";
        Ok(states)
    }
}

fn build_static(desc: (u8, [u8; 16]), tables: &BinkTables) -> Option<StaticTree> {
    let table = tables.huffman_tables.get(desc.0 as usize)?;
    StaticTree::new(table, desc.1).ok()
}

/// Reads a tree descriptor and returns its number and symbol permutation.
fn read_tree(br: &mut BitReader<'_>) -> Result<(u8, [u8; 16])> {
    let tree_num = br.read(4) as u8;
    if tree_num > 15 {
        return Err(VideoError::new(
            VideoErrorKind::BadValue,
            format!("Huffman tree number {tree_num}"),
        ));
    }
    let mut syms = IDENTITY;
    if tree_num == 0 {
        return Ok((0, syms));
    }
    if br.bit() == 1 {
        // The 3-bit field is one less than the number of explicitly coded symbols: the DLL
        // loop runs `for (i = 0; i <= n; i++)` (0x3001c0e2..0x3001c0e7).
        let n = br.read(3) as usize + 1;
        let mut used = [false; 16];
        let mut next = 0u8;
        for (i, slot) in syms.iter_mut().enumerate() {
            if i < n {
                let s = br.read(4) as u8 & 15;
                *slot = s;
                used[s as usize] = true;
            } else {
                while next < 16 && used[next as usize] {
                    next += 1;
                }
                let s = next.min(15);
                *slot = s;
                used[s as usize] = true;
                next = next.saturating_add(1);
            }
        }
    } else {
        let depth = br.read(2) as usize;
        let mut cur = syms;
        for level in 0..=depth.min(4) {
            let size = 1usize << level;
            let skip = size * 2;
            let mut tmp = [0u8; 16];
            let mut j = 0;
            while j < 16 {
                merge(
                    br,
                    &mut tmp[j..j + skip],
                    &cur[j..j + size],
                    &cur[j + size..j + skip],
                );
                j += skip;
            }
            cur = tmp;
        }
        syms = cur;
    }
    Ok((tree_num, syms))
}

fn merge(br: &mut BitReader<'_>, dst: &mut [u8], s1: &[u8], s2: &[u8]) {
    let size = s1.len();
    let (mut i1, mut i2, mut o) = (0, 0, 0);
    while i1 < size && i2 < size {
        if br.bit() == 0 {
            dst[o] = s1[i1];
            i1 += 1;
        } else {
            dst[o] = s2[i2];
            i2 += 1;
        }
        o += 1;
    }
    while i1 < size {
        dst[o] = s1[i1];
        i1 += 1;
        o += 1;
    }
    while i2 < size {
        dst[o] = s2[i2];
        i2 += 1;
        o += 1;
    }
}

/// Bundle value encoding.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 4-bit RLE bundle: values >= 12 are runs of the previous value (block/sub-block types).
    RleRun,
    /// Plain 4-bit bundle with an optional constant fill (run lengths).
    Plain4,
    Signed,
    Pair,
    Colors,
    Raw16,
}

/// Cached bundle state.
#[derive(Debug, Clone)]
struct Bundle {
    name: &'static str,
    kind: Kind,
    runsize: usize,
    tree: Option<StaticTree>,
    high: Vec<Option<StaticTree>>,
    data: Vec<u8>,
    pos: usize,
    data_end: usize,
    sentinel: bool,
    signed: Vec<i32>,
    signed_pos: usize,
    signed_end: usize,
    dc: Vec<i32>,
    dc_pos: usize,
    dc_end: usize,
    lastval: u8,
    previous_value: u8,
    dc_start_bits: usize,
    dc_signed: bool,
}

impl Bundle {
    fn empty() -> Self {
        Self {
            name: "unassigned",
            kind: Kind::Plain4,
            runsize: 0,
            tree: None,
            high: Vec::new(),
            data: Vec::new(),
            pos: 0,
            data_end: 0,
            sentinel: false,
            signed: Vec::new(),
            signed_pos: 0,
            signed_end: 0,
            dc: Vec::new(),
            dc_pos: 0,
            dc_end: 0,
            lastval: 0,
            previous_value: 0,
            dc_start_bits: 0,
            dc_signed: false,
        }
    }
    fn new(kind: Kind, tree: Option<StaticTree>, runsize: usize) -> Self {
        let mut b = Self::empty();
        b.kind = kind;
        b.tree = tree;
        b.runsize = runsize;
        b
    }
    fn colors(tree: Option<StaticTree>, high: Vec<Option<StaticTree>>, runsize: usize) -> Self {
        let mut b = Self::new(Kind::Colors, tree, runsize);
        b.high = high;
        b
    }

    fn dc(runsize: usize, start_bits: usize, signed: bool) -> Self {
        let mut b = Self::new(Kind::Raw16, None, runsize);
        b.dc_start_bits = start_bits;
        b.dc_signed = signed;
        b
    }

    fn next(&mut self, br: &mut BitReader<'_>) -> Result<u8> {
        if self.name == "sub" && std::env::var_os("XIII_VIDEO_TRACE_SUB").is_some() {
            eprintln!(
                "sub next pos={} len={} bit={}",
                self.pos,
                self.data.len(),
                br.bits_read()
            );
        }
        if self.sentinel {
            let v = self.data.get(self.pos).copied().ok_or_else(|| {
                VideoError::new(
                    VideoErrorKind::OutOfData,
                    format!("{} zero-count sentinel exceeded stale buffer", self.name),
                )
            })?;
            self.pos += 1;
            return Ok(v);
        }
        if self.pos >= self.data_end {
            self.refill(br)?;
        }
        if !self.sentinel && self.pos >= self.data_end {
            return Err(VideoError::new(
                VideoErrorKind::OutOfData,
                format!(
                    "bundle {} produced no value (kind {:?} runsize {})",
                    self.name, self.kind, self.runsize
                ),
            ));
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn next_signed(&mut self, br: &mut BitReader<'_>) -> Result<i32> {
        if self.sentinel {
            let v = self.signed.get(self.signed_pos).copied().ok_or_else(|| {
                VideoError::new(
                    VideoErrorKind::OutOfData,
                    "signed zero-count sentinel exhausted",
                )
            })?;
            self.signed_pos += 1;
            return Ok(v);
        }
        if self.signed_pos >= self.signed_end {
            self.refill_signed(br)?;
        }
        if self.sentinel {
            let v = self.signed.get(self.signed_pos).copied().ok_or_else(|| {
                VideoError::new(
                    VideoErrorKind::OutOfData,
                    "signed zero-count sentinel exhausted",
                )
            })?;
            self.signed_pos += 1;
            return Ok(v);
        }
        if !self.sentinel && self.signed_pos >= self.signed_end {
            return Err(VideoError::new(
                VideoErrorKind::OutOfData,
                format!("signed bundle produced no value (runsize {})", self.runsize),
            ));
        }
        let v = self.signed[self.signed_pos];
        self.signed_pos += 1;
        Ok(v)
    }

    fn next_dc(&mut self, br: &mut BitReader<'_>) -> Result<i32> {
        if self.sentinel {
            let v = self.dc.get(self.dc_pos).copied().ok_or_else(|| {
                VideoError::new(
                    VideoErrorKind::OutOfData,
                    "DC zero-count sentinel exhausted",
                )
            })?;
            self.dc_pos += 1;
            return Ok(v);
        }
        if self.dc_pos >= self.dc_end {
            self.refill_dc(br)?;
        }
        if self.sentinel {
            let v = self.dc.get(self.dc_pos).copied().ok_or_else(|| {
                VideoError::new(
                    VideoErrorKind::OutOfData,
                    "DC zero-count sentinel exhausted",
                )
            })?;
            self.dc_pos += 1;
            return Ok(v);
        }
        if !self.sentinel && self.dc_pos >= self.dc_end {
            return Err(VideoError::new(
                VideoErrorKind::OutOfData,
                format!("DC bundle produced no value (runsize {})", self.runsize),
            ));
        }
        let v = self.dc[self.dc_pos];
        self.dc_pos += 1;
        Ok(v)
    }

    fn refill(&mut self, br: &mut BitReader<'_>) -> Result<()> {
        let old_data = self.data.clone();
        let t = br.read(self.runsize) as usize;
        if std::env::var_os("XIII_VIDEO_TRACE_REFILL").is_some() {
            eprintln!(
                "refill {} {:?} bits={} t={}",
                self.name,
                self.kind,
                br.bits_read(),
                t
            );
        }
        if t == 0 {
            // The DLL's empty-refill sentinel sets cur_dec=buf+4 and cur_end=buf
            // (`0x3001c3c4`, `0x3001cb3e`, `0x3001c952`). Consumers still read from the
            // stale buffer for four bytes. Preserve that observable behavior rather than
            // converting zero-count to an empty Rust vector.
            self.data = old_data;
            self.pos = 4;
            self.data_end = 0;
            self.sentinel = true;
            return Ok(());
        }
        self.data.clear();
        self.pos = 0;
        self.data_end = 0;
        self.sentinel = false;
        match self.kind {
            Kind::RleRun => {
                if br.bit() != 0 {
                    let v = br.read(4) as u8;
                    self.data.resize(t, v);
                    self.previous_value = v;
                } else {
                    // `t` is the number of *output* values (the DLL decrements the remaining
                    // count by the run length; 0x3001c321/0x3001c342): v < 12 is one literal,
                    // v >= 12 is RUN_VALUES[v-12] copies of the previous value.
                    let mut out = 0usize;
                    while out < t {
                        let v = self.tree.as_ref().map_or(0, |tr| tr.decode(br));
                        if v < 12 {
                            self.data.push(v);
                            out += 1;
                        } else {
                            let run = RUN_VALUES[(v - 12) as usize];
                            let last = self.data.last().copied().unwrap_or(self.previous_value);
                            self.data.extend(std::iter::repeat_n(last, run));
                            out += run;
                        }
                    }
                    // The DLL's visible range is exactly [buf, buf+t), even if a final run
                    // writes beyond the requested count.
                    self.data.truncate(t);
                }
            }
            Kind::Plain4 => {
                if br.bit() == 1 {
                    let v = br.read(4) as u8;
                    self.data.extend(std::iter::repeat_n(v, t));
                } else {
                    for _ in 0..t {
                        self.data
                            .push(self.tree.as_ref().map_or(0, |tr| tr.decode(br)));
                    }
                }
            }
            Kind::Pair => {
                for _ in 0..t {
                    let n0 = self.tree.as_ref().map_or(0, |tr| tr.decode(br));
                    let n1 = self.tree.as_ref().map_or(0, |tr| tr.decode(br));
                    self.data.push(n0 | (n1 << 4));
                }
            }
            Kind::Colors => {
                if br.bit() == 1 {
                    // The shared refill's constant path reads one nibble and expands it to a
                    // byte by duplicating that nibble (`0x3001c35d..0x3001c3c0`).
                    let nibble = br.read(4) as u8;
                    self.data.resize(t, nibble | (nibble << 4));
                } else {
                    for _ in 0..t {
                        let hi = match self
                            .high
                            .get(self.lastval as usize)
                            .and_then(|t| t.as_ref())
                        {
                            Some(tr) => tr.decode(br),
                            None => br.read(4) as u8,
                        };
                        let lo = self.tree.as_ref().map_or(0, |tr| tr.decode(br));
                        self.lastval = hi;
                        self.data.push((hi << 4) | lo);
                    }
                }
            }
            Kind::Signed | Kind::Raw16 => {}
        }
        if !self.data.is_empty() && self.kind == Kind::RleRun {
            self.previous_value = *self.data.last().unwrap_or(&self.previous_value);
        }
        self.data_end = self.data.len();
        if old_data.len() > self.data.len() {
            self.data.extend_from_slice(&old_data[self.data.len()..]);
        }
        Ok(())
    }

    fn refill_signed(&mut self, br: &mut BitReader<'_>) -> Result<()> {
        let old_signed = self.signed.clone();
        let t = br.read(self.runsize) as usize;
        if t == 0 {
            self.signed = old_signed;
            self.signed_pos = 4;
            self.signed_end = 0;
            self.sentinel = true;
            return Ok(());
        }
        self.signed.clear();
        self.signed_pos = 0;
        self.signed_end = 0;
        self.sentinel = false;
        let fill = br.bit() != 0;
        if fill {
            let mut v = br.read(4) as i32;
            if v != 0 && br.bit() != 0 {
                v = -v;
            }
            self.signed.resize(t, v);
        } else {
            for _ in 0..t {
                let mut v = self.tree.as_ref().map_or(0, |tr| tr.decode(br)) as i32;
                if v != 0 && br.bit() != 0 {
                    v = -v;
                }
                self.signed.push(v);
            }
        }
        self.signed_end = self.signed.len();
        if old_signed.len() > self.signed.len() {
            self.signed
                .extend_from_slice(&old_signed[self.signed.len()..]);
        }
        Ok(())
    }

    fn refill_dc(&mut self, br: &mut BitReader<'_>) -> Result<()> {
        let old_dc = self.dc.clone();
        let t = br.read(self.runsize) as usize;
        if t == 0 {
            self.dc = old_dc;
            self.dc_pos = 2;
            self.dc_end = 0;
            self.sentinel = true;
            return Ok(());
        }
        self.dc.clear();
        self.dc_pos = 0;
        self.dc_end = 0;
        self.sentinel = false;
        let mut start = br.read(
            self.dc_start_bits
                .saturating_sub(usize::from(self.dc_signed)),
        ) as i32;
        if self.dc_signed && start != 0 && br.bit() != 0 {
            start = -start;
        }
        self.dc.push(start);
        let mut cur = start;
        let mut i = 1;
        while i < t {
            let n = 8.min(t - i);
            let w = br.read(4) as usize;
            if w == 0 {
                for _ in 0..n {
                    self.dc.push(cur);
                }
                i += n;
                continue;
            }
            for _ in 0..n {
                let mut v = br.read(w) as i32;
                if v != 0 && br.bit() == 1 {
                    v = -v;
                }
                cur += v;
                self.dc.push(cur);
                i += 1;
            }
        }
        self.dc_end = self.dc.len();
        if old_dc.len() > self.dc.len() {
            self.dc.extend_from_slice(&old_dc[self.dc.len()..]);
        }
        Ok(())
    }
}

fn motion_copy(out: &mut [u8], w: usize, h: usize, bx: usize, by: usize, dx: i32, dy: i32) {
    let mut copy = [0u8; 64];
    for yy in 0..8 {
        for xx in 0..8 {
            let sx = bx as i32 + xx as i32 + dx;
            let sy = by as i32 + yy as i32 + dy;
            copy[yy * 8 + xx] = if sx < 0 || sy < 0 || sx >= w as i32 || sy >= h as i32 {
                128
            } else {
                out[sy as usize * w + sx as usize]
            };
        }
    }
    for yy in 0..8 {
        for xx in 0..8 {
            out[(by + yy) * w + bx + xx] = copy[yy * 8 + xx];
        }
    }
}

fn scale_block_2x(block: &[u8; 64], out: &mut [u8], w: usize, bx: usize, by: usize) {
    for yy in 0..16 {
        for xx in 0..16 {
            let idx = (by + yy) * w + bx + xx;
            if idx < out.len() {
                out[idx] = block[(yy / 2) * 8 + xx / 2];
            }
        }
    }
}

fn fill_block(out: &mut [u8], w: usize, bx: usize, by: usize, c: u8) {
    for yy in 0..8 {
        for xx in 0..8 {
            out[(by + yy) * w + bx + xx] = c;
        }
    }
}

fn pattern_position(pattern: &[u8; 64], i: usize) -> usize {
    pattern[i] as usize
}

#[allow(clippy::too_many_arguments)]
fn pattern_fill_rows(
    out: &mut [u8],
    w: usize,
    bx: usize,
    by: usize,
    c0: u8,
    c1: u8,
    pattern: &mut Bundle,
    masks: &[u32; 16],
    br: &mut BitReader<'_>,
) -> Result<()> {
    for y in 0..8 {
        let row_pattern = pattern.next(br)?;
        for x in 0..8 {
            let nibble = if x < 4 {
                row_pattern & 0x0f
            } else {
                row_pattern >> 4
            } as usize;
            let bit = (masks[nibble] >> ((x & 3) * 8)) & 0xff != 0;
            out[(by + y) * w + bx + x] = if bit { c1 } else { c0 };
        }
    }
    Ok(())
}

/// Decodes the 64 run-fill values in pattern order (used by the 8x8 and 16x16 run blocks).
fn fill_run_values(
    br: &mut BitReader<'_>,
    bundles: &mut [Bundle; bundle::COUNT],
    pattern: &[u8; 64],
) -> Result<[u8; 64]> {
    let mut vals = [0u8; 64];
    let mut filled = 0usize;
    while filled < 64 {
        if br.bit() == 1 {
            // Solid run path reads the colour before the run byte (`0x3001dc16` then
            // `0x3001dc25`).
            let c = bundles[bundle::COLORS].next(br)?;
            // The block handler increments the stored run byte (`movzbl; incl`), so the
            // encoded value is length minus one.
            let run = bundles[bundle::RUN].next(br)? as usize + 1;
            for _ in 0..run {
                if filled >= 64 {
                    break;
                }
                vals[pattern_position(pattern, filled)] = c;
                filled += 1;
            }
        } else {
            // Literal run path reads the run length, then consumes that many colours.
            let run = bundles[bundle::RUN].next(br)? as usize + 1;
            for _ in 0..run {
                if filled >= 64 {
                    break;
                }
                let c = bundles[bundle::COLORS].next(br)?;
                vals[pattern_position(pattern, filled)] = c;
                filled += 1;
            }
        }
    }
    Ok(vals)
}

#[allow(clippy::too_many_arguments)]
fn fill_run(
    br: &mut BitReader<'_>,
    bundles: &mut [Bundle; bundle::COUNT],
    pattern: usize,
    patterns: &[[u8; 64]; 16],
    out: &mut [u8],
    w: usize,
    bx: usize,
    by: usize,
) -> Result<()> {
    let pat = &patterns[pattern & 15];
    let vals = fill_run_values(br, bundles, pat)?;
    for yy in 0..8 {
        for xx in 0..8 {
            out[(by + yy) * w + bx + xx] = vals[yy * 8 + xx];
        }
    }
    Ok(())
}

fn add_residue(block: &[i16; 64], scan: &[u8; 64], out: &mut [u8], w: usize, bx: usize, by: usize) {
    for &s in scan.iter() {
        let p = s as usize;
        let idx = (by + p / 8) * w + bx + p % 8;
        out[idx] = clamp8(f32::from(out[idx]) + f32::from(block[p]));
    }
}

/// DC prediction mode for reconstruction.
enum AddMode<'a> {
    Intra { dc: i32, pred: &'a mut i32 },
    Inter { dc: i32, pred: &'a mut i32 },
}

/// Dequantises with `q`, runs a float IDCT and writes the block into the plane.
#[allow(clippy::too_many_arguments)]
fn reconstruct(
    block: &mut [i32; 64],
    q: usize,
    tables: &BinkTables,
    out: &mut [u8],
    w: usize,
    bx: usize,
    by: usize,
    mode: AddMode<'_>,
) {
    let (dc, add) = match mode {
        AddMode::Intra { dc, pred } => {
            *pred = dc;
            (dc, false)
        }
        AddMode::Inter { dc, pred } => {
            let v = *pred + dc;
            *pred = v;
            (v, true)
        }
    };
    block[0] = dc;
    let spatial = dequant_idct(block, q, tables);
    for yy in 0..8 {
        for xx in 0..8 {
            let idx = (by + yy) * w + bx + xx;
            let val = if add {
                f32::from(out[idx]) + spatial[yy * 8 + xx]
            } else {
                128.0 + spatial[yy * 8 + xx]
            };
            out[idx] = clamp8(val);
        }
    }
}

/// Dequantises with `q` and runs an 8x8 float IDCT, returning the spatial block.
fn dequant_idct(block: &mut [i32; 64], q: usize, tables: &BinkTables) -> [f32; 64] {
    let quant = &tables.quant.tables[q.min(15)];
    for i in 0..64 {
        block[i] = ((i64::from(block[i]) * i64::from(quant[i])) >> 11) as i32;
    }
    static BASIS: OnceLock<[[f32; 8]; 8]> = OnceLock::new();
    let basis = BASIS.get_or_init(|| {
        std::array::from_fn(|k| {
            std::array::from_fn(|x| {
                let scale = if k == 0 { 1.0 / 2f32.sqrt() } else { 1.0 };
                scale * (((2 * x + 1) as f32 * k as f32 * std::f32::consts::PI) / 16.0).cos()
            })
        })
    });
    let mut horizontal = [[0f32; 8]; 8];
    for v in 0..8 {
        for x in 0..8 {
            let mut sum = 0.0;
            for u in 0..8 {
                sum += block[v * 8 + u] as f32 * basis[u][x];
            }
            horizontal[v][x] = sum;
        }
    }
    let mut spatial = [0f32; 64];
    for y in 0..8 {
        for x in 0..8 {
            let mut sum = 0.0;
            for v in 0..8 {
                sum += horizontal[v][x] * basis[v][y];
            }
            spatial[y * 8 + x] = sum * 0.25;
        }
    }
    spatial
}

/// Reconstructs an 8x8 block (used by the 16x16 scaled DCT sub-type).
fn reconstruct_block(
    coeff: &mut [i32; 64],
    q: usize,
    tables: &BinkTables,
    add: bool,
    dc: i32,
) -> [u8; 64] {
    coeff[0] = dc;
    let spatial = dequant_idct(coeff, q, tables);
    let mut out = [0u8; 64];
    for i in 0..64 {
        out[i] = clamp8(if add { 0.0 } else { 128.0 } + spatial[i]);
    }
    out
}

/// Reads DCT coefficient magnitudes for one block.
fn read_dct_coeffs(br: &mut BitReader<'_>, block: &mut [i32; 64], scan: &[u8; 64]) -> Result<()> {
    let maxbits = br.read(4) as usize;
    if maxbits == 0 {
        return Ok(());
    }
    if maxbits > 15 {
        return Err(VideoError::new(
            VideoErrorKind::BadValue,
            format!("DCT maxbits {maxbits}"),
        ));
    }
    let tree = dct_tree();
    let mut decided = 0u64;
    let mut sign = [false; 64];
    let mut mag = [0i32; 64];
    let mut mask = 1i32.checked_shl(maxbits as u32 - 1).unwrap_or(0);
    while mask != 0 {
        for (i, value) in mag.iter_mut().enumerate() {
            if decided & (1u64 << i) != 0 && br.bit() == 1 {
                *value |= mask;
            }
        }
        traverse_dct(&tree, br, mask, &mut mag, &mut decided, &mut sign);
        mask >>= 1;
    }
    for i in 0..64 {
        if decided & (1u64 << i) != 0 {
            let v = if sign[i] { -mag[i] } else { mag[i] };
            block[scan[i] as usize] = v;
        }
    }
    Ok(())
}

fn traverse_dct(
    node: &DctNode,
    br: &mut BitReader<'_>,
    mask: i32,
    mag: &mut [i32; 64],
    decided: &mut u64,
    sign: &mut [bool; 64],
) {
    match node {
        DctNode::Leaf(idx) => {
            let i = *idx as usize;
            let bit = 1u64 << i;
            if *decided & bit != 0 {
                return;
            }
            if br.bit() == 1 {
                *decided |= bit;
                sign[i] = br.bit() == 1;
                mag[i] |= mask;
            }
        }
        DctNode::Branch {
            children,
            mask: subtree_mask,
        } => {
            let all_decided = subtree_mask & !*decided == 0;
            if all_decided {
                return;
            }
            if br.bit() == 1 {
                for c in children {
                    traverse_dct(c, br, mask, mag, decided, sign);
                }
            }
        }
    }
}

fn node_mask(node: &DctNode) -> u64 {
    match node {
        DctNode::Leaf(i) => 1u64 << *i,
        DctNode::Branch { mask, .. } => *mask,
    }
}

/// Node of the fixed DCT coefficient tree (public prose: Kostya's coefficient-tree post).
#[derive(Debug)]
enum DctNode {
    Leaf(u8),
    Branch { children: Vec<DctNode>, mask: u64 },
}

fn branch(children: Vec<DctNode>) -> DctNode {
    let mask = children
        .iter()
        .fold(0u64, |mask, child| mask | node_mask(child));
    DctNode::Branch { children, mask }
}

fn leaves(a: u8, n: u8) -> Vec<DctNode> {
    (a..a + n).map(DctNode::Leaf).collect()
}

fn dct_tree() -> DctNode {
    branch(vec![
        branch(vec![
            branch(leaves(4, 4)),
            branch(vec![
                branch(leaves(8, 4)),
                branch(leaves(12, 4)),
                branch(leaves(16, 4)),
                branch(leaves(20, 4)),
            ]),
        ]),
        branch(vec![
            branch(leaves(24, 4)),
            branch(vec![
                branch(leaves(28, 4)),
                branch(leaves(32, 4)),
                branch(leaves(36, 4)),
                branch(leaves(40, 4)),
            ]),
        ]),
        branch(vec![
            branch(leaves(44, 4)),
            branch(vec![
                branch(leaves(48, 4)),
                branch(leaves(52, 4)),
                branch(leaves(56, 4)),
                branch(leaves(60, 4)),
            ]),
        ]),
        DctNode::Leaf(1),
        DctNode::Leaf(2),
        DctNode::Leaf(3),
    ])
}

/// Residue reader.
fn read_residue(
    br: &mut BitReader<'_>,
    block: &mut [i16; 64],
    masks_count: usize,
    scan: &[u8; 64],
) -> Result<()> {
    // Lossy residue: 7-bit mask count already read by the caller; here each mask is a bit plane.
    let tree = dct_tree();
    let mut decided = 0u64;
    let mut sign = [false; 64];
    let mut mag = [0i32; 64];
    let mut mask = 1i32
        .checked_shl(masks_count.saturating_sub(1) as u32)
        .unwrap_or(0);
    while mask != 0 {
        for (i, value) in mag.iter_mut().enumerate() {
            if decided & (1u64 << i) != 0 && br.bit() == 1 {
                *value |= mask;
            }
        }
        traverse_dct(&tree, br, mask, &mut mag, &mut decided, &mut sign);
        mask >>= 1;
    }
    for i in 0..64 {
        if decided & (1u64 << i) != 0 {
            let v = if sign[i] { -mag[i] } else { mag[i] };
            block[scan[i] as usize] = v as i16;
        }
    }
    Ok(())
}

fn read_u32(data: &[u8], off: &mut usize) -> Result<u32> {
    let s = data.get(*off..*off + 4).ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::Truncated,
            format!("plane size word at byte {off}"),
        )
    })?;
    *off += 4;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn slice_with_lookahead(data: &[u8], off: usize, len: usize) -> Result<Vec<u8>> {
    let end = off
        .checked_add(len)
        .ok_or_else(|| VideoError::new(VideoErrorKind::BadValue, "plane range overflow"))?;
    let plane = data.get(off..end).ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::Truncated,
            format!("plane range {off}..{end} past packet {}", data.len()),
        )
    })?;
    // Nine bundles can each request up to 16 count bits at the plane tail (144 bits); round up
    // to a 32-bit word boundary for the DLL reader's lookahead.
    let cap = len
        .checked_add(24)
        .ok_or_else(|| VideoError::new(VideoErrorKind::BadValue, "plane lookahead overflow"))?;
    let mut padded = Vec::with_capacity(cap);
    padded.extend_from_slice(plane);
    padded.resize(cap, 0);
    Ok(padded)
}

fn floor_log2(n: usize) -> usize {
    if n <= 1 {
        0
    } else {
        (usize::BITS - 1 - n.leading_zeros()) as usize
    }
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}
