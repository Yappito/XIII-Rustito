# xiii-video

Clean-room Rust reader, video decoder and Bink Audio (DCT) decoder for the Bink 1 (revision `i`)
cutscenes shipped with XIII Classic (2003). It never reads or links a Bink implementation; it reads the fixed decoder
tables from the user's own installation at runtime by structural signature.

**Status (measured, see `local/reports/item17b-bink-cleanroom.md`):** all 24,167 frames of the
19 GOG cutscenes decode with 0 errors. Every compared frame (all 302 frames of `ubi.bik`, plus
6 sampled frames from each of the 19 files) is byte-identical in Y, Cb and Cr to the FFmpeg
black-box oracle. The alpha-plane and grayscale paths are implemented from the disassembly but
no XIII file uses them, so they are untested on real data.

## Clean-room sources

The format was implemented from public prose documentation, measurement on the files, and
disassembly of RAD's own library that ships with the game. The following sources were used:

1. **MultimediaWiki, "Bink Container"** (based on Mike Melanson, *Description of the Bink File
   Format*, 2003): 44-byte header, audio-track header groups, the frame-offset table with the
   keyframe bit, and the per-frame audio/video packet layout.
2. **MultimediaWiki, "Bink Video"**: LSB-first bitstream; the 16 predefined Huffman trees and the
   frame's tree-number/symbol-permutation coding; the bundle idea; the ten block types; the note
   that revision `i` prefixes the alpha and luma planes with a 32-bit plane size.
3. **Kostya Shirayev ("Kostya's Boring Codec World") prose posts**, background only: the
   coefficient work-list idea and the 16 quantisers.
4. **`XIII_Game/system/binkw32.dll`** (RAD Game Tools, read-only, shipped with the game): every
   bit-level rule of the decoder was established by disassembling this DLL with `llvm-objdump`
   (output only under git-ignored `local/re/`); the addresses are cited in `src/decoder.rs`. The
   fixed tables are *located and read from this binary at runtime*, never copied into the
   repository.
5. **FFmpeg binary as a black-box oracle only**: reference frames (raw yuv420p / PNG) produced by
   running the installed `ffmpeg.exe`, compared byte-for-byte. No FFmpeg source was opened,
   fetched or paraphrased. (Incidental note from the first pass: unrelated web-search result
   excerpts displayed FFmpeg identifiers; they were not opened and nothing was taken from them.
   See the task report's compliance statement.)
6. **Bink Audio (item17c)** was implemented *only* from the `binkw32.dll` disassembly (the
   audio routines at `0x3001adf0..0x3001b470`, `BinkOpen`/`BinkOpenTrack`/`BinkGetTrackData`,
   and the transform routines it calls) and from measurement against the FFmpeg binary run as a
   black-box oracle (`ffmpeg -i X.bik -map 0:a:N -f s16le`). No prose page or Bink Audio source
   was read for this, and no FFmpeg, OpenGothic or other decoder source was opened or fetched.
   The transform is pinned down by what it computes (the definition is given under Bink Audio
   below). The DLL routine has the structure of a general-purpose FFT/DCT package (twiddle and
   cosine tables built on first use, `ip[0] = 0`). Its definition was taken as the standard
   unnormalised DCT-III form, as a hypothesis from general knowledge, and then confirmed
   numerically: it is the form that reaches > 100 dB against the oracle. No source of that
   package was opened.

## Fixed tables read from `binkw32.dll`

None of these bytes live in the repository. `BinkTables::from_install(game_dir)` locates each table
by signature and checks an invariant:

| Table | Location method | Invariant |
|---|---|---|
| Huffman code lengths (16x16) | 256-byte window whose row 0 is sixteen `4`s and every row is a complete prefix code | `sum 2^-len == 1` per row |
| Huffman lookup tables (16) | the width table (`4..=7`) preceded by eight zero bytes and the tables, starting with the raw-nibble table `40 41 .. 4f` | every table uses all 16 leaf positions and code lengths `1..=width` |
| RLE run lengths (4) | the four bytes after the width table | positive multiples of 4, strictly increasing |
| Run-fill patterns (16x64) | sixteen consecutive 64-byte permutations preceded by eight zero bytes and the DCT scan permutation | each chunk is a permutation of `0..63` |
| Two-colour masks (16 x `u32`) | four replicated-byte bits per nibble plus the following complement table | every nibble maps to its four mask bits; second table is the complement |
| DCT scan order (64) | the permutation immediately before the pattern table | permutation of `0..63`; matches the hard-coded scatter at `0x3002145c` |
| Dequantisation, intra and inter (2 x 16x64 `i32`) | the first two non-overlapping families whose first column equals `round(65536 * q)` for the documented quantisers | positive; tables of a family proportional; the two families differ |

On the shipped GOG DLL (375808 bytes, FNV-1a64 `4362b2f63155a3e5`; the Steam copy is identical)
the locator finds intra `q0[63]=25879`, inter `q0[63]=10289`, lookup widths
`[4,5,5,5,5,5,5,6,6,6,6,6,6,7,7,7]`.

## Decoding (all rules from `binkw32.dll`)

* **Packet** (`0x3001f350`): `[alpha size, alpha plane]`, luma size, luma plane, Cr plane, Cb
  plane. Each size counts from its own size word; Cr starts at `luma size word + size`; Cb starts
  at the 32-bit word after the last word the Cr bit reader loaded. Plane sizes:
  `align8(w) x align8(h)` and `align8((w+1)/2) x align8((h+1)/2)`. (Cr-before-Cb was measured
  against the oracle.)
* **Plane** (`0x3001d3b0`): 23 tree descriptors (block types, sub-types, 16 colour-context trees
  plus the low-nibble tree, pattern, X, Y, run). At the start of **every 8-pixel block row** each
  of the nine bundles is refilled in that order **only if its read pointer equals its end pointer**;
  a zero count sets read = buffer+4, end = buffer, which disables further refills for the plane.
  Count widths are `floor(log2(n + 511)) + 1` for n = bw, bw/2, 64bw, 8bw, bw, bw, bw, bw, 48bw.
* **Bundles**: block/sub types are 4-bit RLE (raw-nibble constant fill, or symbols where 12..15
  repeat the last literal by the DLL's run lengths; the count is in output values); colours use
  the previous high nibble as context (constant fill decodes one coded colour); pattern bytes are
  two symbols; motion components carry a sign bit; DC values are 11-bit (intra) or 10-bit+sign
  (inter) starts followed by groups of up to 8 deltas of a 4-bit width.
* **Blocks**: 0 skip, 1 16x16 (sub-type 3/5/6/8/9 decoded at 8x8 and pixel-doubled; written on
  even block rows only), 2 motion, 3 run (bits only while < 63 pixels are filled), 4 motion +
  residue (7-bit mask budget, 3-bit pass count), 5 intra DCT, 6 fill, 7 motion + inter DCT, 8
  two-colour pattern, 9 raw. DCT blocks read the coefficients **before** the 4-bit quantiser.
* **Coefficients** (`0x30020c70`): a work list of groups; a coefficient that becomes significant
  in the pass with threshold `2^k` reads `k` more magnitude bits and a sign at once.
* **IDCT** (`0x3001f4d0`, `0x30020500`): 11-bit fixed-point AAN-style butterflies (2896, 3784,
  2217, -5352), dequantisation `coef * q >> 11` in the column pass, output `(v + 127) >> 8`
  truncated to a byte (intra) or added with byte wrap-around (inter). No saturation anywhere.

`YuvFrame::to_rgba` converts to RGBA8 with ITU-R BT.601 limited range and nearest-sample chroma
(labelled assumption matching how FFmpeg presents Bink; RAD's own blitters were not analysed);
against FFmpeg's RGB PNGs this gives 47-50 dB, the residual being chroma interpolation only.

## Bink Audio (DCT variant), `src/audio.rs`

**Status (measured, see `local/reports/item17c-bink-audio.md`):** every audio packet of every
track in the 18 cutscenes that have audio (100,190 packets: 4 single-track files plus 14 five-track files) decodes with
0 errors. Against the FFmpeg black-box oracle (`-map 0:a:N -f s16le`), track 0 of every file
reaches **85.8 to 92.4 dB SNR** with max |diff| 1 LSB (one exception: 186 LSB on one
saturated sample of `Cine10`, explained below). Outside the block cross-fades the SNR is
**102.5 to 105.3 dB**. Tracks 1 to 4 of `Cine02` measure 89.6 to 90.0 dB. Not bit-exact
against FFmpeg, for two documented reasons:

* **Cross-fade rounding.** The DLL blends 16-bit samples and floors (unsigned `div` of the
  signed sum, `0x3001b40f`). FFmpeg rounds. About 98% of the differing samples are -1 LSB inside
  the 128-frame cross-fades.
* **Clip before blend.** The DLL saturates a block to 16 bits before cross-fading. FFmpeg blends
  floats. On `Cine10` block 302 a pre-clip value of -33105.6 therefore gives a 186 LSB
  difference, which is (-33105.6 + 32768) * 142 / 256 (measured).

The remaining ~0.04% body differences are ±1 rounding from float precision (`f64` transform
here vs the DLL's x87/`f32` and FFmpeg's float path). The decoded length also follows the DLL:
each packet produces exactly its declared byte count. FFmpeg emits whole blocks, so its output is
longer by the truncated tail (e.g. 1280 samples on `ubi.bik`).

The **RDFT variant** (tracks without flag `0x1000`, or with `0x8000`) is not implemented and is
reported as `Unsupported`, because no XIII file uses it.

### Audio tables read from `binkw32.dll`

`AudioTables::from_install(game_dir)` locates them as one block in `BINKDATA`. None of these
values are in the repository.

| Table | Location method | Invariant |
|---|---|---|
| Exponent scales (24 x `f64`) | anchor: 24 doubles, each exactly twice the previous, the last `1.0` (used at `0x3001aebf`) | doubling run |
| Critical frequencies (25 x `u32`) | ends one zero dword before the anchor (`0x3001b133`, `0x3001b340`) | `f[0] == 0`, strictly increasing, below 65536; pad dword is zero |
| RLE run multipliers (16 bytes) | directly before the frequencies (`0x3001abe6`) | positive, strictly increasing |

There must be exactly one match. On the GOG DLL it is at file offset `0x44840`.

### Decoding rules (addresses in `binkw32.dll`)

* **Track header** (`container.rs`): `N` tracks are stored as three groups of `N` dwords (max
  decoded bytes, `rate | flags << 16`, id), not as 12-byte records. Flag `0x2000` = stereo
  (`0x30015afa`). `0x4000` = 16-bit output (`0x30015b76`). DCT variant = `0x1000` set and
  `0x8000` clear (`BinkOpen` `0x30012fe5`).
* **Setup** (`0x3001b0c0`): `N` = 2048 / 1024 / 512 for rates of at least 44100 Hz /
  22050 Hz / below. Scale = `f32(2 / sqrt(N))`. Band count = the first critical frequency at or
  above `(rate+1)/2` (max 25). Band edges = `max(1, f * (N/2) / ((rate+1)/2))`, closed by `N/2`.
* **Packet** (`BinkGetTrackData` `0x30015c20`, `BinkDoFrame` `0x300141b4`): the leading
  dword is the decoded byte count. Blocks are decoded until it is reached, and the last block's
  output is truncated.
* **Block** (`0x3001adf0`): LSB-first over dwords. The DCT variant skips 2 bits (`0x3001ae1b`).
  Per channel:
  * two packed 29-bit floats (5-bit exponent, 23-bit mantissa, sign at bit 28; `0x3001ae60`);
  * one 8-bit quantiser per band, `q = 10^(i * 0.0664)` (`0x3001af50`, constant at `0x3004ff04`);
  * coefficients 2..N (`0x3001ab00`): a flag bit chooses a run of `8 * rle[4 bits]` or 8
    values, then a 4-bit width (0 = zero run), then per value its magnitude and, when non-zero,
    a sign bit. The quantiser starts at 0.0 and changes when the index *equals* `2 * band[k]`
    (zero runs catch up with `>`);
  * inverse DCT `x[k] = sum a[j] cos(pi j (k + 1/2) / N)` (`ddct(N, +1)` at `0x300013d0`).

  A block consumes whole dwords (`0x3001b098`).
* **Output** (`0x3001aa20` / `0x3001a950`): `saturate(round_half_even(x * scale))`, with
  channels interleaved. An out-of-range `fistp` gives -32768.
* **Overlap** (`0x3001b3d7..0x3001b437`): the last `N*ch/16` interleaved samples are held back.
  The next block's head becomes `(prev*(n-i) + cur*i) / n` (floor). The first block is not
  cross-faded.

The transform is evaluated in `f64` through a zero-padded complex FFT of length `2N`. Unit tests
check it against the direct O(N²) definition.
