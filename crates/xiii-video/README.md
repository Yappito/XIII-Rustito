# xiii-video

Clean-room Rust reader and video decoder for the Bink 1 cutscenes shipped with XIII Classic
(2003). It never reads or links a Bink implementation; it reads the fixed decoder tables from the
user's own installation at runtime by structural signature.

## Clean-room sources

The format was implemented from public prose documentation and from measurement on the files, plus
disassembly of RAD's own library that ships with the game. The following sources were used:

1. **MultimediaWiki, "Bink Container"** (based on Mike Melanson, *Description of the Bink File
   Format*, 2003): 44-byte header, audio-track header groups, the frame-offset table with the
   keyframe bit, and the per-frame audio/video packet layout.
2. **MultimediaWiki, "Bink Video"**: LSB-first bitstream; the 16 predefined Huffman trees and the
   frame's tree-number/symbol-permutation coding; the bundle readers (4-bit, 4-bit signed, 4-bit
   RLE, 4-bit pair, 8-bit, 16-bit delta); the ten block types; the note that revision `i` prefixes
   the alpha and luma planes with a 32-bit plane size.
3. **Kostya Shirayev ("Kostya's Boring Codec World") prose posts** used for background only:
   "Bink: 'lossy' coefficients reading" and "Bink: 'lenless' block coding" (the residue/DCT tree
   shape and mask progression), "Bink: a bunch of peculiarities" (16 quantisers), and "Bink
   encoder: coefficients coding" (the list-based coefficient-tree traversal).
4. **`XIII_Game/system/binkw32.dll`** (RAD Game Tools, read-only, shipped with the game): the fixed
   tables are *located and read from this binary at runtime*, never copied into the repository.
   Disassembly with `llvm-objdump` (output only under git-ignored `local/re/`) established the
    plane-header tree order, bundle refill behavior and lookup-table representation; see below.
5. **FFmpeg binary as a black-box oracle only**: reference PNG frames for comparison; no FFmpeg
   source was opened, fetched or paraphrased. (Incidental note: unrelated web-search result
   excerpts displayed FFmpeg identifiers; they were not opened and no table bytes or code structure
   were taken from them. See the task report's compliance statement.)

## Fixed tables read from `binkw32.dll`

None of these bytes live in the repository. `BinkTables::from_install(game_dir)` locates each table
by signature and checks an invariant:

| Table | Location method | Invariant |
|---|---|---|
| Huffman code lengths (16×16) | 256-byte window whose row 0 is sixteen `4`s and every row is a complete prefix code | `sum 2^-len == 1` per row |
| Huffman lookup tables (16) | the width table (`4..=7`) preceded by eight zero bytes and followed, `sum 2^width` earlier, by the raw-nibble table `40 41 .. 4f` | every table uses all 16 leaf positions and code lengths `1..=width` |
| Run-fill patterns (16×64) | sixteen consecutive 64-byte permutations preceded by eight zero bytes and the DCT scan permutation | each chunk is a permutation of `0..63` |
| Two-colour masks (16×`u32`) | structural search for four replicated-byte bits per nibble plus the following complement table | every nibble maps to its four mask bits; second table is bitwise complement |
| DCT scan order (64) | the permutation immediately before the pattern table | permutation of `0..63` |
| Dequantisation (16×64 `i32`) | first column equals `round(65536 * q)` for the documented quantisers `{1, 4/3, 5/3, 2, 8/3, 3.5, 4, 5, 6, 8, 12, 17, 22, 28, 34, 44}` | positive; tables proportional |

On the shipped GOG DLL (375808 bytes, FNV-1a64 `4362b2f63155a3e5`) the locator finds: Huffman row 0
`[4;16]`, pattern 0 `[0,8,16,24,32,40,48,56,...]`, scan `[0,1,8,9,2,3,10,11,...]`, and quant
`q0[0]=65536`, `q15[0]=2883584`.

## Decoding

`Decoder::decode_frame` decodes the Y, U, V (and alpha) planes; `YuvFrame::to_rgba` converts to
RGBA8 with a labelled full-range BT.601 matrix. The output is diagnostic: the module is under
validation and returns an explicit error for constructs it cannot decode (see the task report).

The DLL's common constant-refill path reads a 4-bit value and replicates it into each output byte
(`0x3001c35d..0x3001c3c0`); colour bundles use that expansion as well. The decoder remains
incomplete: a successful frame return is not evidence of pixel agreement or complete cutscene
support.
