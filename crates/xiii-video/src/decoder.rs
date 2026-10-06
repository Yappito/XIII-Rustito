//! Bink 1 (revision `i`) video decoder.
//!
//! The structure follows the public prose descriptions cited in `README.md` (per-plane Huffman
//! "bundles", ten 8x8 block types, motion compensation, DCT and residue). Every bit-level rule
//! was established from the disassembly of RAD's own `binkw32.dll` shipped with the game; the
//! relevant addresses are cited next to each rule. The fixed tables (Huffman lookup tables, RLE
//! run lengths, run-fill patterns, two-colour masks, coefficient scan order and both
//! dequantisation families) are read from that DLL at runtime via [`crate::tables::BinkTables`].
//!
//! The decoder never reports success for a construct it cannot decode: bitstream underrun,
//! a run that overshoots its bundle count, or a 16x16 block crossing the plane edge return an
//! explicit [`VideoError`]. Constructs the shipped decoder treats as a plain advance (block
//! types 10..15, unused 16x16 sub-types) are counted in [`FrameStats`] rather than hidden.

use crate::bitreader::BitReader;
use crate::container::Header;
use crate::error::{Result, VideoError, VideoErrorKind};
use crate::huffman::StaticTree;
use crate::tables::BinkTables;

/// One decoded frame in planes.
#[derive(Debug, Clone)]
pub struct YuvFrame {
    /// Luma plane (`plane_width` x `plane_height`).
    pub y: Vec<u8>,
    /// First chroma plane (Cb), `chroma_width` x `chroma_height`.
    pub u: Vec<u8>,
    /// Second chroma plane (Cr), `chroma_width` x `chroma_height`.
    pub v: Vec<u8>,
    /// Alpha plane at luma resolution, when present.
    pub a: Option<Vec<u8>>,
    /// Display width in pixels (header width).
    pub width: usize,
    /// Display height in pixels (header height).
    pub height: usize,
    /// Luma plane width (display width rounded up to 8).
    pub plane_width: usize,
    /// Luma plane height (display height rounded up to 8).
    pub plane_height: usize,
    /// Chroma plane width (`(width + 1) / 2` rounded up to 8).
    pub chroma_width: usize,
    /// Chroma plane height (`(height + 1) / 2` rounded up to 8).
    pub chroma_height: usize,
}

impl YuvFrame {
    /// Converts the display area to tightly packed RGBA8.
    ///
    /// Matrix (labelled assumption): ITU-R BT.601, limited ("TV") range, 4:2:0 nearest-sample
    /// chroma. This is how the FFmpeg black-box oracle presents Bink output; RAD's own
    /// YUV->RGB blitters were not analysed.
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut out = vec![0u8; self.width * self.height * 4];
        for yy in 0..self.height {
            for xx in 0..self.width {
                let y = (f32::from(self.y[yy * self.plane_width + xx]) - 16.0) * (255.0 / 219.0);
                let ci = (yy / 2) * self.chroma_width + xx / 2;
                let u = (f32::from(self.u[ci]) - 128.0) * (255.0 / 224.0);
                let v = (f32::from(self.v[ci]) - 128.0) * (255.0 / 224.0);
                let o = (yy * self.width + xx) * 4;
                out[o] = clamp8(y + 1.402 * v);
                out[o + 1] = clamp8(y - 0.344_136 * u - 0.714_136 * v);
                out[o + 2] = clamp8(y + 1.772 * u);
                out[o + 3] = self
                    .a
                    .as_ref()
                    .map_or(255, |a| a[yy * self.plane_width + xx]);
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
    /// Bits consumed by the video payload (all planes).
    pub bits_used: usize,
    /// Total bits available in the packet.
    pub bits_total: usize,
    /// Block types seen, indexed by type (0..=9).
    pub block_type_counts: [u64; 10],
    /// 16x16 sub-block types seen, indexed by sub-type (values above 9 are not counted here;
    /// like every sub-type other than 3/5/6/8/9 they also appear in `noop_blocks`).
    pub sub_type_counts: [u64; 10],
    /// True if any block type was outside 0..=9.
    pub saw_unknown_block: bool,
    /// Blocks the shipped decoder treats as a plain advance (types 10..15 and 16x16
    /// sub-types outside 3/5/6/8/9); their pixels keep the previous buffer contents.
    pub noop_blocks: u64,
    /// Bundle values read past the decoded range of their bundle (the shipped decoder reads
    /// stale buffer bytes there; a well-formed stream never does this).
    pub stale_reads: u64,
    /// Motion vectors whose source block left the plane and was clamped.
    pub clamped_motion: u64,
    /// Largest number of unread bits left in a plane whose extent is known (alpha and luma
    /// from their size words, the last chroma plane from the packet end). The shipped reader
    /// works in 32-bit words, so a correctly parsed plane leaves fewer than 32 bits.
    pub max_plane_slack_bits: usize,
    /// True if a plane read past its size word's extent.
    pub plane_overrun: bool,
}

impl FrameStats {
    fn note_extent(&mut self, used_bits: usize, extent_bytes: usize) {
        let extent = extent_bytes * 8;
        if used_bits > extent {
            self.plane_overrun = true;
        } else {
            self.max_plane_slack_bits = self.max_plane_slack_bits.max(extent - used_bits);
        }
    }
}

/// The clean-room decoder.
#[derive(Debug, Clone)]
pub struct Decoder {
    tables: BinkTables,
}

/// Bits the plane decoder consumed from its start.
#[derive(Debug)]
struct PlaneResult {
    bits: usize,
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

    /// Decodes a full frame from `packet` (the video payload), using `prev` as the reference.
    ///
    /// Packet layout for revision `i` (frame routine `0x3001f350`): an optional alpha plane
    /// preceded by a 32-bit size, a 32-bit size then the luma plane, then the two chroma
    /// planes (Cr first, then Cb). Each size counts from the size word itself: chroma starts at
    /// `luma_size_word + size` (`0x3001f413..0x3001f41a`). The second chroma plane starts at the
    /// 32-bit word after the last word the first chroma plane's bit reader loaded (the plane
    /// routine returns its word pointer, `0x3001f2f5`).
    pub fn decode_frame(
        &self,
        packet: &[u8],
        header: &Header,
        prev: Option<&YuvFrame>,
    ) -> Result<(YuvFrame, FrameStats)> {
        let width = header.width as usize;
        let height = header.height as usize;
        // Plane sizes: `(w + 7) & ~7` for luma and `((w + 1) / 2 + 7) & ~7` for chroma
        // (`0x3001f391`, `0x3001f453..0x3001f47b`).
        let w = align8(width);
        let h = align8(height);
        let cw = align8(width.div_ceil(2));
        let ch = align8(height.div_ceil(2));
        let mut stats = FrameStats {
            bits_total: packet.len() * 8,
            ..Default::default()
        };
        let prev = prev.filter(|p| p.plane_width == w && p.plane_height == h);

        let mut off = 0usize;
        let a = if header.has_alpha() {
            let size = read_u32(packet, off)? as usize;
            let plane_start = off + 4;
            let mut plane = start_plane(prev.and_then(|p| p.a.as_deref()), w * h);
            let r = self.decode_plane(
                packet,
                plane_start,
                w,
                h,
                prev.and_then(|p| p.a.as_deref()),
                &mut plane,
                &mut stats,
            )?;
            stats.bits_used += r.bits;
            off = checked_plane_end(packet, off, size, "alpha")?;
            stats.note_extent(r.bits, size - 4);
            Some(plane)
        } else {
            None
        };

        let y_size = read_u32(packet, off)? as usize;
        let mut y = start_plane(prev.map(|p| p.y.as_slice()), w * h);
        let r = self.decode_plane(
            packet,
            off + 4,
            w,
            h,
            prev.map(|p| p.y.as_slice()),
            &mut y,
            &mut stats,
        )?;
        stats.bits_used += r.bits;
        let chroma_start = checked_plane_end(packet, off, y_size, "luma")?;
        stats.note_extent(r.bits, y_size - 4);

        let (u, v) = if header.has_gray() {
            // Grayscale files have no chroma planes (`0x3001f41e`: flag 0x20000 skips them).
            (vec![128u8; cw * ch], vec![128u8; cw * ch])
        } else {
            // The first coded chroma plane is Cr, the second Cb (measured: swapping them
            // against the FFmpeg oracle turns a 22.8 dB chroma mismatch into an exact match on
            // `ubi.bik` frame 1).
            let mut v = start_plane(prev.map(|p| p.v.as_slice()), cw * ch);
            let rv = self.decode_plane(
                packet,
                chroma_start,
                cw,
                ch,
                prev.map(|p| p.v.as_slice()),
                &mut v,
                &mut stats,
            )?;
            stats.bits_used += rv.bits;
            let u_start = chroma_start + rv.bits.div_ceil(32) * 4;
            let mut u = start_plane(prev.map(|p| p.u.as_slice()), cw * ch);
            let ru = self.decode_plane(
                packet,
                u_start,
                cw,
                ch,
                prev.map(|p| p.u.as_slice()),
                &mut u,
                &mut stats,
            )?;
            stats.bits_used += ru.bits;
            stats.note_extent(ru.bits, packet.len().saturating_sub(u_start));
            (u, v)
        };

        Ok((
            YuvFrame {
                y,
                u,
                v,
                a,
                width,
                height,
                plane_width: w,
                plane_height: h,
                chroma_width: cw,
                chroma_height: ch,
            },
            stats,
        ))
    }

    /// Decodes one plane starting at byte `start` of `packet` into `out`.
    ///
    /// Plane routine `0x3001d3b0`: read the tree descriptors, then for each 8-pixel block row
    /// refill every bundle whose decoded data is used up, then walk the blocks of the row.
    #[allow(clippy::too_many_arguments)]
    fn decode_plane(
        &self,
        packet: &[u8],
        start: usize,
        w: usize,
        h: usize,
        prev: Option<&[u8]>,
        out: &mut [u8],
        stats: &mut FrameStats,
    ) -> Result<PlaneResult> {
        let data = packet.get(start..).ok_or_else(|| {
            VideoError::new(
                VideoErrorKind::Truncated,
                format!("plane starts at byte {start}, past packet {}", packet.len()),
            )
        })?;
        let mut br = BitReader::new(data);
        let mut b = Bundles::read_header(&mut br, w / 8, &self.tables)?;
        let zero;
        let prev: &[u8] = match prev {
            Some(p) => p,
            None => {
                zero = vec![0u8; w * h];
                &zero
            }
        };
        let plane = Plane { w, h, prev };
        let mut at = (0usize, 0usize);
        if let Err(e) = self.plane_rows(&mut br, &mut b, &plane, out, &mut at, stats) {
            return Err(VideoError::new(
                e.kind(),
                format!(
                    "{w}x{h} plane, block row {} col {} (bit {}): {}",
                    at.0,
                    at.1,
                    br.bits_read(),
                    e.message()
                ),
            ));
        }
        if br.overflowed() {
            return Err(VideoError::new(
                VideoErrorKind::OutOfData,
                format!(
                    "{w}x{h} plane read past the end of the packet ({} bits available)",
                    br.total_bits()
                ),
            ));
        }
        Ok(PlaneResult {
            bits: br.bits_read(),
        })
    }

    /// Walks the block rows of one plane (`0x3001da10..0x3001f2ef`).
    #[allow(clippy::too_many_arguments)]
    fn plane_rows(
        &self,
        br: &mut BitReader<'_>,
        b: &mut Bundles,
        plane: &Plane<'_>,
        out: &mut [u8],
        at: &mut (usize, usize),
        stats: &mut FrameStats,
    ) -> Result<()> {
        let (w, h) = (plane.w, plane.h);
        let (bw, bh) = (w / 8, h / 8);

        for row in 0..bh {
            *at = (row, 0);
            b.refill_all(br, &self.tables)?;
            let mut col = 0usize;
            while col < bw {
                at.1 = col;
                let bx = col * 8;
                let by = row * 8;
                let bt = b.block_types.next(stats)?;
                if bt > 9 {
                    // Block types above 9 are a plain advance (`0x3001dad9` -> `0x3001f274`).
                    stats.saw_unknown_block = true;
                    stats.noop_blocks += 1;
                    col += 1;
                    continue;
                }
                stats.block_type_counts[usize::from(bt)] += 1;
                match bt {
                    0 => {
                        // Skip: copy the co-located block of the previous frame (`0x3001dae6`).
                        let blk = plane.fetch(bx as i32, by as i32, stats);
                        put8(out, w, bx, by, &blk);
                    }
                    1 => {
                        // 16x16 scaled block (`0x3001e5ce`). On odd block rows (`y & 8`) the
                        // type only advances over the half already written.
                        if bx + 16 > w {
                            return Err(VideoError::new(
                                VideoErrorKind::BadValue,
                                format!("16x16 block at ({bx},{by}) crosses the {w}x{h} plane"),
                            ));
                        }
                        if row & 1 == 0 {
                            if by + 16 > h {
                                return Err(VideoError::new(
                                    VideoErrorKind::BadValue,
                                    format!("16x16 block at ({bx},{by}) crosses the {w}x{h} plane"),
                                ));
                            }
                            let sub = b.sub_types.next(stats)?;
                            if let Some(c) = stats.sub_type_counts.get_mut(usize::from(sub)) {
                                *c += 1;
                            }
                            match self.decode_scaled(br, b, sub, stats)? {
                                Some(blk) => put16(out, w, bx, by, &blk),
                                None => stats.noop_blocks += 1,
                            }
                        }
                        col += 2;
                        continue;
                    }
                    2 => {
                        // Motion copy from the previous frame (`0x3001e46b`).
                        let dx = i32::from(b.x_off.next(stats)? as i8);
                        let dy = i32::from(b.y_off.next(stats)? as i8);
                        let blk = plane.fetch(bx as i32 + dx, by as i32 + dy, stats);
                        put8(out, w, bx, by, &blk);
                    }
                    3 => {
                        let blk = self.decode_run(br, b, stats)?;
                        put8(out, w, bx, by, &blk);
                    }
                    4 => {
                        // Motion copy plus residue (`0x3001de16`): a 7-bit mask budget is read
                        // after the vectors, then `0x30021a90` adds the residue bytes.
                        let dx = i32::from(b.x_off.next(stats)? as i8);
                        let dy = i32::from(b.y_off.next(stats)? as i8);
                        let mut blk = plane.fetch(bx as i32 + dx, by as i32 + dy, stats);
                        let masks = br.read(7);
                        let res = read_residue(br, masks)?;
                        for (i, &r) in res.iter().enumerate() {
                            let p = usize::from(self.tables.scan[i]);
                            blk[p] = blk[p].wrapping_add(r as u8);
                        }
                        put8(out, w, bx, by, &blk);
                    }
                    5 => {
                        // Intra DCT (`0x3001dd68`): DC from its bundle, coefficients, then the
                        // 4-bit quantiser, then dequantise + IDCT with the intra family.
                        let coeffs = self.read_dct_block(br, b.intra_dc.next(stats)?)?;
                        let q = br.read(4) as usize;
                        let blk = idct(&coeffs, &self.tables.quant.tables[q], None);
                        put8(out, w, bx, by, &blk);
                    }
                    6 => {
                        // Fill (`0x3001e0ca`).
                        let c = b.colors.next(stats)?;
                        put8(out, w, bx, by, &[c; 64]);
                    }
                    7 => {
                        // Inter DCT (`0x3001df57`): motion copy, inter DC, coefficients, 4-bit
                        // quantiser, then IDCT added to the prediction (`0x30020500`).
                        let dx = i32::from(b.x_off.next(stats)? as i8);
                        let dy = i32::from(b.y_off.next(stats)? as i8);
                        let pred = plane.fetch(bx as i32 + dx, by as i32 + dy, stats);
                        let coeffs = self.read_dct_block(br, b.inter_dc.next(stats)?)?;
                        let q = br.read(4) as usize;
                        let blk = idct(&coeffs, &self.tables.quant_inter.tables[q], Some(&pred));
                        put8(out, w, bx, by, &blk);
                    }
                    8 => {
                        let blk = self.decode_pattern(b, stats)?;
                        put8(out, w, bx, by, &blk);
                    }
                    9 => {
                        // Raw: 64 colour-bundle bytes in row order (`0x3001e535`).
                        let mut blk = [0u8; 64];
                        for p in blk.iter_mut() {
                            *p = b.colors.next(stats)?;
                        }
                        put8(out, w, bx, by, &blk);
                    }
                    _ => unreachable!("block type checked above"),
                }
                col += 1;
            }
        }
        Ok(())
    }

    /// Decodes the 8x8 content of a 16x16 block (`0x3001e5ed`, jump table `0x3001f328`).
    ///
    /// Sub-types 3 (run), 5 (intra DCT), 6 (fill), 8 (pattern) and 9 (raw) decode an 8x8 block
    /// that the shipped routines write pixel-doubled; every other sub-type is a plain advance.
    fn decode_scaled(
        &self,
        br: &mut BitReader<'_>,
        b: &mut Bundles,
        sub: u8,
        stats: &mut FrameStats,
    ) -> Result<Option<[u8; 64]>> {
        Ok(Some(match sub {
            3 => self.decode_run(br, b, stats)?,
            5 => {
                // `0x3001ef08`: DC, coefficients, quantiser, then the scaled IDCT `0x300204d0`
                // (intra family, same arithmetic as `0x3001f4d0`, pixels doubled).
                let coeffs = self.read_dct_block(br, b.intra_dc.next(stats)?)?;
                let q = br.read(4) as usize;
                idct(&coeffs, &self.tables.quant.tables[q], None)
            }
            6 => [b.colors.next(stats)?; 64],
            8 => self.decode_pattern(b, stats)?,
            9 => {
                let mut blk = [0u8; 64];
                for p in blk.iter_mut() {
                    *p = b.colors.next(stats)?;
                }
                blk
            }
            _ => return Ok(None),
        }))
    }

    /// Run block (`0x3001db7d`, scaled variant `0x3001eae6`).
    ///
    /// A raw 4-bit pattern index selects one of the 16 fill orders. While fewer than 63 pixels
    /// are filled a flag bit chooses between a single-colour run (colour, then run length) and
    /// a run of literal colours; run lengths are the run-bundle byte plus one. If exactly 63
    /// pixels are filled, the last one is a literal colour with no flag bit (`0x3001dc96`).
    fn decode_run(
        &self,
        br: &mut BitReader<'_>,
        b: &mut Bundles,
        stats: &mut FrameStats,
    ) -> Result<[u8; 64]> {
        let pattern = &self.tables.patterns.patterns[br.read(4) as usize];
        let mut blk = [0u8; 64];
        let mut i = 0usize;
        while i < 63 {
            if br.bit() == 1 {
                let c = b.colors.next(stats)?;
                let run = usize::from(b.run.next(stats)?) + 1;
                for _ in 0..run {
                    let pos = *pattern.get(i).ok_or_else(run_overflow)?;
                    blk[usize::from(pos)] = c;
                    i += 1;
                }
            } else {
                let run = usize::from(b.run.next(stats)?) + 1;
                for _ in 0..run {
                    let pos = *pattern.get(i).ok_or_else(run_overflow)?;
                    blk[usize::from(pos)] = b.colors.next(stats)?;
                    i += 1;
                }
            }
        }
        if i == 63 {
            blk[usize::from(pattern[63])] = b.colors.next(stats)?;
        }
        Ok(blk)
    }

    /// Two-colour pattern block (`0x3001e153`): two colours, then eight pattern bytes (one per
    /// row; low nibble = pixels 0..3, high nibble = 4..7). A set mask bit selects the second
    /// colour (`0x3004f850`-style masks AND second colour, complement AND first).
    fn decode_pattern(&self, b: &mut Bundles, stats: &mut FrameStats) -> Result<[u8; 64]> {
        let c0 = b.colors.next(stats)?;
        let c1 = b.colors.next(stats)?;
        let masks = &self.tables.binary_patterns.masks;
        let mut blk = [0u8; 64];
        for y in 0..8 {
            let p = b.pattern.next(stats)?;
            for x in 0..8 {
                let nibble = if x < 4 { p & 0x0f } else { p >> 4 };
                let set = (masks[usize::from(nibble)] >> ((x & 3) * 8)) & 0xff != 0;
                blk[y * 8 + x] = if set { c1 } else { c0 };
            }
        }
        Ok(blk)
    }

    /// Reads one DCT block's coefficients (`0x30020c70`) and places them in natural order using
    /// the DLL's scan table; the DC comes from its bundle.
    fn read_dct_block(&self, br: &mut BitReader<'_>, dc: i16) -> Result<[i16; 64]> {
        let local = read_dct_coeffs(br)?;
        let mut block = [0i16; 64];
        block[0] = dc;
        // `0x3002145c..0x30021562` scatter list index `i` to position `scan[i]`.
        for i in 1..64 {
            block[usize::from(self.tables.scan[i])] = local[i];
        }
        Ok(block)
    }
}

fn run_overflow() -> VideoError {
    VideoError::new(
        VideoErrorKind::BadValue,
        "run block fills more than 64 pixels",
    )
}

/// Reference plane access for motion compensation.
struct Plane<'a> {
    w: usize,
    h: usize,
    prev: &'a [u8],
}

impl Plane<'_> {
    /// Copies the 8x8 block at (`x`, `y`) of the previous frame. The shipped decoder does no
    /// bounds handling; out-of-plane sources are clamped here and counted.
    fn fetch(&self, x: i32, y: i32, stats: &mut FrameStats) -> [u8; 64] {
        let mut blk = [0u8; 64];
        let max_x = self.w as i32 - 8;
        let max_y = self.h as i32 - 8;
        if x < 0 || y < 0 || x > max_x || y > max_y {
            stats.clamped_motion += 1;
        }
        let x = x.clamp(0, max_x.max(0)) as usize;
        let y = y.clamp(0, max_y.max(0)) as usize;
        for r in 0..8 {
            let s = (y + r) * self.w + x;
            blk[r * 8..r * 8 + 8].copy_from_slice(&self.prev[s..s + 8]);
        }
        blk
    }
}

fn put8(out: &mut [u8], w: usize, bx: usize, by: usize, blk: &[u8; 64]) {
    for r in 0..8 {
        let d = (by + r) * w + bx;
        out[d..d + 8].copy_from_slice(&blk[r * 8..r * 8 + 8]);
    }
}

/// Writes an 8x8 block pixel-doubled into a 16x16 area (the scaled sub-block routines write
/// every value as a 2x2 square, e.g. `0x3001ef9c`).
fn put16(out: &mut [u8], w: usize, bx: usize, by: usize, blk: &[u8; 64]) {
    for r in 0..16 {
        let d = (by + r) * w + bx;
        for c in 0..16 {
            out[d + c] = blk[(r / 2) * 8 + c / 2];
        }
    }
}

fn start_plane(prev: Option<&[u8]>, len: usize) -> Vec<u8> {
    match prev {
        Some(p) if p.len() == len => p.to_vec(),
        _ => vec![0u8; len],
    }
}

fn checked_plane_end(packet: &[u8], word_at: usize, size: usize, what: &str) -> Result<usize> {
    let end = word_at
        .checked_add(size)
        .filter(|&e| e <= packet.len() && size >= 4)
        .ok_or_else(|| {
            VideoError::new(
                VideoErrorKind::BadValue,
                format!(
                    "{what} plane size {size} at byte {word_at} does not fit the {}-byte packet",
                    packet.len()
                ),
            )
        })?;
    Ok(end)
}

fn read_u32(data: &[u8], off: usize) -> Result<u32> {
    let s = data.get(off..off + 4).ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::Truncated,
            format!("plane size word at byte {off}"),
        )
    })?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn align8(n: usize) -> usize {
    (n + 7) & !7
}

/// `floor(log2(n + 511)) + 1`: the count width of every bundle (the `+0x1ff` threshold chain
/// at `0x3001d43e`).
fn count_bits(n: usize) -> usize {
    let v = n + 511;
    (usize::BITS - v.leading_zeros()) as usize
}

// ---------------------------------------------------------------------------------------------
// Bundles
// ---------------------------------------------------------------------------------------------

/// What a byte bundle's refill routine does.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ByteKind {
    /// Block/sub-block types: 4-bit RLE (`0x3001c190`).
    Rle,
    /// Colours: context-coded high nibble plus low nibble (`0x3001c6d0`).
    Colors,
    /// Pattern rows: two symbols per byte, no constant fill (`0x3001cb70`).
    Pattern,
    /// Motion vector components: symbol plus sign (`0x3001cd70`).
    Motion,
    /// Run lengths: plain 4-bit symbols (`0x3001c980`).
    Run,
}

/// A byte bundle. `cur`/`end` mirror the shipped decoder's read and end pointers: a refill
/// happens at the start of a block row only when `cur == end`. A zero count sets `cur = 4`,
/// `end = 0` (`0x3001c3c4`), which disables further refills for the plane.
struct ByteBundle {
    kind: ByteKind,
    count_bits: usize,
    tree: StaticTree,
    buf: Vec<u8>,
    cur: usize,
    end: usize,
}

impl ByteBundle {
    fn new(kind: ByteKind, count_bits: usize, tree: StaticTree) -> Self {
        Self {
            kind,
            count_bits,
            tree,
            buf: vec![0u8; (1usize << count_bits) + 64],
            cur: 0,
            end: 0,
        }
    }

    fn next(&mut self, stats: &mut FrameStats) -> Result<u8> {
        let v = *self.buf.get(self.cur).ok_or_else(|| {
            VideoError::new(
                VideoErrorKind::OutOfData,
                format!(
                    "{:?} bundle read past its buffer (index {})",
                    self.kind, self.cur
                ),
            )
        })?;
        if self.cur >= self.end {
            stats.stale_reads += 1;
        }
        self.cur += 1;
        Ok(v)
    }

    /// Reads the count and, if non-zero, starts a new decoded range at the buffer start.
    /// Returns the count, or `None` when no refill happens.
    fn begin(&mut self, br: &mut BitReader<'_>) -> Option<usize> {
        if self.cur != self.end {
            return None;
        }
        let t = br.read(self.count_bits) as usize;
        if t == 0 {
            self.cur = 4;
            self.end = 0;
            return None;
        }
        self.cur = 0;
        self.end = t;
        Some(t)
    }

    fn refill(&mut self, br: &mut BitReader<'_>, rle_runs: &[u8; 4]) -> Result<()> {
        let Some(t) = self.begin(br) else {
            return Ok(());
        };
        match self.kind {
            ByteKind::Rle => {
                if br.bit() == 1 {
                    // Constant fill: a raw nibble repeated `t` times (`0x3001c35d..0x3001c3c0`).
                    let v = br.read(4) as u8;
                    self.buf[..t].fill(v);
                } else {
                    // `t` counts output values; symbols 12..15 repeat the last literal
                    // (initially 0, reset per refill: `0x3001c282`) by the DLL's run lengths.
                    let mut left = t as isize;
                    let mut pos = 0usize;
                    let mut last = 0u8;
                    while left > 0 {
                        let v = self.tree.decode(br);
                        if v < 12 {
                            self.buf[pos] = v;
                            pos += 1;
                            left -= 1;
                            last = v;
                        } else {
                            let run = usize::from(rle_runs[usize::from(v - 12)]);
                            self.buf[pos..pos + run].fill(last);
                            pos += run;
                            left -= run as isize;
                        }
                    }
                    if left != 0 {
                        // The shipped loop only stops at exactly zero (`0x3001c34f`).
                        return Err(VideoError::new(
                            VideoErrorKind::BadValue,
                            format!("RLE run overshoots its bundle count {t}"),
                        ));
                    }
                }
            }
            ByteKind::Run => {
                // `0x3001c980`: constant raw nibble, or `t` symbols.
                if br.bit() == 1 {
                    let v = br.read(4) as u8;
                    self.buf[..t].fill(v);
                } else {
                    for i in 0..t {
                        self.buf[i] = self.tree.decode(br);
                    }
                }
            }
            ByteKind::Pattern => {
                // `0x3001cb70`: no fill flag; byte = second symbol << 4 | first symbol.
                for i in 0..t {
                    let lo = self.tree.decode(br);
                    let hi = self.tree.decode(br);
                    self.buf[i] = (hi << 4) | lo;
                }
            }
            ByteKind::Motion => {
                // `0x3001cd70`: constant raw nibble or symbols, each non-zero value followed by
                // a sign bit (1 = negative).
                if br.bit() == 1 {
                    let mut v = br.read(4) as u8;
                    if v != 0 && br.bit() == 1 {
                        v = v.wrapping_neg();
                    }
                    self.buf[..t].fill(v);
                } else {
                    for i in 0..t {
                        let mut v = self.tree.decode(br);
                        if v != 0 && br.bit() == 1 {
                            v = v.wrapping_neg();
                        }
                        self.buf[i] = v;
                    }
                }
            }
            ByteKind::Colors => unreachable!("colours use ColorBundle"),
        }
        Ok(())
    }
}

/// The colour bundle: 16 high-nibble trees selected by the previous high nibble, plus one
/// low-nibble tree (`0x3001c120`, refill `0x3001c6d0` for revision `i`).
struct ColorBundle {
    inner: ByteBundle,
    high: Vec<StaticTree>,
    /// Previous high nibble; reset to 0 per plane (`0x3001c178`), kept across refills.
    ctx: u8,
}

impl ColorBundle {
    fn next(&mut self, stats: &mut FrameStats) -> Result<u8> {
        self.inner.next(stats)
    }

    fn decode_one(&mut self, br: &mut BitReader<'_>) -> u8 {
        let hi = self.high[usize::from(self.ctx)].decode(br);
        self.ctx = hi;
        let lo = self.inner.tree.decode(br);
        (hi << 4) | lo
    }

    fn refill(&mut self, br: &mut BitReader<'_>) {
        let Some(t) = self.inner.begin(br) else {
            return;
        };
        if br.bit() == 1 {
            // Constant fill: one coded colour replicated `t` times (`0x3001c7a0`, `0x3001c90a`).
            let c = self.decode_one(br);
            self.inner.buf[..t].fill(c);
        } else {
            for i in 0..t {
                self.inner.buf[i] = self.decode_one(br);
            }
        }
    }
}

/// A DC bundle of 16-bit values (`0x3001cfe0`).
struct DcBundle {
    count_bits: usize,
    signed: bool,
    buf: Vec<i16>,
    cur: usize,
    end: usize,
}

impl DcBundle {
    fn new(count_bits: usize, signed: bool) -> Self {
        Self {
            count_bits,
            signed,
            buf: vec![0i16; (1usize << count_bits) + 64],
            cur: 0,
            end: 0,
        }
    }

    fn next(&mut self, stats: &mut FrameStats) -> Result<i16> {
        let v = *self.buf.get(self.cur).ok_or_else(|| {
            VideoError::new(
                VideoErrorKind::OutOfData,
                format!("DC bundle read past its buffer (index {})", self.cur),
            )
        })?;
        if self.cur >= self.end {
            stats.stale_reads += 1;
        }
        self.cur += 1;
        Ok(v)
    }

    /// First value: 11 bits (intra), or 10 bits plus a sign bit for non-zero values (inter).
    /// The rest come in groups of up to eight: a 4-bit width, then per value a delta of that
    /// width with a sign bit for non-zero deltas; width 0 repeats the running value.
    fn refill(&mut self, br: &mut BitReader<'_>) {
        if self.cur != self.end {
            return;
        }
        let t = br.read(self.count_bits) as usize;
        if t == 0 {
            self.cur = 4;
            self.end = 0;
            return;
        }
        self.cur = 0;
        self.end = t;
        let mut acc: i32 = if self.signed {
            let v = br.read(10) as i32;
            if v != 0 && br.bit() == 1 { -v } else { v }
        } else {
            br.read(11) as i32
        };
        self.buf[0] = acc as i16;
        let mut pos = 1usize;
        let mut left = t - 1;
        while left > 0 {
            let n = left.min(8);
            let bits = br.read(4) as usize;
            if bits == 0 {
                self.buf[pos..pos + n].fill(acc as i16);
            } else {
                for k in 0..n {
                    let mut d = br.read(bits) as i32;
                    if d != 0 && br.bit() == 1 {
                        d = -d;
                    }
                    acc = acc.wrapping_add(d);
                    self.buf[pos + k] = acc as i16;
                }
            }
            pos += n;
            left -= n;
        }
    }
}

/// The nine bundles of a plane.
struct Bundles {
    block_types: ByteBundle,
    sub_types: ByteBundle,
    colors: ColorBundle,
    pattern: ByteBundle,
    x_off: ByteBundle,
    y_off: ByteBundle,
    intra_dc: DcBundle,
    inter_dc: DcBundle,
    run: ByteBundle,
}

impl Bundles {
    /// Reads the plane header: block-type tree, sub-type tree, 16 colour-context trees plus
    /// the low-nibble tree, pattern, X, Y and run trees (`0x3001d94e..0x3001d9f2`).
    fn read_header(br: &mut BitReader<'_>, bw: usize, tables: &BinkTables) -> Result<Self> {
        let block_types = read_tree(br, tables)?;
        let sub_types = read_tree(br, tables)?;
        let mut high = Vec::with_capacity(16);
        for _ in 0..16 {
            high.push(read_tree(br, tables)?);
        }
        let colors_low = read_tree(br, tables)?;
        let pattern = read_tree(br, tables)?;
        let x_off = read_tree(br, tables)?;
        let y_off = read_tree(br, tables)?;
        let run = read_tree(br, tables)?;
        // Count widths (`0x3001d43e..0x3001d90b`): block types bw, sub-types bw/2, colours
        // bw*64, pattern bw*8, motion bw, DC bw, run bw*48.
        Ok(Self {
            block_types: ByteBundle::new(ByteKind::Rle, count_bits(bw), block_types),
            sub_types: ByteBundle::new(ByteKind::Rle, count_bits(bw / 2), sub_types),
            colors: ColorBundle {
                inner: ByteBundle::new(ByteKind::Colors, count_bits(bw * 64), colors_low),
                high,
                ctx: 0,
            },
            pattern: ByteBundle::new(ByteKind::Pattern, count_bits(bw * 8), pattern),
            x_off: ByteBundle::new(ByteKind::Motion, count_bits(bw), x_off),
            y_off: ByteBundle::new(ByteKind::Motion, count_bits(bw), y_off),
            intra_dc: DcBundle::new(count_bits(bw), false),
            inter_dc: DcBundle::new(count_bits(bw), true),
            run: ByteBundle::new(ByteKind::Run, count_bits(bw * 48), run),
        })
    }

    /// Row-start refills in the shipped order (`0x3001da10..0x3001daa2`).
    fn refill_all(&mut self, br: &mut BitReader<'_>, tables: &BinkTables) -> Result<()> {
        let runs = &tables.rle_runs;
        self.block_types.refill(br, runs)?;
        self.sub_types.refill(br, runs)?;
        self.colors.refill(br);
        self.pattern.refill(br, runs)?;
        self.x_off.refill(br, runs)?;
        self.y_off.refill(br, runs)?;
        self.intra_dc.refill(br);
        self.inter_dc.refill(br);
        self.run.refill(br, runs)?;
        Ok(())
    }
}

/// Reads a tree descriptor (`0x3001bd60`) and returns its lookup decoder.
///
/// 4-bit tree number (0 = raw nibbles, identity order). Otherwise one bit selects either an
/// explicit list (3-bit `n`, then `n + 1` 4-bit symbols, the unused symbols following in
/// ascending order) or a 2-bit merge depth `d`: `d + 1` levels of pairwise merges of the
/// identity order where each merge bit 1 takes from the second half (`0x3001bcc0`).
fn read_tree(br: &mut BitReader<'_>, tables: &BinkTables) -> Result<StaticTree> {
    let tree_num = br.read(4) as usize;
    let mut syms: [u8; 16] = std::array::from_fn(|i| i as u8);
    if tree_num != 0 {
        if br.bit() == 1 {
            let n = br.read(3) as usize;
            let mut used = [false; 16];
            for slot in syms.iter_mut().take(n + 1) {
                let s = br.read(4) as u8;
                *slot = s;
                used[usize::from(s)] = true;
            }
            let mut i = n + 1;
            for (s, &u) in used.iter().enumerate() {
                if !u && i < 16 {
                    syms[i] = s as u8;
                    i += 1;
                }
            }
        } else {
            let depth = br.read(2) as usize;
            let mut cur = syms;
            for level in 0..=depth {
                let size = 1usize << level;
                let mut tmp = [0u8; 16];
                for j in (0..16).step_by(size * 2) {
                    merge(
                        br,
                        &mut tmp[j..j + size * 2],
                        &cur[j..j + size],
                        &cur[j + size..j + size * 2],
                    );
                }
                cur = tmp;
            }
            syms = cur;
        }
    }
    let table = tables.huffman_tables.get(tree_num).ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::BadTable,
            format!("no Huffman lookup table {tree_num}"),
        )
    })?;
    StaticTree::new(table, syms)
}

fn merge(br: &mut BitReader<'_>, dst: &mut [u8], s1: &[u8], s2: &[u8]) {
    let (mut i1, mut i2, mut o) = (0, 0, 0);
    while i1 < s1.len() && i2 < s2.len() {
        if br.bit() == 0 {
            dst[o] = s1[i1];
            i1 += 1;
        } else {
            dst[o] = s2[i2];
            i2 += 1;
        }
        o += 1;
    }
    for &v in s1[i1..].iter().chain(&s2[i2..]) {
        dst[o] = v;
        o += 1;
    }
}

// ---------------------------------------------------------------------------------------------
// Coefficients and residue
// ---------------------------------------------------------------------------------------------

/// Work list of coefficient groups. Each entry byte is `index << 2 | mode`:
/// mode 0 = four singles at `index` plus a 16-group at `index + 4`; mode 1 = a 16-group that
/// splits into four 4-groups; mode 2 = a 4-group; mode 3 = one pending coefficient. New
/// pending singles are pushed at the front, split groups at the back; a zero byte is a removed
/// entry.
struct WorkList {
    buf: [u8; 192],
    start: usize,
    end: usize,
}

impl WorkList {
    fn new(initial: &[u8]) -> Self {
        let mut buf = [0u8; 192];
        let start = 96;
        buf[start..start + initial.len()].copy_from_slice(initial);
        Self {
            buf,
            start,
            end: start + initial.len(),
        }
    }

    fn push_front(&mut self, e: u8) -> Result<()> {
        if self.start == 0 {
            return Err(list_overflow());
        }
        self.start -= 1;
        self.buf[self.start] = e;
        Ok(())
    }

    fn push_back(&mut self, e: u8) -> Result<()> {
        if self.end >= self.buf.len() {
            return Err(list_overflow());
        }
        self.buf[self.end] = e;
        self.end += 1;
        Ok(())
    }
}

fn list_overflow() -> VideoError {
    VideoError::new(VideoErrorKind::BadValue, "coefficient work list overflow")
}

/// One pass of the coefficient list walk shared by the DCT and residue readers.
///
/// `on_new(br, index)` is called when a coefficient becomes significant. Returns `Ok(true)`
/// when `on_new` asked to stop.
fn walk_list(
    br: &mut BitReader<'_>,
    list: &mut WorkList,
    on_new: &mut dyn FnMut(&mut BitReader<'_>, u8) -> Result<bool>,
) -> Result<bool> {
    let mut p = list.start;
    while p < list.end {
        let e = list.buf[p];
        if e == 0 || br.bit() == 0 {
            p += 1;
            continue;
        }
        let idx = e >> 2;
        match e & 3 {
            0 | 2 => {
                if e & 3 == 0 {
                    // Mode 0 keeps the entry as the following 16-group (`0x30020db2`).
                    list.buf[p] = ((idx + 4) << 2) | 1;
                } else {
                    // Mode 2 removes the entry (`0x30020df3`).
                    list.buf[p] = 0;
                    p += 1;
                }
                // Four singles: bit 1 defers the coefficient to the front of the list, bit 0
                // makes it significant now (`0x30020dfe..0x30021165`).
                for j in 0..4 {
                    let i = idx + j;
                    if br.bit() == 1 {
                        list.push_front((i << 2) | 3)?;
                    } else if on_new(br, i)? {
                        return Ok(true);
                    }
                }
            }
            1 => {
                // Split a 16-group into four 4-groups (`0x30020dc1`); the entry is re-examined.
                list.buf[p] = (idx << 2) | 2;
                list.push_back(((idx + 4) << 2) | 2)?;
                list.push_back(((idx + 8) << 2) | 2)?;
                list.push_back(((idx + 12) << 2) | 2)?;
            }
            _ => {
                // A pending single becomes significant (`0x300210db`).
                if on_new(br, idx)? {
                    return Ok(true);
                }
                list.buf[p] = 0;
                p += 1;
            }
        }
    }
    Ok(false)
}

/// DCT coefficient reader (`0x30020c70`), returning coefficients in list order (1..=63).
///
/// A 4-bit `bits` value gives the number of passes. In the pass with threshold
/// `mask = 1 << k` (k from `bits - 1` down to 0), a newly significant coefficient reads `k`
/// more magnitude bits (`value = bits | mask`) and a sign bit; there is no later refinement.
fn read_dct_coeffs(br: &mut BitReader<'_>) -> Result<[i16; 64]> {
    let mut local = [0i16; 64];
    let bits = br.read(4);
    let mut list = WorkList::new(&[
        4 << 2,
        24 << 2,
        44 << 2,
        (1 << 2) | 3,
        (2 << 2) | 3,
        (3 << 2) | 3,
    ]);
    for k in (0..bits).rev() {
        let mask = 1u32 << k;
        let mut on_new = |br: &mut BitReader<'_>, i: u8| -> Result<bool> {
            let mut v = (br.read(k as usize) | mask) as i32;
            if br.bit() == 1 {
                v = -v;
            }
            local[usize::from(i)] = v as i16;
            Ok(false)
        };
        walk_list(br, &mut list, &mut on_new)?;
    }
    Ok(local)
}

/// Residue reader (`0x30021590`), returning signed byte residues in list order.
///
/// A 3-bit value plus one gives the number of passes; the threshold `mask` starts at
/// `1 << (passes - 1)` as a signed byte and is halved arithmetically. Each pass first refines
/// every already significant coefficient (bit 1 adds `mask` away from zero), then walks the
/// list (new coefficients get `+-mask`). The walk stops after `masks + 1` such operations.
fn read_residue(br: &mut BitReader<'_>, masks: u32) -> Result<[i8; 64]> {
    let mut out = [0i8; 64];
    let passes = br.read(3) + 1;
    let mut mask = (1u8 << (passes - 1)) as i8;
    let mut list = WorkList::new(&[4 << 2, 24 << 2, 44 << 2, 2]);
    let mut nz: Vec<u8> = Vec::with_capacity(64);
    let mut ops = 0u32;
    for _ in 0..passes {
        for &k in &nz {
            if br.bit() == 1 {
                let i = usize::from(k);
                let step = if out[i] < 0 {
                    mask.wrapping_neg()
                } else {
                    mask
                };
                out[i] = out[i].wrapping_add(step);
                let old = ops;
                ops += 1;
                if old == masks {
                    return Ok(out);
                }
            }
        }
        let m = mask;
        let mut on_new = |br: &mut BitReader<'_>, i: u8| -> Result<bool> {
            nz.push(i);
            out[usize::from(i)] = if br.bit() == 1 { m.wrapping_neg() } else { m };
            let old = ops;
            ops += 1;
            Ok(old == masks)
        };
        if walk_list(br, &mut list, &mut on_new)? {
            return Ok(out);
        }
        mask >>= 1;
    }
    Ok(out)
}

// ---------------------------------------------------------------------------------------------
// IDCT
// ---------------------------------------------------------------------------------------------

/// One 8-point pass of the shipped integer IDCT (`0x3001f4f0..0x3001f6ae` columns,
/// `0x3001f6e0..` rows). Constants are 11-bit fixed point: 2896 (`0xb50`), 3784 (`0xec8`),
/// 2217 (`0x8a9`) and -5352 (`0xffffeb18`).
fn idct8(x: [i32; 8]) -> [i32; 8] {
    let a = x[0].wrapping_add(x[4]);
    let b = x[0].wrapping_sub(x[4]);
    let c = x[2].wrapping_add(x[6]);
    let t = (x[2].wrapping_sub(x[6]).wrapping_mul(2896) >> 11).wrapping_sub(c);
    let e0 = a.wrapping_add(c);
    let e3 = a.wrapping_sub(c);
    let e1 = b.wrapping_add(t);
    let e2 = b.wrapping_sub(t);

    let p = x[1].wrapping_add(x[7]);
    let s = x[1].wrapping_sub(x[7]);
    let q = x[3].wrapping_add(x[5]);
    let r = x[5].wrapping_sub(x[3]);
    let o0 = q.wrapping_add(p);
    let z = s.wrapping_add(r).wrapping_mul(3784) >> 11;
    let b1 = (r.wrapping_mul(-5352) >> 11)
        .wrapping_sub(o0)
        .wrapping_add(z);
    let b2 = (p.wrapping_sub(q).wrapping_mul(2896) >> 11).wrapping_sub(b1);
    let b3 = (s.wrapping_mul(2217) >> 11)
        .wrapping_sub(z)
        .wrapping_add(b2);

    [
        e0.wrapping_add(o0),
        e1.wrapping_add(b1),
        e2.wrapping_add(b2),
        e3.wrapping_sub(b3),
        e3.wrapping_add(b3),
        e2.wrapping_sub(b2),
        e1.wrapping_sub(b1),
        e0.wrapping_sub(o0),
    ]
}

/// Dequantise (`coef * q >> 11`, per column on input) and run the 2-D IDCT. Each output is
/// `(v + 0x7f) >> 8` truncated to a byte; with `pred` it is added to the prediction with
/// byte wrap-around (`0x30020500`), otherwise stored directly (`0x3001f4d0`). Neither path
/// saturates.
fn idct(coeffs: &[i16; 64], quant: &[i32; 64], pred: Option<&[u8; 64]>) -> [u8; 64] {
    let mut tmp = [0i32; 64];
    for c in 0..8 {
        let col: [i32; 8] = std::array::from_fn(|r| {
            i32::from(coeffs[r * 8 + c]).wrapping_mul(quant[r * 8 + c]) >> 11
        });
        let o = idct8(col);
        for r in 0..8 {
            tmp[r * 8 + c] = o[r];
        }
    }
    let mut out = [0u8; 64];
    for r in 0..8 {
        let row: [i32; 8] = std::array::from_fn(|c| tmp[r * 8 + c]);
        let o = idct8(row);
        for c in 0..8 {
            let v = (o[c].wrapping_add(0x7f) >> 8) as u8;
            out[r * 8 + c] = match pred {
                Some(p) => p[r * 8 + c].wrapping_add(v),
                None => v,
            };
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// LSB-first bit writer for building synthetic streams.
    struct BitWriter {
        bytes: Vec<u8>,
        bit: usize,
    }

    impl BitWriter {
        fn new() -> Self {
            Self {
                bytes: Vec::new(),
                bit: 0,
            }
        }
        fn put(&mut self, v: u32, n: usize) {
            for i in 0..n {
                if self.bit / 8 >= self.bytes.len() {
                    self.bytes.push(0);
                }
                if (v >> i) & 1 == 1 {
                    self.bytes[self.bit / 8] |= 1 << (self.bit % 8);
                }
                self.bit += 1;
            }
        }
    }

    #[test]
    fn count_bits_matches_threshold_chain() {
        // 0x1ff + n compared against powers of two: n = 0 -> 9 bits, n = 1 -> 10 bits.
        assert_eq!(count_bits(0), 9);
        assert_eq!(count_bits(1), 10);
        assert_eq!(count_bits(80), 10);
        assert_eq!(count_bits(513), 11);
        assert_eq!(count_bits(80 * 64), 13);
    }

    #[test]
    fn idct_of_dc_only_block_is_flat() {
        let mut c = [0i16; 64];
        c[0] = 1024; // 1024 * 65536 >> 11 = 32768 -> (32768 + 127) >> 8 = 128
        let q = [65536i32; 64];
        let out = idct(&c, &q, None);
        assert!(out.iter().all(|&v| v == 128), "{out:?}");
    }

    #[test]
    fn idct_add_wraps_instead_of_saturating() {
        let mut c = [0i16; 64];
        c[0] = 80; // 80 * 65536 >> 11 = 2560 -> (2560 + 127) >> 8 = 10
        let q = [65536i32; 64];
        let pred = [250u8; 64];
        let out = idct(&c, &q, Some(&pred));
        assert!(out.iter().all(|&v| v == 4), "{out:?}");
    }

    #[test]
    fn dct_reader_zero_bits_reads_nothing_else() {
        let mut w = BitWriter::new();
        w.put(0, 4);
        w.put(0xFFFF, 16);
        let mut br = BitReader::new(&w.bytes);
        let local = read_dct_coeffs(&mut br).unwrap();
        assert!(local.iter().all(|&v| v == 0));
        assert_eq!(br.bits_read(), 4);
    }

    #[test]
    fn dct_reader_single_coefficient_last_pass() {
        // bits = 1: one pass with mask 1 and no magnitude bits. Entries: [4:m0, 24:m0, 44:m0,
        // 1:m3, 2:m3, 3:m3]. Skip the first three (bit 0), take coefficient 1 (bit 1) with a
        // negative sign, skip 2 and 3.
        let mut w = BitWriter::new();
        w.put(1, 4);
        for b in [0, 0, 0, 1, 1, 0, 0] {
            w.put(b, 1);
        }
        let mut br = BitReader::new(&w.bytes);
        let local = read_dct_coeffs(&mut br).unwrap();
        assert_eq!(local[1], -1);
        assert_eq!(local.iter().filter(|&&v| v != 0).count(), 1);
        assert_eq!(br.bits_read(), 11);
    }

    #[test]
    fn dct_reader_group_defers_with_bit_one() {
        // bits = 2: pass k=1 (mask 2, one magnitude bit). Entry 4:m0 is opened; its four
        // singles 4..7: coefficient 4 significant now (bit 0, magnitude 1 -> 3, sign +),
        // coefficient 5 deferred (bit 1), 6 and 7 deferred. The re-examined entry 8:m1 is
        // skipped (bit 0), then 24, 44 and singles 1..3 are skipped.
        let mut w = BitWriter::new();
        w.put(2, 4);
        w.put(1, 1); // open 4:m0
        w.put(0, 1); // coef 4 now
        w.put(1, 1); // magnitude bit
        w.put(0, 1); // sign +
        w.put(1, 1); // defer 5
        w.put(1, 1); // defer 6
        w.put(1, 1); // defer 7
        w.put(0, 1); // 8:m1 skipped
        for _ in 0..5 {
            w.put(0, 1); // 24, 44, 1, 2, 3
        }
        // Final pass (mask 1): list is [7:m3, 6:m3, 5:m3, 8:m1, 24, 44, 1, 2, 3]; take 5.
        w.put(0, 1); // 7
        w.put(0, 1); // 6
        w.put(1, 1); // 5
        w.put(1, 1); // sign -
        for _ in 0..6 {
            w.put(0, 1);
        }
        let mut br = BitReader::new(&w.bytes);
        let local = read_dct_coeffs(&mut br).unwrap();
        assert_eq!(local[4], 3);
        assert_eq!(local[5], -1);
        assert_eq!(local.iter().filter(|&&v| v != 0).count(), 2);
        assert_eq!(br.bits_read(), w.bit);
    }

    #[test]
    fn residue_stops_after_mask_budget() {
        // passes = 1 (mask 1). Entry 4:m0 opened; coefficient 4 significant (+1). With a mask
        // budget of 0 the reader stops right after that first operation.
        let mut w = BitWriter::new();
        w.put(0, 3);
        w.put(1, 1); // open 4:m0
        w.put(0, 1); // coef 4 now
        w.put(0, 1); // sign +
        w.put(0xFF, 8); // must not be consumed
        let mut br = BitReader::new(&w.bytes);
        let res = read_residue(&mut br, 0).unwrap();
        assert_eq!(res[4], 1);
        assert_eq!(br.bits_read(), 6);
    }

    #[test]
    fn dc_bundle_deltas_and_repeat_groups() {
        // count bits for bw = 1 is 10; t = 10 values. Start 100 (11 bits). Group of 8 with
        // width 2: deltas +1 x8. Then group of 1 with width 0: repeat.
        let mut w = BitWriter::new();
        w.put(10, 10);
        w.put(100, 11);
        w.put(2, 4);
        for _ in 0..8 {
            w.put(1, 2);
            w.put(0, 1);
        }
        w.put(0, 4);
        let mut br = BitReader::new(&w.bytes);
        let mut dc = DcBundle::new(count_bits(1), false);
        dc.refill(&mut br);
        let mut stats = FrameStats::default();
        let vals: Vec<i16> = (0..10).map(|_| dc.next(&mut stats).unwrap()).collect();
        assert_eq!(vals, vec![100, 101, 102, 103, 104, 105, 106, 107, 108, 108]);
        assert_eq!(stats.stale_reads, 0);
        // Fully consumed: the next row refills; a zero count disables further refills.
        let mut w2 = BitWriter::new();
        w2.put(0, 10);
        let mut br2 = BitReader::new(&w2.bytes);
        dc.refill(&mut br2);
        assert_eq!((dc.cur, dc.end), (4, 0));
        dc.refill(&mut br2);
        assert_eq!(
            br2.bits_read(),
            10,
            "no further count is read after a zero count"
        );
    }

    /// Synthetic tables: every tree is the raw-nibble lookup table (no DLL bytes involved).
    fn synthetic_tables() -> BinkTables {
        use crate::tables::{BinaryPatterns, HuffmanLengths, Patterns, QuantTables};
        let raw: Vec<u8> = (0..16u8).map(|i| 0x40 | i).collect();
        let mut masks = [0u32; 16];
        for (n, m) in masks.iter_mut().enumerate() {
            for bit in 0..4 {
                if n & (1 << bit) != 0 {
                    *m |= 0xff << (bit * 8);
                }
            }
        }
        BinkTables {
            huffman_lengths: HuffmanLengths {
                rows: [[4u8; 16]; 16],
            },
            huffman_tables: vec![raw; 16],
            tree_maxbits: [4; 16],
            rle_runs: [4, 8, 16, 20],
            patterns: Patterns {
                patterns: [std::array::from_fn(|i| i as u8); 16],
            },
            binary_patterns: BinaryPatterns { masks },
            scan: std::array::from_fn(|i| i as u8),
            quant: QuantTables {
                tables: [[65536; 64]; 16],
            },
            quant_inter: QuantTables {
                tables: [[65536; 64]; 16],
            },
            dll_size: 0,
            dll_fnv1a: 0,
        }
    }

    /// Writes a plane header with every tree number 0 (raw nibbles).
    fn raw_header(w: &mut BitWriter) {
        for _ in 0..23 {
            w.put(0, 4);
        }
    }

    #[test]
    fn plane_decodes_fill_pattern_and_raw_blocks_in_row_refill_order() {
        // A 24x8 plane: three 8x8 blocks in one row. Bundle counts are 10 bits wide for every
        // bundle at this width (count_bits(3), (1), (192), (24), (3), (3), (144)).
        let mut w = BitWriter::new();
        raw_header(&mut w);
        // Block types: t = 3, literal symbols 6 (fill), 8 (pattern), 9 (raw).
        w.put(3, 10);
        w.put(0, 1);
        for v in [6, 8, 9] {
            w.put(v, 4);
        }
        w.put(0, 10); // sub-types: none
        // Colours: t = 67 (1 fill + 2 pattern + 64 raw); context-coded high nibble (raw tree)
        // then low nibble. Colour k = 0x10 + k for k < 67, i.e. high 1..5.
        w.put(67, 10);
        w.put(0, 1);
        for k in 0..67u32 {
            let c = 0x10 + k;
            w.put(c >> 4, 4);
            w.put(c & 15, 4);
        }
        // Pattern rows: t = 8; byte = second symbol << 4 | first symbol. Row r = 0x0F or 0xF0.
        w.put(8, 10);
        for r in 0..8 {
            let (lo, hi) = if r % 2 == 0 { (0xF, 0x0) } else { (0x0, 0xF) };
            w.put(lo, 4);
            w.put(hi, 4);
        }
        for _ in 0..5 {
            w.put(0, 10); // x, y, intra DC, inter DC, run: empty
        }
        let mut data = w.bytes.clone();
        data.resize(data.len() + 8, 0);
        let dec = Decoder::new(synthetic_tables());
        let mut out = vec![0u8; 24 * 8];
        let mut stats = FrameStats::default();
        let r = dec
            .decode_plane(&data, 0, 24, 8, None, &mut out, &mut stats)
            .unwrap();
        assert_eq!(r.bits, w.bit, "every written bit is consumed, nothing more");
        assert_eq!(stats.block_type_counts[6..], [1, 0, 1, 1]);
        assert_eq!(stats.stale_reads, 0);
        // Fill block: colour 0x10.
        assert!((0..8).all(|y| out[y * 24..y * 24 + 8].iter().all(|&p| p == 0x10)));
        // Pattern block: colours 0x11 (clear bit) / 0x12 (set bit); row 0 low nibble set.
        assert_eq!(
            &out[8..16],
            &[0x12, 0x12, 0x12, 0x12, 0x11, 0x11, 0x11, 0x11]
        );
        assert_eq!(
            &out[24 + 8..24 + 16],
            &[0x11, 0x11, 0x11, 0x11, 0x12, 0x12, 0x12, 0x12]
        );
        // Raw block: colours 0x13.. in row order.
        assert_eq!(out[16], 0x13);
        assert_eq!(out[7 * 24 + 23], 0x13 + 63);
    }

    #[test]
    fn plane_rejects_an_rle_run_that_overshoots_its_count() {
        let mut w = BitWriter::new();
        raw_header(&mut w);
        w.put(3, 10); // block types: t = 3
        w.put(0, 1);
        w.put(12, 4); // run symbol: 4 copies > 3 remaining
        let mut data = w.bytes.clone();
        data.resize(data.len() + 64, 0);
        let dec = Decoder::new(synthetic_tables());
        let mut out = vec![0u8; 24 * 8];
        let err = dec
            .decode_plane(&data, 0, 24, 8, None, &mut out, &mut FrameStats::default())
            .unwrap_err();
        assert_eq!(err.kind(), VideoErrorKind::BadValue);
        assert!(err.message().contains("overshoots"), "{err}");
    }

    #[test]
    fn plane_reports_underrun_instead_of_succeeding() {
        let mut w = BitWriter::new();
        raw_header(&mut w);
        w.put(3, 10); // three block types announced, data missing
        let data = w.bytes.clone();
        let dec = Decoder::new(synthetic_tables());
        let mut out = vec![0u8; 24 * 8];
        let err = dec
            .decode_plane(&data, 0, 24, 8, None, &mut out, &mut FrameStats::default())
            .unwrap_err();
        assert_eq!(err.kind(), VideoErrorKind::OutOfData, "{err}");
    }

    #[test]
    fn work_list_overflow_is_an_error() {
        let mut l = WorkList::new(&[1]);
        for _ in 0..96 {
            l.push_front(3).unwrap();
        }
        assert!(l.push_front(3).is_err());
    }
}
