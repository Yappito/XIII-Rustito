//! Bink Audio (DCT variant) decoder for the XIII cutscenes.
//!
//! Every rule below was established by disassembling RAD's own `binkw32.dll` shipped with the
//! game (addresses cited inline); the fixed tables (RLE run multipliers, critical band
//! frequencies, exponent scales) are read from that DLL at runtime by
//! [`crate::tables::AudioTables`]. No Bink implementation source was consulted.
//!
//! Structure (DLL routine in brackets):
//!
//! * **Track setup** (`0x3001b0c0`): transform length `N` = 2048 for rates of at least 44100 Hz,
//!   1024 from 22050 Hz, else 512; output scale `2 / sqrt(N)` (stored as `f32`); band edges
//!   `max(1, freq[i] * (N/2) / ((rate + 1) / 2))` for every critical frequency below
//!   `(rate + 1) / 2`, closed by `N/2`.
//! * **Packet** (`BinkGetTrackData` `0x30015c20`, `BinkDoFrame` `0x300141b4`): the packet's
//!   leading dword is the decoded byte count; blocks are decoded until that many bytes of 16-bit
//!   PCM exist, the last block's output being truncated.
//! * **Block** (`0x3001adf0`): an LSB-first bitstream over little-endian dwords. In the DCT
//!   variant the first two bits are skipped (`0x3001ae1b`). Per channel: two packed 29-bit
//!   floats (coefficients 0 and 1), one 8-bit quantiser index per band
//!   (`q = 10^(index * 0.0664)`), then the run/width coded coefficients 2..N (`0x3001ab00`),
//!   then an inverse DCT of length `N` (Ooura-style `ddct(N, +1)` at `0x300013d0`). The block
//!   consumes whole dwords (`0x3001b098`).
//! * **Output** (`0x3001aa20` stereo / `0x3001a950` mono): `sample = saturate(round(x * scale))`
//!   interleaved per channel.
//! * **Overlap** (`0x3001b3e8..0x3001b437`): the last `N * channels / 16` interleaved samples of
//!   every block are held back and linearly cross-faded into the first samples of the next
//!   block; the first block of the stream is not cross-faded.

use crate::bitreader::BitReader;
use crate::container::AudioTrack;
use crate::error::{Result, VideoError, VideoErrorKind};
use crate::tables::{AUDIO_CRITICAL_FREQS, AudioTables};

/// Scalar of the quantiser exponent: `q = 10 ^ (index * QUANT_EXPONENT)`. The `f32` constant
/// at `0x3004ff04` (multiplied at `0x3001af97`); the base `10.0` is the `f64` at `0x3004fec0`
/// and the power is taken by the CRT `pow` helper called at `0x3001af9d`.
const QUANT_EXPONENT: f32 = 0.0664;

/// Counters the decoder keeps across blocks.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioDecodeStats {
    /// Packets decoded.
    pub packets: u64,
    /// Blocks (transforms per channel set) decoded.
    pub blocks: u64,
    /// Interleaved 16-bit samples emitted.
    pub samples: u64,
    /// Packed floats whose 5-bit exponent was above 23. The DLL indexes past its 24-entry
    /// exponent table for these (into unrelated data); this decoder uses `2^(e - 23)` and
    /// counts them so the case is never hidden. Zero on every XIII file.
    pub exponent_out_of_table: u64,
    /// Output samples that saturated to the 16-bit range.
    pub clipped: u64,
    /// Bytes left in a packet payload after its declared output was produced.
    pub trailing_bytes: u64,
}

/// Result of decoding one packet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PacketStats {
    /// Blocks decoded for this packet.
    pub blocks: usize,
    /// Interleaved samples appended.
    pub samples: usize,
    /// Payload bytes consumed by the blocks (whole dwords).
    pub bytes_used: usize,
}

/// Streaming decoder for one Bink Audio (DCT) track.
#[derive(Debug, Clone)]
pub struct AudioDecoder {
    sample_rate: u32,
    channels: usize,
    frame_len: usize,
    scale: f32,
    /// `bands[k]` for `k in 0..=num_bands`, in units of coefficient pairs.
    bands: Vec<usize>,
    rle: [u8; 16],
    exponent_scale: [f64; crate::tables::AUDIO_EXPONENTS],
    overlap: Vec<i16>,
    first: bool,
    dct: Dct3,
    coeffs: Vec<Vec<f32>>,
    quant: Vec<f32>,
    block: Vec<i16>,
    stats: AudioDecodeStats,
}

impl AudioDecoder {
    /// Creates a decoder for `track` using the DLL tables.
    ///
    /// Only the DCT variant is implemented (every XIII track uses it); an RDFT track returns
    /// [`VideoErrorKind::Unsupported`].
    pub fn new(tables: &AudioTables, track: &AudioTrack) -> Result<Self> {
        if !track.audio_dct() {
            return Err(VideoError::new(
                VideoErrorKind::Unsupported,
                format!(
                    "Bink Audio RDFT variant (flags 0x{:04x}) is not implemented; only the DCT variant is",
                    track.flags
                ),
            ));
        }
        Self::with_format(tables, u32::from(track.sample_rate), track.channels())
    }

    /// Creates a DCT-variant decoder for an explicit sample rate and channel count.
    pub fn with_format(tables: &AudioTables, sample_rate: u32, channels: u16) -> Result<Self> {
        if sample_rate == 0 || !(1..=2).contains(&channels) {
            return Err(VideoError::new(
                VideoErrorKind::BadValue,
                format!("invalid audio format {sample_rate} Hz x {channels} channels"),
            ));
        }
        // 0x3001b0c3..0x3001b0f0: transform length from the sample rate.
        let frame_len: usize = if sample_rate >= 44100 {
            2048
        } else if sample_rate >= 22050 {
            1024
        } else {
            512
        };
        let half = frame_len / 2;
        // 0x3001b122: (rate + 1) >> 1.
        let nyquist = sample_rate.div_ceil(2);
        // 0x3001b131..0x3001b177: number of bands = first critical frequency >= nyquist.
        let num_bands = tables
            .critical_freqs
            .iter()
            .position(|&f| f >= nyquist)
            .unwrap_or(AUDIO_CRITICAL_FREQS);
        // 0x3001b340..0x3001b375: band edges, clamped to at least 1, closed by N/2.
        let mut bands = Vec::with_capacity(num_bands + 1);
        for &f in &tables.critical_freqs[..num_bands] {
            let b = (u64::from(f) * half as u64 / u64::from(nyquist)) as usize;
            bands.push(b.max(1));
        }
        bands.push(half);
        // 0x3001b317..0x3001b338: scale = 2.0 / sqrt(N) stored as f32.
        let scale = (2.0f64 / (frame_len as f64).sqrt()) as f32;
        let channels = usize::from(channels);
        let n_overlap = frame_len * channels / 16;
        Ok(AudioDecoder {
            sample_rate,
            channels,
            frame_len,
            scale,
            bands,
            rle: tables.rle,
            exponent_scale: tables.exponent_scale,
            overlap: vec![0; n_overlap],
            first: true,
            dct: Dct3::new(frame_len),
            coeffs: vec![vec![0.0; frame_len]; channels],
            quant: vec![0.0; num_bands],
            block: vec![0; frame_len * channels],
            stats: AudioDecodeStats::default(),
        })
    }

    /// Output sample rate in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Output channel count (samples are interleaved).
    pub fn channels(&self) -> u16 {
        self.channels as u16
    }

    /// Transform length per channel.
    pub fn frame_len(&self) -> usize {
        self.frame_len
    }

    /// Interleaved samples one block contributes to the output (block minus the held-back
    /// overlap).
    pub fn samples_per_block(&self) -> usize {
        self.block.len() - self.overlap.len()
    }

    /// Band edges (coefficient pairs) including the closing `N/2`.
    pub fn bands(&self) -> &[usize] {
        &self.bands
    }

    /// Accumulated counters.
    pub fn stats(&self) -> &AudioDecodeStats {
        &self.stats
    }

    /// Forgets the overlap history (as on a fresh open); the next block is not cross-faded.
    pub fn reset(&mut self) {
        self.first = true;
        self.overlap.fill(0);
    }

    /// Decodes one audio packet payload (the bytes after the length and decoded-size words)
    /// and appends `decoded_bytes / 2` interleaved samples to `out`.
    ///
    /// Blocks are decoded until the declared size is reached; the last block's output is
    /// truncated exactly like the DLL does (its overlap tail is still kept). Reading past the
    /// payload is an [`VideoErrorKind::OutOfData`] error.
    pub fn decode_packet(
        &mut self,
        payload: &[u8],
        decoded_bytes: u32,
        out: &mut Vec<i16>,
    ) -> Result<PacketStats> {
        // The DLL reads whole dwords; pad a ragged tail with zeros so the final dword is legal.
        let mut padded = payload.to_vec();
        padded.resize(payload.len().div_ceil(4) * 4, 0);
        let mut remaining = decoded_bytes as usize / 2;
        let mut pos = 0usize;
        let mut blocks = 0usize;
        let mut samples = 0usize;
        while remaining > 0 {
            if pos >= padded.len() {
                return Err(VideoError::new(
                    VideoErrorKind::OutOfData,
                    format!(
                        "audio packet exhausted after {blocks} blocks with {} bytes still declared",
                        remaining * 2
                    ),
                ));
            }
            let used = self.decode_block(&padded[pos..])?;
            pos += used;
            blocks += 1;
            let take = remaining.min(self.samples_per_block());
            out.extend_from_slice(&self.block[..take]);
            remaining -= take;
            samples += take;
        }
        self.stats.packets += 1;
        self.stats.samples += samples as u64;
        self.stats.trailing_bytes += payload.len().saturating_sub(pos) as u64;
        Ok(PacketStats {
            blocks,
            samples,
            bytes_used: pos,
        })
    }

    /// Decodes one block from the start of `data` into the internal block buffer (overlap
    /// applied) and returns the number of bytes it consumed (a multiple of four).
    fn decode_block(&mut self, data: &[u8]) -> Result<usize> {
        let mut br = BitReader::new(data);
        // 0x3001ae1b: the DCT variant starts from the first dword shifted right by two.
        br.skip(2);
        for ch in 0..self.channels {
            self.read_channel(&mut br, ch);
            if br.overflowed() {
                return Err(VideoError::new(
                    VideoErrorKind::OutOfData,
                    format!(
                        "audio block channel {ch} read past its {} payload bytes",
                        data.len()
                    ),
                ));
            }
            let c = &mut self.coeffs[ch];
            self.dct.inverse(c);
        }
        // 0x3001b098: bytes consumed = dwords fetched.
        let used = br.bits_read().div_ceil(32) * 4;

        // 0x3001aa20 / 0x3001a950: scale, round to nearest (x87 default), saturate.
        for i in 0..self.frame_len {
            for ch in 0..self.channels {
                let (s, clipped) = to_i16(f64::from(self.coeffs[ch][i]) * f64::from(self.scale));
                self.stats.clipped += u64::from(clipped);
                self.block[i * self.channels + ch] = s;
            }
        }

        // 0x3001b3d7..0x3001b417: cross-fade the held-back tail of the previous block.
        let n_ov = self.overlap.len();
        if self.first {
            self.first = false;
        } else {
            let n = n_ov as u32;
            for i in 0..n_ov {
                let prev = i32::from(self.overlap[i]);
                let cur = i32::from(self.block[i]);
                let sum = prev
                    .wrapping_mul(n_ov as i32 - i as i32)
                    .wrapping_add(cur.wrapping_mul(i as i32));
                // `xor edx, edx; div esi`: an unsigned 32-bit division of the signed sum,
                // truncated to 16 bits by `mov word ptr [edi], ax`. For the power-of-two
                // overlap lengths used here this equals a flooring division.
                self.block[i] = ((sum as u32) / n) as u16 as i16;
            }
        }
        // 0x3001b419..0x3001b437: keep the last N*channels/16 samples for the next block.
        let tail = self.block.len() - n_ov;
        self.overlap.copy_from_slice(&self.block[tail..]);
        self.stats.blocks += 1;
        Ok(used)
    }

    /// Reads one channel's coefficients into `self.coeffs[ch]` (not yet transformed).
    fn read_channel(&mut self, br: &mut BitReader<'_>, ch: usize) {
        let n = self.frame_len;
        let c0 = self.read_float29(br);
        let c1 = self.read_float29(br);
        // 0x3001af50..0x3001afb0: one 8-bit quantiser index per band.
        for k in 0..self.quant.len() {
            let index = br.read(8);
            let q = 10f64.powf(f64::from(index) * f64::from(QUANT_EXPONENT));
            self.quant[k] = q as f32;
        }
        let c = &mut self.coeffs[ch];
        c[0] = c0;
        c[1] = c1;

        // 0x3001ab00: run/width coded coefficients 2..N.
        let bands = &self.bands;
        let quant = &self.quant;
        // 0x3001ab07: the running quantiser starts at 0.0 (`0x3004feb0`).
        let mut q = 0.0f32;
        let mut k = 0usize;
        // 0x3001ab13..0x3001ab36: skip bands that end before coefficient 2.
        while k < quant.len() && 2 * bands[k] < 2 {
            q = quant[k];
            k += 1;
        }
        let mut i = 2usize;
        while i < n {
            // 0x3001ab60..0x3001abfd: one flag bit selects an RLE run (`8 * rle[4 bits]`
            // values) or a single group of 8.
            let end = if br.read(1) != 0 {
                let r = br.read(4) as usize;
                i + 8 * usize::from(self.rle[r])
            } else {
                i + 8
            };
            let end = end.min(n);
            // 0x3001ac09: 4-bit magnitude width of the run.
            let width = br.read(4) as usize;
            if width == 0 {
                // 0x3001ac4c..0x3001acb3: a zero run, then catch the band index up.
                c[i..end].fill(0.0);
                i = end;
                while k < quant.len() && i > 2 * bands[k] {
                    q = quant[k];
                    k += 1;
                }
            } else {
                while i < end {
                    // 0x3001ad00..0x3001ad1d: entering a band switches the quantiser. Only
                    // equality is tested, once per coefficient (a DLL property kept as is).
                    if k < quant.len() && i == 2 * bands[k] {
                        q = quant[k];
                        k += 1;
                    }
                    let v = br.read(width);
                    c[i] = if v != 0 {
                        // 0x3001ad65..0x3001adb0: sign bit after a non-zero magnitude.
                        let neg = br.read(1) != 0;
                        let v = if neg { -(v as i32) } else { v as i32 };
                        (f64::from(v) * f64::from(q)) as f32
                    } else {
                        0.0
                    };
                    i += 1;
                }
            }
        }
    }

    /// Reads a packed 29-bit float (`0x3001ae60..0x3001aecd`): bits 0..4 exponent, 5..27
    /// mantissa, 28 sign; value `mantissa * scale[exponent]`.
    fn read_float29(&mut self, br: &mut BitReader<'_>) -> f32 {
        let v = br.read(29);
        let mantissa = (v >> 5) & 0x7f_ffff;
        let e = (v & 0x1f) as usize;
        let scale = match self.exponent_scale.get(e) {
            Some(&s) => s,
            None => {
                self.stats.exponent_out_of_table += 1;
                2f64.powi(e as i32 - 23)
            }
        };
        let x = f64::from(mantissa) * scale;
        let x = if v & 0x1000_0000 != 0 { -x } else { x };
        x as f32
    }
}

/// Rounds like `fistp` under the default round-to-nearest-even mode and saturates like the
/// output loops at `0x3001aa56..0x3001aadb`. Values outside the `i32` range produce the x87
/// "integer indefinite" `0x80000000`, which saturates to `-32768`. Returns `(sample, clipped)`.
fn to_i16(x: f64) -> (i16, bool) {
    let r = x.round_ties_even();
    if r.is_nan() || r >= 2_147_483_648.0 || r < -2_147_483_648.0 {
        return (i16::MIN, true);
    }
    let v = r as i64;
    if v > i64::from(i16::MAX) {
        (i16::MAX, true)
    } else if v < i64::from(i16::MIN) {
        (i16::MIN, true)
    } else {
        (v as i16, false)
    }
}

/// Inverse DCT `x[k] = sum_{j=0}^{N-1} a[j] cos(pi * j * (k + 1/2) / N)` (DCT-III without
/// halving `a[0]`, i.e. Ooura's "IDCT excluding scale" definition that the DLL's
/// `ddct(N, +1, ...)` call at `0x3001affd` computes). Evaluated in `f64` through a
/// zero-padded complex FFT of length `2N`:
/// `x[k] = Re( sum_j (a[j] e^{i pi j / 2N}) e^{i 2 pi j k / 2N} )`.
#[derive(Debug, Clone)]
struct Dct3 {
    n: usize,
    /// `e^{i pi j / 2N}` for `j < N`.
    pre: Vec<(f64, f64)>,
    /// `e^{i 2 pi t / 2N}` for `t < N` (FFT twiddles).
    twiddle: Vec<(f64, f64)>,
    /// Bit-reversal permutation for length `2N`.
    rev: Vec<usize>,
    re: Vec<f64>,
    im: Vec<f64>,
}

impl Dct3 {
    fn new(n: usize) -> Self {
        assert!(
            n.is_power_of_two() && n >= 2,
            "DCT length must be a power of two"
        );
        let m = 2 * n;
        let pre = (0..n)
            .map(|j| {
                let a = std::f64::consts::PI * j as f64 / m as f64;
                (a.cos(), a.sin())
            })
            .collect();
        let twiddle = (0..n)
            .map(|t| {
                let a = 2.0 * std::f64::consts::PI * t as f64 / m as f64;
                (a.cos(), a.sin())
            })
            .collect();
        let bits = m.trailing_zeros();
        let rev = (0..m)
            .map(|i| i.reverse_bits() >> (usize::BITS - bits))
            .collect();
        Dct3 {
            n,
            pre,
            twiddle,
            rev,
            re: vec![0.0; m],
            im: vec![0.0; m],
        }
    }

    /// Transforms `a` in place (input and output stored as `f32`, like the DLL's arrays).
    fn inverse(&mut self, a: &mut [f32]) {
        let n = self.n;
        let m = 2 * n;
        self.re.fill(0.0);
        self.im.fill(0.0);
        for ((&x, &(c, s)), &r) in a.iter().zip(&self.pre).zip(&self.rev).take(n) {
            let v = f64::from(x);
            self.re[r] = v * c;
            self.im[r] = v * s;
        }
        // Iterative radix-2 FFT with positive exponent.
        let mut len = 2;
        while len <= m {
            let half = len / 2;
            let step = m / len;
            for start in (0..m).step_by(len) {
                for t in 0..half {
                    let (wc, ws) = self.twiddle[t * step];
                    let (ar, ai) = (self.re[start + t], self.im[start + t]);
                    let (br, bi) = (self.re[start + t + half], self.im[start + t + half]);
                    let (xr, xi) = (br * wc - bi * ws, br * ws + bi * wc);
                    self.re[start + t] = ar + xr;
                    self.im[start + t] = ai + xi;
                    self.re[start + t + half] = ar - xr;
                    self.im[start + t + half] = ai - xi;
                }
            }
            len *= 2;
        }
        for (k, out) in a.iter_mut().enumerate().take(n) {
            *out = self.re[k] as f32;
        }
    }
}

/// One decoded track plus per-packet bookkeeping (used by the tools and tests).
#[derive(Debug, Clone, Default)]
pub struct DecodedTrack {
    /// Interleaved 16-bit samples.
    pub pcm: Vec<i16>,
    /// Sample rate in Hz.
    pub sample_rate: u32,
    /// Channel count.
    pub channels: u16,
    /// Interleaved samples each block contributes (`frame_len * channels - overlap`).
    pub samples_per_block: usize,
    /// Interleaved samples at the start of every block after the first that are cross-faded.
    pub overlap: usize,
    /// Packets that carried audio for the track.
    pub packets: usize,
    /// Packets that failed to decode.
    pub errors: usize,
    /// First error message, if any.
    pub first_error: Option<String>,
    /// `pcm.len()` after each frame's packet (index = frame), for audio/video alignment.
    pub samples_after_frame: Vec<usize>,
    /// Decoder counters at the end.
    pub stats: AudioDecodeStats,
}

/// Decodes audio track `track` of a whole-file buffer for frames `0..frames` (all frames when
/// `None`). Packet errors are counted; the decoder continues with the next packet (its overlap
/// state is kept), so one bad packet is reported rather than hiding the rest.
pub fn decode_track(
    file: &[u8],
    bik: &crate::container::BikFile,
    track: usize,
    tables: &AudioTables,
    frames: Option<usize>,
) -> Result<DecodedTrack> {
    let info = bik.audio.get(track).ok_or_else(|| {
        VideoError::new(
            VideoErrorKind::BadValue,
            format!("no audio track {track} ({} tracks)", bik.audio.len()),
        )
    })?;
    let mut dec = AudioDecoder::new(tables, info)?;
    let mut out = DecodedTrack {
        sample_rate: dec.sample_rate(),
        channels: dec.channels(),
        samples_per_block: dec.samples_per_block(),
        overlap: dec.frame_len() * usize::from(dec.channels()) / 16,
        ..DecodedTrack::default()
    };
    let n = frames.unwrap_or(usize::MAX).min(bik.frame_count());
    for f in 0..n {
        let result = bik
            .frame_packets(file, f)
            .and_then(|p| match p.audio[track] {
                Some(a) => {
                    let start = a.offset as usize;
                    let payload = &file[start..start + a.size as usize];
                    dec.decode_packet(payload, a.decoded_bytes, &mut out.pcm)
                        .map(|_| true)
                }
                None => Ok(false),
            });
        match result {
            Ok(true) => out.packets += 1,
            Ok(false) => {}
            Err(e) => {
                out.packets += 1;
                out.errors += 1;
                out.first_error
                    .get_or_insert_with(|| format!("frame {f}: {e}"));
            }
        }
        out.samples_after_frame.push(out.pcm.len());
    }
    out.stats = dec.stats().clone();
    Ok(out)
}

/// Signal-to-noise ratio of `test` against `reference` in dB over their common length
/// (`+inf` when identical), and the largest absolute sample difference.
pub fn snr_db(reference: &[i16], test: &[i16]) -> (f64, u32) {
    let n = reference.len().min(test.len());
    let mut sig = 0f64;
    let mut noise = 0f64;
    let mut max_diff = 0u32;
    for i in 0..n {
        let r = f64::from(reference[i]);
        let d = i32::from(reference[i]) - i32::from(test[i]);
        sig += r * r;
        noise += f64::from(d) * f64::from(d);
        max_diff = max_diff.max(d.unsigned_abs());
    }
    let snr = if noise == 0.0 {
        f64::INFINITY
    } else if sig == 0.0 {
        f64::NEG_INFINITY
    } else {
        10.0 * (sig / noise).log10()
    };
    (snr, max_diff)
}

/// Serialises interleaved 16-bit PCM as a canonical RIFF/WAVE file.
pub fn wav_bytes(pcm: &[i16], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = (pcm.len() * 2) as u32;
    let mut v = Vec::with_capacity(44 + pcm.len() * 2);
    v.extend_from_slice(b"RIFF");
    v.extend_from_slice(&(36 + data_len).to_le_bytes());
    v.extend_from_slice(b"WAVEfmt ");
    v.extend_from_slice(&16u32.to_le_bytes());
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&channels.to_le_bytes());
    v.extend_from_slice(&sample_rate.to_le_bytes());
    v.extend_from_slice(&(sample_rate * u32::from(channels) * 2).to_le_bytes());
    v.extend_from_slice(&(channels * 2).to_le_bytes());
    v.extend_from_slice(&16u16.to_le_bytes());
    v.extend_from_slice(b"data");
    v.extend_from_slice(&data_len.to_le_bytes());
    for s in pcm {
        v.extend_from_slice(&s.to_le_bytes());
    }
    v
}

/// Comparison of decoded PCM against an oracle, split by block region.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct OracleComparison {
    /// Samples compared (the common length).
    pub compared: usize,
    /// SNR over all compared samples (dB).
    pub snr_db: f64,
    /// SNR excluding the cross-faded head of every block after the first (dB).
    pub snr_body_db: f64,
    /// Largest absolute difference.
    pub max_diff: u32,
    /// Largest absolute difference outside the cross-faded regions.
    pub max_diff_body: u32,
    /// Samples that differ inside the cross-faded regions.
    pub differing_crossfade: usize,
    /// Samples that differ elsewhere.
    pub differing_body: usize,
}

/// Compares `ours` with `reference` sample by sample. Block boundaries are every
/// `samples_per_block` samples; the first `overlap` samples of every block but the first are
/// the cross-faded region.
pub fn compare_with_oracle(
    reference: &[i16],
    ours: &[i16],
    samples_per_block: usize,
    overlap: usize,
) -> OracleComparison {
    let n = reference.len().min(ours.len());
    let (mut sig, mut noise, mut sig_b, mut noise_b) = (0f64, 0f64, 0f64, 0f64);
    let (mut max_diff, mut max_diff_body) = (0u32, 0u32);
    let (mut diff_x, mut diff_b) = (0usize, 0usize);
    let block = samples_per_block.max(1);
    for i in 0..n {
        let r = f64::from(reference[i]);
        let d = i32::from(reference[i]) - i32::from(ours[i]);
        let e = f64::from(d) * f64::from(d);
        sig += r * r;
        noise += e;
        max_diff = max_diff.max(d.unsigned_abs());
        let crossfade = i >= block && i % block < overlap;
        if crossfade {
            diff_x += usize::from(d != 0);
        } else {
            sig_b += r * r;
            noise_b += e;
            max_diff_body = max_diff_body.max(d.unsigned_abs());
            diff_b += usize::from(d != 0);
        }
    }
    let db = |s: f64, e: f64| {
        if e == 0.0 {
            f64::INFINITY
        } else if s == 0.0 {
            f64::NEG_INFINITY
        } else {
            10.0 * (s / e).log10()
        }
    };
    OracleComparison {
        compared: n,
        snr_db: db(sig, noise),
        snr_body_db: db(sig_b, noise_b),
        max_diff,
        max_diff_body,
        differing_crossfade: diff_x,
        differing_body: diff_b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic tables with the documented structure (not the DLL's values).
    fn tables() -> AudioTables {
        let mut critical_freqs = [0u32; AUDIO_CRITICAL_FREQS];
        for (i, f) in critical_freqs.iter_mut().enumerate() {
            *f = i as u32 * 1000;
        }
        AudioTables {
            rle: [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            critical_freqs,
            exponent_scale: std::array::from_fn(|e| 2f64.powi(e as i32 - 23)),
            rle_offset: 0,
            dll_size: 0,
            dll_fnv1a: 0,
        }
    }

    /// LSB-first bit writer (the inverse of [`BitReader`]).
    #[derive(Default)]
    struct Bits {
        bytes: Vec<u8>,
        bit: usize,
    }

    impl Bits {
        fn put(&mut self, v: u32, n: usize) {
            for k in 0..n {
                if self.bit.is_multiple_of(8) {
                    self.bytes.push(0);
                }
                if (v >> k) & 1 != 0 {
                    let last = self.bytes.len() - 1;
                    self.bytes[last] |= 1 << (self.bit % 8);
                }
                self.bit += 1;
            }
        }
        /// Pads to the next dword (blocks consume whole dwords).
        fn finish_block(&mut self) {
            while !self.bit.is_multiple_of(32) {
                self.put(0, 1);
            }
        }
    }

    /// Encodes `mantissa * 2^(e-23)` as the packed 29-bit float.
    fn float29(b: &mut Bits, mantissa: u32, e: u32, negative: bool) {
        let v = (e & 0x1f) | ((mantissa & 0x7f_ffff) << 5) | (u32::from(negative) << 28);
        b.put(v, 29);
    }

    /// One mono block: DC `c0`, every other coefficient zero (zero-width runs of 8).
    fn dc_block(b: &mut Bits, dec: &AudioDecoder, mantissa: u32, negative: bool) {
        b.put(0, 2); // skipped bits of the DCT variant
        float29(b, mantissa, 23, negative);
        float29(b, 0, 0, false);
        for _ in 0..dec.quant.len() {
            b.put(0, 8);
        }
        let mut i = 2;
        while i < dec.frame_len {
            b.put(0, 1); // no RLE: 8 coefficients
            b.put(0, 4); // width 0
            i += 8;
        }
        b.finish_block();
    }

    #[test]
    fn dct3_matches_direct_definition() {
        for n in [8usize, 64, 512] {
            let mut dct = Dct3::new(n);
            let input: Vec<f32> = (0..n)
                .map(|j| ((j * 7919 % 113) as f32 - 56.0) * 0.37)
                .collect();
            let mut a = input.clone();
            dct.inverse(&mut a);
            let mut worst = 0f64;
            for (k, &got) in a.iter().enumerate() {
                let want: f64 = (0..n)
                    .map(|j| {
                        f64::from(input[j])
                            * (std::f64::consts::PI * j as f64 * (k as f64 + 0.5) / n as f64).cos()
                    })
                    .sum();
                worst = worst.max((f64::from(got) - want).abs() / (1.0 + want.abs()));
            }
            assert!(worst < 1e-5, "n={n}: relative error {worst}");
        }
    }

    #[test]
    fn frame_length_and_bands_follow_the_sample_rate() {
        let t = tables();
        let d = AudioDecoder::with_format(&t, 48000, 2).unwrap();
        assert_eq!(d.frame_len(), 2048);
        // nyquist 24000: freqs 0..=23000 are below it, 24000 is not -> 24 bands + closing edge.
        assert_eq!(d.bands().len(), 25);
        assert_eq!(d.bands()[0], 1, "a zero edge is clamped to 1");
        assert_eq!(*d.bands().last().unwrap(), 1024);
        assert_eq!(d.samples_per_block(), 2048 * 2 - 256);
        assert_eq!(
            AudioDecoder::with_format(&t, 44099, 1).unwrap().frame_len(),
            1024
        );
        assert_eq!(
            AudioDecoder::with_format(&t, 22049, 1).unwrap().frame_len(),
            512
        );
        // 0 Hz and 3 channels are rejected rather than producing garbage.
        assert!(AudioDecoder::with_format(&t, 0, 1).is_err());
        assert!(AudioDecoder::with_format(&t, 44100, 3).is_err());
    }

    #[test]
    fn rdft_tracks_are_reported_unsupported() {
        let track = AudioTrack {
            max_decoded_bytes: 0,
            sample_rate: 44100,
            flags: 0x6000, // stereo, 16-bit, no DCT flag
            id: 0,
        };
        let e = AudioDecoder::new(&tables(), &track).unwrap_err();
        assert_eq!(e.kind(), VideoErrorKind::Unsupported);
        // 0x8000 overrides the DCT flag (BinkOpen 0x30012fe7).
        let track = AudioTrack {
            flags: 0x9000,
            ..track
        };
        assert!(!track.audio_dct());
    }

    #[test]
    fn dc_block_decodes_to_constant_and_consumes_whole_dwords() {
        let t = tables();
        let mut dec = AudioDecoder::with_format(&t, 22050, 1).unwrap();
        let mut b = Bits::default();
        // c0 = 3000; x[k] = c0 for every k, sample = round(c0 * 2/sqrt(1024)).
        dc_block(&mut b, &dec, 3000, false);
        let block_bits = b.bit;
        let mut out = Vec::new();
        let declared = (dec.samples_per_block() * 2) as u32;
        let st = dec.decode_packet(&b.bytes, declared, &mut out).unwrap();
        assert_eq!(st.blocks, 1);
        assert_eq!(st.bytes_used, block_bits / 8);
        assert_eq!(st.bytes_used % 4, 0);
        let want = (3000.0f64 * f64::from((2.0f64 / 32.0) as f32)).round_ties_even() as i16;
        assert_eq!(out.len(), dec.samples_per_block());
        assert!(out.iter().all(|&s| s == want), "first {:?}", &out[..4]);
        assert_eq!(dec.stats().trailing_bytes, 0);
    }

    #[test]
    fn crossfade_floors_like_the_unsigned_division() {
        let t = tables();
        let mut dec = AudioDecoder::with_format(&t, 22050, 1).unwrap();
        let n_ov = dec.overlap.len();
        assert_eq!(n_ov, 1024 / 16);
        // Block 1 settles to -3 everywhere, block 2 to 0: cross-fade sample i is
        // floor(-3 * (n - i) / n), which is -3 at i = 1 (truncation would give -2).
        let scale = f64::from((2.0f64 / 32.0) as f32);
        let m = (3.0 / scale).round() as u32; // c0 so that round(c0 * scale) == 3
        let mut b = Bits::default();
        dc_block(&mut b, &dec, m, true);
        dc_block(&mut b, &dec, 0, false);
        let per = dec.samples_per_block();
        let mut out = Vec::new();
        dec.decode_packet(&b.bytes, (2 * per * 2) as u32, &mut out)
            .unwrap();
        assert_eq!(out.len(), 2 * per);
        assert!(out[..per].iter().all(|&s| s == -3));
        let second = &out[per..];
        for (i, &s) in second.iter().enumerate().take(n_ov) {
            let sum = -3 * (n_ov as i32 - i as i32);
            assert_eq!(i32::from(s), sum.div_euclid(n_ov as i32), "i={i}");
        }
        assert_eq!(second[1], -3);
        assert!(second[n_ov..].iter().all(|&s| s == 0));
    }

    #[test]
    fn first_block_after_reset_is_not_crossfaded() {
        let t = tables();
        let mut dec = AudioDecoder::with_format(&t, 22050, 1).unwrap();
        let mut b = Bits::default();
        dc_block(&mut b, &dec, 1000, false);
        let per = dec.samples_per_block();
        let mut first = Vec::new();
        dec.decode_packet(&b.bytes, (per * 2) as u32, &mut first)
            .unwrap();
        dec.reset();
        let mut again = Vec::new();
        dec.decode_packet(&b.bytes, (per * 2) as u32, &mut again)
            .unwrap();
        assert_eq!(first, again);
    }

    #[test]
    fn declared_size_truncates_the_last_block_and_overrun_is_an_error() {
        let t = tables();
        let mut dec = AudioDecoder::with_format(&t, 22050, 1).unwrap();
        let mut b = Bits::default();
        dc_block(&mut b, &dec, 1000, false);
        let mut out = Vec::new();
        // Only 10 samples declared: one block is decoded, 10 samples emitted.
        let st = dec.decode_packet(&b.bytes, 20, &mut out).unwrap();
        assert_eq!((st.blocks, st.samples, out.len()), (1, 10, 10));
        // Declaring two blocks' worth over a one-block payload must fail, not pad.
        let per = dec.samples_per_block();
        let e = dec
            .decode_packet(&b.bytes, (per * 2 * 2) as u32, &mut out)
            .unwrap_err();
        assert_eq!(e.kind(), VideoErrorKind::OutOfData);
        // A truncated block (payload cut mid-way) also fails.
        let e = dec
            .decode_packet(&b.bytes[..8], (per * 2) as u32, &mut Vec::new())
            .unwrap_err();
        assert_eq!(e.kind(), VideoErrorKind::OutOfData);
        // Zero declared bytes decode nothing (BinkGetTrackData 0x30015c79).
        let st = dec.decode_packet(&b.bytes, 0, &mut out).unwrap();
        assert_eq!(st.blocks, 0);
    }

    #[test]
    fn coefficients_use_rle_runs_signs_and_band_quantisers() {
        let t = tables();
        // 22050 Hz mono: N = 1024, nyquist 11025, edges max(1, f*512/11025).
        let mut dec = AudioDecoder::with_format(&t, 22050, 1).unwrap();
        let bands = dec.bands().to_vec();
        assert_eq!(&bands[..3], &[1, 46, 92]);
        let mut b = Bits::default();
        b.put(0, 2);
        float29(&mut b, 5, 23, false);
        float29(&mut b, 7, 23, true);
        let nq = dec.quant.len();
        for k in 0..nq {
            // quantiser index 0 for band 0 (q = 1), 15 for band 1 (q = 10^(15*0.0664)).
            b.put(if k == 1 { 15 } else { 0 }, 8);
        }
        // RLE run: flag 1, rle[11] = 12 -> 96 coefficients (2..98), width 3.
        b.put(1, 1);
        b.put(11, 4);
        b.put(3, 4);
        for i in 2..98u32 {
            let v = i % 8; // 0 has no sign bit
            b.put(v, 3);
            if v != 0 {
                b.put(i & 1, 1); // odd -> negative
            }
        }
        // The rest: zero-width groups of 8.
        let mut i = 98;
        while i < dec.frame_len {
            b.put(0, 1);
            b.put(0, 4);
            i += 8;
        }
        b.finish_block();
        let mut br = BitReader::new(&b.bytes);
        br.skip(2);
        dec.read_channel(&mut br, 0);
        assert!(!br.overflowed());
        assert_eq!(br.bits_read().div_ceil(32) * 4, b.bytes.len());
        let c = &dec.coeffs[0];
        assert_eq!((c[0], c[1]), (5.0, -7.0));
        let q1 = (10f64.powf(15.0 * f64::from(QUANT_EXPONENT))) as f32;
        for (i, &got) in c.iter().enumerate().take(98).skip(2) {
            let v = (i % 8) as f64;
            let v = if i % 2 == 1 { -v } else { v };
            // Band 0 covers coefficients [2, 92), band 1 starts at 2 * 46 = 92.
            let q = if i < 92 { 1.0 } else { f64::from(q1) };
            assert_eq!(f64::from(got), f64::from((v * q) as f32), "coefficient {i}");
        }
        assert!(c[98..].iter().all(|&v| v == 0.0));
    }

    #[test]
    fn exponents_beyond_the_table_are_counted() {
        let t = tables();
        let mut dec = AudioDecoder::with_format(&t, 22050, 1).unwrap();
        let mut b = Bits::default();
        float29(&mut b, 1, 25, false);
        let mut br = BitReader::new(&b.bytes);
        assert_eq!(dec.read_float29(&mut br), 4.0);
        assert_eq!(dec.stats().exponent_out_of_table, 1);
    }

    #[test]
    fn output_rounding_and_saturation_match_fistp() {
        assert_eq!(to_i16(2.5), (2, false));
        assert_eq!(to_i16(3.5), (4, false));
        assert_eq!(to_i16(-2.5), (-2, false));
        assert_eq!(to_i16(32767.4), (32767, false));
        assert_eq!(to_i16(32767.5), (32767, true));
        assert_eq!(to_i16(-40000.0), (-32768, true));
        // Outside the i32 range x87 stores the integer indefinite, which saturates negative.
        assert_eq!(to_i16(3.0e9), (-32768, true));
        assert_eq!(to_i16(f64::NAN), (-32768, true));
    }

    #[test]
    fn oracle_comparison_separates_crossfades() {
        let reference = vec![100i16; 40];
        let mut ours = reference.clone();
        ours[10] = 99; // block 1 (block = 10), cross-fade (overlap 2)
        ours[15] = 98; // block 1 body
        ours[1] = 90; // block 0 has no cross-fade: counts as body
        let c = compare_with_oracle(&reference, &ours, 10, 2);
        assert_eq!(c.compared, 40);
        assert_eq!((c.differing_crossfade, c.differing_body), (1, 2));
        assert_eq!((c.max_diff, c.max_diff_body), (10, 10));
        let (snr, max) = snr_db(&reference, &reference);
        assert!(snr.is_infinite() && max == 0);
    }

    #[test]
    fn wav_header_is_canonical() {
        let w = wav_bytes(&[1, -1, 2, -2], 48000, 2);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([w[4], w[5], w[6], w[7]]), 36 + 8);
        assert_eq!(u16::from_le_bytes([w[22], w[23]]), 2);
        assert_eq!(u32::from_le_bytes([w[24], w[25], w[26], w[27]]), 48000);
        assert_eq!(w.len(), 44 + 8);
    }
}
