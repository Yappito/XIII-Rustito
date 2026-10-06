//! Locates the fixed Bink decoder tables inside the installation's `binkw32.dll`.
//!
//! The RAD library shipped with XIII contains the constant tables the decoder needs. None of
//! them are copied into this repository: they are found in the DLL at load time by structural
//! signature and checked against an invariant. This keeps the decoder honest for both owned
//! installations (GOG and patched Steam) without embedding any RAD bytes.
//!
//! Located tables:
//!
//! * `huffman_lengths`: 16 rows x 16 code lengths. Row 0 is all fours (raw nibbles); every row
//!   is a complete prefix code (Kraft sum `sum 2^-len == 1`).
//! * `patterns`: 16 consecutive 64-byte permutations of `0..63` (the run-fill block scan
//!   patterns described by the MultimediaWiki *Bink Video* prose and Kostya's pattern-run post).
//! * `scan`: the 64-byte permutation immediately before the pattern table (the pair-oriented DCT
//!   coefficient scan). Located relative to the signature-found pattern table.
//! * `quant`: 16 x 64 `i32` dequantisation tables whose first column matches
//!   `round(65536 * q)` for the documented quantiser set
//!   `{1, 4/3, 5/3, 2, 8/3, 3.5, 4, 5, 6, 8, 12, 17, 22, 28, 34, 44}`.

use std::path::{Path, PathBuf};

use crate::error::{Result, VideoError, VideoErrorKind};

/// The 16 fixed Huffman code-length rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HuffmanLengths {
    /// `rows[tree][leaf]` = code length, or 0 for an unused leaf.
    pub rows: [[u8; 16]; 16],
}

/// The 16 run-fill patterns, each a permutation of `0..63`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Patterns {
    /// `patterns[i][j]` = the block position visited at fill step `j` for pattern `i`.
    pub patterns: [[u8; 64]; 16],
}

/// The 16 dequantisation tables.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuantTables {
    /// `tables[q][i]` = fixed-point dequant multiplier for quantiser `q` and coefficient `i`.
    pub tables: [[i32; 64]; 16],
}

/// The 16 masks used by Bink's two-colour pattern block type (one 4-pixel group per dword).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinaryPatterns {
    /// Mask bit `k` selects the second colour for pixel `k` of a four-pixel group.
    pub masks: [u32; 16],
}

/// All fixed tables required by the decoder, read from the installation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BinkTables {
    /// Fixed Huffman code lengths per tree number (invariant cross-check).
    pub huffman_lengths: HuffmanLengths,
    /// Precomputed Huffman lookup tables: `table[idx] = (code_length << 4) | leaf_position`,
    /// indexed by the next `tree_maxbits[t]` bits. This is the representation the shipped
    /// decoder uses; reading it avoids re-deriving the code assignment.
    pub huffman_tables: Vec<Vec<u8>>,
    /// Lookup width (in bits) for each precomputed Huffman table.
    pub tree_maxbits: [u8; 16],
    /// Run lengths of the RLE bundle symbols 12..=15, stored directly after the width table
    /// (the RLE refill at `0x3001c311` indexes `width_table + 4 + symbol`).
    pub rle_runs: [u8; 4],
    /// Run-fill scan patterns.
    pub patterns: Patterns,
    /// Two-colour pattern masks read from the RAD DLL.
    pub binary_patterns: BinaryPatterns,
    /// DCT coefficient scan order.
    pub scan: [u8; 64],
    /// Intra-block dequantisation tables (first matching family in the DLL; used by the 8x8
    /// intra DCT routine at `0x3001dd68`, which indexes `0x3004b2c0 + q * 256`).
    pub quant: QuantTables,
    /// Inter-block dequantisation tables (second matching family; used by the inter DCT add
    /// routine `0x30020500`, which indexes `0x3004d300 + q * 256`).
    pub quant_inter: QuantTables,
    /// Size in bytes of the DLL the tables were read from.
    pub dll_size: u64,
    /// FNV-1a 64-bit hash of the DLL bytes (diagnostics only; not an authentication).
    pub dll_fnv1a: u64,
}

/// Documented quantiser multipliers (public prose: MultimediaWiki / Kostya's "peculiarities").
const QUANTISERS: [f64; 16] = [
    1.0,
    4.0 / 3.0,
    5.0 / 3.0,
    2.0,
    8.0 / 3.0,
    3.5,
    4.0,
    5.0,
    6.0,
    8.0,
    12.0,
    17.0,
    22.0,
    28.0,
    34.0,
    44.0,
];

/// Scale of the fixed-point quantiser base (2^16).
const QUANT_BASE: f64 = 65536.0;

impl BinkTables {
    /// Reads and locates the tables from the `binkw32.dll` of an owned installation.
    ///
    /// `game_dir` is the installation root; the DLL is looked up case-insensitively at
    /// `system/binkw32.dll` first and by a shallow recursive search otherwise.
    pub fn from_install(game_dir: &Path) -> Result<Self> {
        let dll = find_binkw32(game_dir)?;
        let bytes = std::fs::read(&dll).map_err(|e| {
            VideoError::new(
                VideoErrorKind::Io,
                format!("cannot read {}: {e}", dll.display()),
            )
        })?;
        Self::from_dll_bytes(&bytes)
            .map_err(|e| VideoError::new(e.kind(), format!("{}: {}", dll.display(), e.message())))
    }

    /// Locates the tables in DLL bytes.
    pub fn from_dll_bytes(bytes: &[u8]) -> Result<Self> {
        let huffman_offset = find_huffman_lengths(bytes)?;
        let patterns_offset = find_patterns(bytes)?;
        let scan = find_scan_from_patterns(bytes, patterns_offset)?;
        let (quant_offset, quant_inter_offset) = find_quant_pair(bytes)?;
        let (tree_maxbits, tree_offset) = find_tree_tables(bytes)?;
        let binary_pattern_offset = find_binary_patterns(bytes)?;
        let huffman = read_huffman(bytes, huffman_offset);
        let huffman_tables = read_tree_tables(bytes, tree_offset, &tree_maxbits);
        let rle_runs = read_rle_runs(bytes, tree_offset, &tree_maxbits)?;
        let patterns = read_patterns(bytes, patterns_offset);
        let quant = read_quant(bytes, quant_offset);
        let quant_inter = read_quant(bytes, quant_inter_offset);
        let binary_patterns = read_binary_patterns(bytes, binary_pattern_offset);
        Ok(BinkTables {
            huffman_lengths: huffman,
            huffman_tables,
            tree_maxbits,
            rle_runs,
            patterns,
            binary_patterns,
            scan,
            quant,
            quant_inter,
            dll_size: bytes.len() as u64,
            dll_fnv1a: fnv1a64(bytes),
        })
    }
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// Finds `binkw32.dll` under an installation root, case-insensitively.
pub fn find_binkw32(game_dir: &Path) -> Result<PathBuf> {
    let direct = game_dir.join("system").join("binkw32.dll");
    if direct.is_file() {
        return Ok(direct);
    }
    // Case-insensitive walk of `system/`.
    if let Ok(entries) = std::fs::read_dir(game_dir.join("system")) {
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_ascii_lowercase();
            if name == "binkw32.dll" {
                return Ok(e.path());
            }
        }
    }
    // Fall back to a shallow recursive search (depth <= 4).
    if let Some(p) = search_recursive(game_dir, "binkw32.dll", 4) {
        return Ok(p);
    }
    Err(VideoError::new(
        VideoErrorKind::TableNotFound,
        format!("binkw32.dll not found under {}", game_dir.display()),
    ))
}

fn search_recursive(dir: &Path, want: &str, depth: usize) -> Option<PathBuf> {
    if depth == 0 {
        return None;
    }
    let entries = std::fs::read_dir(dir).ok()?;
    let mut subdirs = Vec::new();
    for e in entries.flatten() {
        let p = e.path();
        let name = e.file_name().to_string_lossy().to_ascii_lowercase();
        if p.is_file() {
            if name == want {
                return Some(p);
            }
        } else if p.is_dir() {
            subdirs.push(p);
        }
    }
    for d in subdirs {
        if let Some(p) = search_recursive(&d, want, depth - 1) {
            return Some(p);
        }
    }
    None
}

/// True when a 16-entry code-length row is a complete prefix code (Kraft equality).
fn row_is_complete_prefix_code(row: &[u8]) -> bool {
    let mut sum = 0u64;
    for &l in row {
        if l == 0 || l > 15 {
            return false;
        }
        sum += 1u64 << (15 - l);
    }
    sum == 1u64 << 15
}

/// Finds the 16x16 Huffman code-length table: 16 consecutive rows, each a complete prefix code,
/// with row 0 equal to sixteen fours (the raw-nibble tree).
pub fn find_huffman_lengths(dll: &[u8]) -> Result<usize> {
    let mut offset = None;
    for b in 0..dll.len().saturating_sub(256) {
        let region = &dll[b..b + 256];
        let row0 = &region[0..16];
        if row0 != [4u8; 16] {
            continue;
        }
        if (0..16).all(|r| row_is_complete_prefix_code(&region[r * 16..r * 16 + 16])) {
            if offset.is_some() {
                return Err(VideoError::new(
                    VideoErrorKind::BadTable,
                    "more than one Huffman length table matched",
                ));
            }
            offset = Some(b);
        }
    }
    offset.ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::TableNotFound,
            "Huffman code-length table not found in binkw32.dll",
        )
    })
}

fn is_perm64(chunk: &[u8]) -> bool {
    if chunk.len() != 64 {
        return false;
    }
    let mut seen = [false; 64];
    for &v in chunk {
        if v >= 64 || seen[v as usize] {
            return false;
        }
        seen[v as usize] = true;
    }
    true
}

/// Finds the run-fill pattern table: 16 consecutive 64-byte permutations immediately preceded
/// by eight zero bytes and, before those, the DCT scan permutation.
pub fn find_patterns(dll: &[u8]) -> Result<usize> {
    let mut offset = None;
    for b in 72..dll.len().saturating_sub(1024) {
        let ok = (0..16).all(|i| is_perm64(&dll[b + i * 64..b + i * 64 + 64]));
        if !ok {
            continue;
        }
        if dll[b - 8..b].iter().any(|&z| z != 0) {
            continue;
        }
        if !is_perm64(&dll[b - 72..b - 8]) {
            continue;
        }
        if offset.is_some() {
            return Err(VideoError::new(
                VideoErrorKind::BadTable,
                "more than one pattern table matched",
            ));
        }
        offset = Some(b);
    }
    offset.ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::TableNotFound,
            "run-fill pattern table not found in binkw32.dll",
        )
    })
}

/// Reads the DCT scan permutation sitting immediately before the pattern table, separated by
/// eight zero bytes. Verified to be a permutation.
pub fn find_scan_from_patterns(dll: &[u8], patterns_offset: usize) -> Result<[u8; 64]> {
    if patterns_offset < 72 {
        return Err(VideoError::new(
            VideoErrorKind::BadTable,
            "pattern table too close to the start of the DLL for a preceding scan table",
        ));
    }
    let zero = &dll[patterns_offset - 8..patterns_offset];
    if zero.iter().any(|&b| b != 0) {
        return Err(VideoError::new(
            VideoErrorKind::BadTable,
            "no eight-byte zero gap before the pattern table",
        ));
    }
    let start = patterns_offset - 72;
    let mut scan = [0u8; 64];
    scan.copy_from_slice(&dll[start..start + 64]);
    if !is_perm64(&scan) {
        return Err(VideoError::new(
            VideoErrorKind::BadTable,
            "candidate DCT scan order is not a permutation",
        ));
    }
    Ok(scan)
}

/// Expected first column of the quantiser tables, derived from the documented multipliers.
pub fn expected_quant_first_column() -> [i32; 16] {
    std::array::from_fn(|q| (QUANT_BASE * QUANTISERS[q] + 0.5).floor() as i32)
}

/// Finds the 16x64 `i32` quantisation tables by their documented scale sequence.
///
/// Returns the first matching family (the intra tables).
pub fn find_quant(dll: &[u8]) -> Result<usize> {
    let all = find_quant_all(dll);
    all.first().copied().ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::TableNotFound,
            "quantisation tables not found in binkw32.dll",
        )
    })
}

/// Finds the intra and the inter quantisation families.
///
/// The shipped decoder keeps two distinct 16x64 families with the same first column (the
/// documented quantiser scale): the intra DCT routine reads the first, the inter DCT add
/// routine the second (see [`BinkTables::quant_inter`]). Both must be present, must not
/// overlap and must differ.
pub fn find_quant_pair(dll: &[u8]) -> Result<(usize, usize)> {
    let all = find_quant_all(dll);
    let first = *all.first().ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::TableNotFound,
            "quantisation tables not found in binkw32.dll",
        )
    })?;
    let second = all
        .iter()
        .copied()
        .find(|&o| o >= first + 16 * 256)
        .ok_or_else(|| {
            VideoError::new(
                VideoErrorKind::TableNotFound,
                "second (inter) quantisation table family not found in binkw32.dll",
            )
        })?;
    if dll[first..first + 16 * 256] == dll[second..second + 16 * 256] {
        return Err(VideoError::new(
            VideoErrorKind::BadTable,
            "intra and inter quantisation families are identical",
        ));
    }
    Ok((first, second))
}

fn find_quant_all(dll: &[u8]) -> Vec<usize> {
    let expected = expected_quant_first_column();
    let mut found = Vec::new();
    let stride = 64 * 4; // 64 i32
    for b in 0..dll.len().saturating_sub(16 * stride) {
        let mut ok = true;
        for (q, &want) in expected.iter().enumerate() {
            let o = b + q * stride;
            let v = i32::from_le_bytes([dll[o], dll[o + 1], dll[o + 2], dll[o + 3]]);
            if v != want {
                ok = false;
                break;
            }
        }
        // Invariant: every entry is positive and the tables are scaled copies of each other
        // (cross products agree, ignoring rounding).
        if ok && quant_tables_are_scaled(dll, b) {
            found.push(b);
        }
    }
    found
}

fn quant_tables_are_scaled(dll: &[u8], base: usize) -> bool {
    let stride = 256usize;
    let read = |q: usize, i: usize| -> i64 {
        let o = base + q * stride + i * 4;
        i64::from(i32::from_le_bytes([
            dll[o],
            dll[o + 1],
            dll[o + 2],
            dll[o + 3],
        ]))
    };
    // All entries positive.
    for q in 0..16 {
        for i in 0..64 {
            if read(q, i) <= 0 {
                return false;
            }
        }
    }
    // table[0] and table[15] must be proportional: base0[i]*t15[0] ~= t15[i]*base0[0].
    let a0 = read(0, 0);
    let a15 = read(15, 0);
    for i in [1usize, 7, 16, 33, 63] {
        let lhs = read(0, i) * a15;
        let rhs = read(15, i) * a0;
        let diff = (lhs - rhs).abs();
        let scale = (lhs.abs() + rhs.abs()).max(1);
        if diff * 1000 > scale {
            return false;
        }
    }
    true
}

/// The first lookup table (raw nibbles tree) is `(4 << 4) | position` for positions 0..15.
const TREE0_SIGNATURE: [u8; 16] = [
    0x40, 0x41, 0x42, 0x43, 0x44, 0x45, 0x46, 0x47, 0x48, 0x49, 0x4a, 0x4b, 0x4c, 0x4d, 0x4e, 0x4f,
];

/// Locates the 16 precomputed Huffman lookup tables and their widths.
///
/// The tables are stored contiguously; after them come eight zero bytes and a 16-byte table of
/// per-tree lookup widths (each 4..=7). The start is found from the width table: the tree region
/// must end eight bytes before it, sum to `sum 2^width`, and begin with the raw-nibble table.
pub fn find_tree_tables(dll: &[u8]) -> Result<([u8; 16], usize)> {
    let mut found = None;
    for off in 8..dll.len().saturating_sub(16) {
        let m = &dll[off..off + 16];
        if m[0] != 4 || m.iter().any(|&b| !(4..=7).contains(&b)) {
            continue;
        }
        if dll[off - 8..off].iter().any(|&z| z != 0) {
            continue;
        }
        let total: usize = m.iter().map(|&b| 1usize << b).sum();
        if off < 8 + total {
            continue;
        }
        let start = off - 8 - total;
        if dll[start..start + 16] != TREE0_SIGNATURE {
            continue;
        }
        // Verify every table: all 16 leaf positions occur and every entry's code length is in
        // 1..=width. (A lookup table repeats a position 2^(width-length) times, so positions are
        // not a permutation.)
        let mut ok = true;
        let mut p = start;
        for &w in m.iter() {
            let size = 1usize << w;
            let table = &dll[p..p + size];
            let mut seen = [false; 16];
            for &e in table {
                let pos = (e & 0x0f) as usize;
                let len = (e >> 4) as usize;
                if len == 0 || len > w as usize {
                    ok = false;
                    break;
                }
                seen[pos] = true;
            }
            if !seen.iter().all(|&s| s) {
                ok = false;
            }
            p += size;
        }
        if !ok {
            continue;
        }
        let mut widths = [0u8; 16];
        widths.copy_from_slice(m);
        if found.is_some() {
            return Err(VideoError::new(
                VideoErrorKind::BadTable,
                "more than one Huffman lookup table set matched",
            ));
        }
        found = Some((widths, start));
    }
    found.ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::TableNotFound,
            "precomputed Huffman lookup tables not found in binkw32.dll",
        )
    })
}

/// Reads the four RLE run lengths that follow the lookup-width table.
///
/// Invariant: each run is a positive multiple of four (the refill writes runs in 4-byte units,
/// `0x3001c327..0x3001c337`) and the runs are strictly increasing.
fn read_rle_runs(dll: &[u8], start: usize, widths: &[u8; 16]) -> Result<[u8; 4]> {
    let total: usize = widths.iter().map(|&b| 1usize << b).sum();
    let at = start + total + 8 + 16;
    let runs: [u8; 4] = dll
        .get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .ok_or_else(|| {
            VideoError::new(
                VideoErrorKind::TableNotFound,
                "RLE run-length table past the end of binkw32.dll",
            )
        })?;
    let ok = runs.iter().all(|&r| r != 0 && r % 4 == 0 && r <= 64)
        && runs.windows(2).all(|w| w[0] < w[1]);
    if !ok {
        return Err(VideoError::new(
            VideoErrorKind::BadTable,
            format!("RLE run-length table {runs:?} fails its invariant"),
        ));
    }
    Ok(runs)
}

fn read_tree_tables(dll: &[u8], start: usize, widths: &[u8; 16]) -> Vec<Vec<u8>> {
    let mut out = Vec::with_capacity(16);
    let mut p = start;
    for &w in widths.iter() {
        let size = 1usize << w;
        out.push(dll[p..p + size].to_vec());
        p += size;
    }
    out
}

fn read_huffman(dll: &[u8], offset: usize) -> HuffmanLengths {
    let mut rows = [[0u8; 16]; 16];
    for (r, row) in rows.iter_mut().enumerate() {
        row.copy_from_slice(&dll[offset + r * 16..offset + r * 16 + 16]);
    }
    HuffmanLengths { rows }
}

fn read_patterns(dll: &[u8], offset: usize) -> Patterns {
    let mut patterns = [[0u8; 64]; 16];
    for (p, pat) in patterns.iter_mut().enumerate() {
        pat.copy_from_slice(&dll[offset + p * 64..offset + p * 64 + 64]);
    }
    Patterns { patterns }
}

/// Finds the compact two-colour pattern mask table and verifies the following table is its
/// bitwise complement. The mask family is recognized structurally: its four bytes encode the
/// four bits of each 4-pixel nibble.
pub fn find_binary_patterns(dll: &[u8]) -> Result<usize> {
    let len = 16 * 4;
    for start in 0..dll.len().saturating_sub(len * 2) {
        let mut matches = true;
        for i in 0..16usize {
            let mut expected = 0u32;
            for bit in 0..4 {
                expected |= u32::from(if i & (1 << bit) != 0 { 0xffu8 } else { 0 }) << (bit * 8);
            }
            let o = start + i * 4;
            let actual = u32::from_le_bytes([dll[o], dll[o + 1], dll[o + 2], dll[o + 3]]);
            if actual != expected {
                matches = false;
                break;
            }
            let o = start + len + i * 4;
            let complement = u32::from_le_bytes([dll[o], dll[o + 1], dll[o + 2], dll[o + 3]]);
            if complement != !expected {
                matches = false;
                break;
            }
        }
        if matches {
            return Ok(start);
        }
    }
    Err(VideoError::new(
        VideoErrorKind::TableNotFound,
        "two-colour pattern mask tables not found in binkw32.dll",
    ))
}

fn read_binary_patterns(dll: &[u8], offset: usize) -> BinaryPatterns {
    let mut masks = [0u32; 16];
    for (i, mask) in masks.iter_mut().enumerate() {
        let o = offset + i * 4;
        *mask = u32::from_le_bytes([dll[o], dll[o + 1], dll[o + 2], dll[o + 3]]);
    }
    BinaryPatterns { masks }
}

fn read_quant(dll: &[u8], offset: usize) -> QuantTables {
    let mut tables = [[0i32; 64]; 16];
    for (q, row) in tables.iter_mut().enumerate() {
        for (i, v) in row.iter_mut().enumerate() {
            let o = offset + q * 256 + i * 4;
            *v = i32::from_le_bytes([dll[o], dll[o + 1], dll[o + 2], dll[o + 3]]);
        }
    }
    QuantTables { tables }
}

// ---------------------------------------------------------------------------------------------
// Bink Audio tables
// ---------------------------------------------------------------------------------------------

/// Number of critical-frequency entries the audio decoder scans (loop bound `0x19` at
/// `0x3001b163` in the init routine `0x3001b0c0`).
pub const AUDIO_CRITICAL_FREQS: usize = 25;

/// Number of power-of-two exponent scales indexed by the 5-bit exponent of the packed 29-bit
/// float (`fmul qword [8*e + table]` at `0x3001aebf`). Only exponents `0..=23` are backed by
/// the table in the shipped DLL; the next 8-byte slots belong to an unrelated table.
pub const AUDIO_EXPONENTS: usize = 24;

/// The fixed tables of the Bink Audio (DCT) decoder, read from the installation's DLL.
///
/// In the shipped `binkw32.dll` they sit together in the `BINKDATA` section: the 16 RLE run
/// multipliers (read by `movzx eax, byte [eax + rle]` at `0x3001abe6`), immediately followed by
/// the 25 critical frequencies (`0x3001b133` / `0x3001b340`), one zero pad dword, and the
/// 24 `f64` exponent scales (`0x3001aebf`, `0x3001af34`).
#[derive(Debug, Clone, PartialEq)]
pub struct AudioTables {
    /// Run multipliers: a coefficient run flagged with the RLE bit covers `8 * rle[i]` values.
    pub rle: [u8; 16],
    /// Critical band edges in Hz, `freqs[0] == 0`, strictly increasing.
    pub critical_freqs: [u32; AUDIO_CRITICAL_FREQS],
    /// Scale for each exponent value of the packed 29-bit float (`2^(e - 23)` on the shipped DLL,
    /// checked as an invariant).
    pub exponent_scale: [f64; AUDIO_EXPONENTS],
    /// Byte offset of the RLE table in the DLL file (diagnostics).
    pub rle_offset: usize,
    /// Size in bytes of the DLL the tables were read from.
    pub dll_size: u64,
    /// FNV-1a 64-bit hash of the DLL bytes (diagnostics only).
    pub dll_fnv1a: u64,
}

impl AudioTables {
    /// Reads and locates the audio tables from the `binkw32.dll` of an owned installation.
    pub fn from_install(game_dir: &Path) -> Result<Self> {
        let dll = find_binkw32(game_dir)?;
        let bytes = std::fs::read(&dll).map_err(|e| {
            VideoError::new(
                VideoErrorKind::Io,
                format!("cannot read {}: {e}", dll.display()),
            )
        })?;
        Self::from_dll_bytes(&bytes)
            .map_err(|e| VideoError::new(e.kind(), format!("{}: {}", dll.display(), e.message())))
    }

    /// Locates the audio tables in DLL bytes.
    ///
    /// Anchor: a run of [`AUDIO_EXPONENTS`] little-endian `f64` values where each entry is
    /// exactly twice the previous one and the last is `1.0`. Relative to the anchor (layout of
    /// the shipped DLL, see the struct docs) the critical-frequency table ends one zero dword
    /// before it and the RLE table directly precedes the frequencies. Every table is checked:
    /// frequencies start at 0, are strictly increasing and below 65536 (the sample rate is a
    /// 16-bit header field); RLE multipliers are positive and strictly increasing; the pad
    /// dword is zero. Exactly one location may match.
    pub fn from_dll_bytes(dll: &[u8]) -> Result<Self> {
        let exp_len = AUDIO_EXPONENTS * 8;
        let freq_len = AUDIO_CRITICAL_FREQS * 4;
        let lead = 16 + freq_len + 4; // rle + freqs + pad dword before the anchor
        let mut found: Option<usize> = None;
        let mut b = lead;
        while b + exp_len <= dll.len() {
            if read_f64(dll, b + exp_len - 8) == 1.0 && exponent_run_ok(dll, b) {
                let rle_at = b - lead;
                if audio_layout_ok(dll, rle_at) {
                    if found.is_some() {
                        return Err(VideoError::new(
                            VideoErrorKind::BadTable,
                            "more than one Bink Audio table block matched",
                        ));
                    }
                    found = Some(rle_at);
                }
            }
            b += 1;
        }
        let rle_at = found.ok_or_else(|| {
            VideoError::new(
                VideoErrorKind::TableNotFound,
                "Bink Audio tables (RLE runs, critical frequencies, exponent scales) not found in binkw32.dll",
            )
        })?;
        let mut rle = [0u8; 16];
        rle.copy_from_slice(&dll[rle_at..rle_at + 16]);
        let critical_freqs: [u32; AUDIO_CRITICAL_FREQS] =
            std::array::from_fn(|i| read_u32(dll, rle_at + 16 + i * 4));
        let exp_at = rle_at + lead;
        let exponent_scale: [f64; AUDIO_EXPONENTS] =
            std::array::from_fn(|i| read_f64(dll, exp_at + i * 8));
        Ok(AudioTables {
            rle,
            critical_freqs,
            exponent_scale,
            rle_offset: rle_at,
            dll_size: dll.len() as u64,
            dll_fnv1a: fnv1a64(dll),
        })
    }
}

fn read_u32(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([d[o], d[o + 1], d[o + 2], d[o + 3]])
}

fn read_f64(d: &[u8], o: usize) -> f64 {
    let mut b = [0u8; 8];
    b.copy_from_slice(&d[o..o + 8]);
    f64::from_le_bytes(b)
}

/// Each entry is exactly twice the previous one, positive and finite.
fn exponent_run_ok(d: &[u8], at: usize) -> bool {
    let mut prev = read_f64(d, at);
    if !(prev.is_finite() && prev > 0.0) {
        return false;
    }
    for i in 1..AUDIO_EXPONENTS {
        let v = read_f64(d, at + i * 8);
        if v != prev * 2.0 {
            return false;
        }
        prev = v;
    }
    true
}

fn audio_layout_ok(d: &[u8], rle_at: usize) -> bool {
    let rle = &d[rle_at..rle_at + 16];
    if rle[0] == 0 || !rle.windows(2).all(|w| w[0] < w[1]) {
        return false;
    }
    let f = |i: usize| read_u32(d, rle_at + 16 + i * 4);
    if f(0) != 0 {
        return false;
    }
    if !(1..AUDIO_CRITICAL_FREQS).all(|i| f(i) > f(i - 1) && f(i) < 65536) {
        return false;
    }
    read_u32(d, rle_at + 16 + AUDIO_CRITICAL_FREQS * 4) == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a synthetic buffer containing a planted Huffman length table, pattern table,
    /// scan permutation and quant tables, at known offsets with padding.
    fn plant() -> (Vec<u8>, usize, usize, usize) {
        let mut dll = vec![0u8; 16384];

        let huff_at = 100usize;
        // Row 0: all fours. Rows 1..: a complete prefix code (use all length 4 again).
        for r in 0..16 {
            for c in 0..16 {
                dll[huff_at + r * 16 + c] = 4;
            }
        }

        let scan_at = 500usize;
        for i in 0..64 {
            dll[scan_at + i] = ((i % 8) * 8 + i / 8) as u8; // transpose permutation
        }
        // 8 zero bytes then 16 permutations.
        let pat_at = scan_at + 72;
        for p in 0..16 {
            for i in 0..64 {
                dll[pat_at + p * 64 + i] = ((i * 5 + p) % 64) as u8;
            }
        }

        let quant_at = 2000usize;
        let expected = expected_quant_first_column();
        // Two families (intra, then inter) with the same first column but different bases.
        for (family, base) in [(quant_at, 1000.0), (quant_at + 4096 + 32, 1300.0)] {
            for q in 0..16 {
                for i in 0..64 {
                    let v = if i == 0 {
                        expected[q]
                    } else {
                        // A positive base matrix scaled by the quantiser ratio.
                        (base * QUANTISERS[q] / QUANTISERS[0] + 0.5).floor() as i32 * (i as i32 + 1)
                    };
                    dll[family + q * 256 + i * 4..family + q * 256 + i * 4 + 4]
                        .copy_from_slice(&v.to_le_bytes());
                }
            }
        }

        // Precomputed Huffman lookup tables: 16 tables of 16 bytes (width 4), then eight zeros,
        // then the width table.
        let tree_at = 12200usize;
        for t in 0..16 {
            for i in 0..16 {
                dll[tree_at + t * 16 + i] = 0x40 + i as u8;
            }
        }
        let widths_at = tree_at + 16 * 16 + 8;
        for i in 0..16 {
            dll[widths_at + i] = 4;
        }
        // Synthetic RLE run lengths (any increasing multiples of four).
        dll[widths_at + 16..widths_at + 20].copy_from_slice(&[4, 8, 16, 20]);
        let binary_at = 12600usize;
        for i in 0..16usize {
            let mut mask = 0u32;
            for bit in 0..4 {
                mask |= u32::from(if i & (1 << bit) != 0 { 0xffu8 } else { 0 }) << (bit * 8);
            }
            dll[binary_at + i * 4..binary_at + i * 4 + 4].copy_from_slice(&mask.to_le_bytes());
            dll[binary_at + 64 + i * 4..binary_at + 64 + i * 4 + 4]
                .copy_from_slice(&(!mask).to_le_bytes());
        }
        (dll, huff_at, pat_at, quant_at)
    }

    #[test]
    fn locates_planted_tables() {
        let (dll, huff, pat, quant) = plant();
        assert_eq!(find_huffman_lengths(&dll).unwrap(), huff);
        assert_eq!(find_patterns(&dll).unwrap(), pat);
        let scan = find_scan_from_patterns(&dll, pat).unwrap();
        assert!(is_perm64(&scan));
        assert_eq!(find_quant(&dll).unwrap(), quant);
        assert_eq!(find_quant_pair(&dll).unwrap(), (quant, quant + 4096 + 32));
        assert_eq!(find_binary_patterns(&dll).unwrap(), 12600);
        let (widths, tree_at) = find_tree_tables(&dll).unwrap();
        assert_eq!(widths, [4u8; 16]);
        assert_eq!(tree_at, 12200);
        let tables = BinkTables::from_dll_bytes(&dll).unwrap();
        assert_eq!(tables.huffman_lengths.rows[0], [4u8; 16]);
        assert_eq!(tables.huffman_tables.len(), 16);
        assert_eq!(tables.tree_maxbits[0], 4);
        assert_eq!(tables.rle_runs, [4, 8, 16, 20]);
        assert_eq!(tables.binary_patterns.masks[0], 0);
        assert_eq!(tables.binary_patterns.masks[15], u32::MAX);
        assert_eq!(
            tables.quant.tables[15][0],
            expected_quant_first_column()[15]
        );
    }

    #[test]
    fn missing_tables_are_reported() {
        let dll = vec![0u8; 4096];
        assert_eq!(
            find_huffman_lengths(&dll).unwrap_err().kind(),
            VideoErrorKind::TableNotFound
        );
        assert_eq!(
            find_quant(&dll).unwrap_err().kind(),
            VideoErrorKind::TableNotFound
        );
    }

    #[test]
    fn corrupt_quant_is_rejected() {
        let (mut dll, _h, _p, quant) = plant();
        dll[quant] ^= 0xFF; // break the first column of the intra family
        // Only one family is left, so the intra/inter pair cannot be formed.
        assert!(find_quant_pair(&dll).is_err());
        assert_ne!(find_quant(&dll).ok(), Some(quant));
    }

    #[test]
    fn bad_rle_runs_are_rejected() {
        let (mut dll, _h, _p, _q) = plant();
        let widths_at = 12200 + 16 * 16 + 8;
        dll[widths_at + 17] = 6; // not a multiple of four
        assert_eq!(
            BinkTables::from_dll_bytes(&dll).unwrap_err().kind(),
            VideoErrorKind::BadTable
        );
    }

    #[test]
    fn missing_inter_family_is_reported() {
        let (mut dll, _h, _p, quant) = plant();
        // Break the second family's first column; the intra family alone must not suffice.
        let second = quant + 4096 + 32;
        dll[second] ^= 0xFF;
        assert_eq!(
            find_quant_pair(&dll).unwrap_err().kind(),
            VideoErrorKind::TableNotFound
        );
        assert_eq!(find_quant(&dll).unwrap(), quant);
    }

    #[test]
    fn scan_requires_zero_gap_and_permutation() {
        let (mut dll, _h, pat, _q) = plant();
        dll[pat - 8] = 1; // destroy the zero gap
        assert!(find_scan_from_patterns(&dll, pat).is_err());
    }

    /// Plants a Bink Audio table block (RLE runs, 25 frequencies, zero pad, 24 exponent scales)
    /// with synthetic values at `at`.
    fn plant_audio(dll: &mut [u8], at: usize) {
        for i in 0..16 {
            dll[at + i] = (i as u8 + 1) * 2;
        }
        for i in 0..AUDIO_CRITICAL_FREQS {
            let f = (i as u32) * 700;
            dll[at + 16 + i * 4..at + 20 + i * 4].copy_from_slice(&f.to_le_bytes());
        }
        let pad = at + 16 + AUDIO_CRITICAL_FREQS * 4;
        dll[pad..pad + 4].fill(0);
        for e in 0..AUDIO_EXPONENTS {
            let v = 2f64.powi(e as i32 - 23);
            dll[pad + 4 + e * 8..pad + 12 + e * 8].copy_from_slice(&v.to_le_bytes());
        }
    }

    #[test]
    fn locates_planted_audio_tables() {
        let mut dll = vec![0u8; 8192];
        plant_audio(&mut dll, 1000);
        let t = AudioTables::from_dll_bytes(&dll).unwrap();
        assert_eq!(t.rle_offset, 1000);
        assert_eq!(t.rle[0], 2);
        assert_eq!(t.critical_freqs[24], 24 * 700);
        assert_eq!(t.exponent_scale[23], 1.0);
        assert_eq!(t.exponent_scale[0], 2f64.powi(-23));
    }

    #[test]
    fn audio_tables_fail_their_invariants() {
        // Missing entirely.
        assert_eq!(
            AudioTables::from_dll_bytes(&[0u8; 4096])
                .unwrap_err()
                .kind(),
            VideoErrorKind::TableNotFound
        );
        // Frequencies not strictly increasing.
        let mut dll = vec![0u8; 8192];
        plant_audio(&mut dll, 1000);
        dll[1000 + 16 + 5 * 4..1000 + 20 + 5 * 4].copy_from_slice(&1u32.to_le_bytes());
        assert!(AudioTables::from_dll_bytes(&dll).is_err());
        // An exponent entry that is not twice its predecessor.
        let mut dll = vec![0u8; 8192];
        plant_audio(&mut dll, 1000);
        let exp_at = 1000 + 16 + AUDIO_CRITICAL_FREQS * 4 + 4;
        dll[exp_at + 10 * 8..exp_at + 11 * 8].copy_from_slice(&3.0f64.to_le_bytes());
        assert!(AudioTables::from_dll_bytes(&dll).is_err());
        // Non-zero pad dword between the frequencies and the exponents.
        let mut dll = vec![0u8; 8192];
        plant_audio(&mut dll, 1000);
        dll[exp_at - 4] = 1;
        assert!(AudioTables::from_dll_bytes(&dll).is_err());
        // Two matching blocks are ambiguous.
        let mut dll = vec![0u8; 8192];
        plant_audio(&mut dll, 1000);
        plant_audio(&mut dll, 4000);
        assert_eq!(
            AudioTables::from_dll_bytes(&dll).unwrap_err().kind(),
            VideoErrorKind::BadTable
        );
    }
}
