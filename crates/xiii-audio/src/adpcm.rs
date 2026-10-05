//! Ubisoft ADPCM decoder (6-bit mode) for streamed `.hsc` data.
//!
//! The algorithm is derived from vgmstream `src/coding/ubi_adpcm_decoder.c`
//! @ `7dc938fa2f210943b37c7b6511852b516ef432ab` (reference only; no code copied). The PC XIII
//! corpus is entirely 6-bit, so this module implements the 6-bit path and rejects 4-bit with an
//! explicit [`AudioErrorKind::UnsupportedCodec`]; 4-bit is used by other Ubisoft titles.
//!
//! Each stream starts with a 0x30-byte header, then fixed frames. A frame holds a 0x34-byte state
//! block per channel followed by two subframes of packed codes (1536 codes, or fewer in the last
//! subframe). 32-bit little-endian words are unpacked most-significant-bit first into 6-bit codes.

use crate::PcmAudio;
use crate::error::{AudioError, AudioErrorKind, Result};

const HEADER_SIZE: usize = 0x30;
const CHANNEL_STATE_SIZE: usize = 0x34;
const CODES_PER_SUBFRAME_MAX: usize = 1536;

const TABLE6_1: [i32; 64] = [
    -100000000, -369, -245, -133, -33, 56, 135, 207, 275, 338, 395, 448, 499, 548, 593, 635, 676,
    717, 755, 791, 825, 858, 889, 919, 948, 975, 1003, 1029, 1054, 1078, 1103, 1132,
    // Unused by the 6-bit mode (spilled from the next table in the reference).
    1800, 1800, 1800, 2048, 3072, 4096, 5000, 5056, 5184, 5240, 6144, 6880, 9624, 12880, 14952,
    18040, 20480, 22920, 25600, 28040, 32560, 35840, 40960, 45832, 51200, 56320, 63488, 67704,
    75776, 89088, 102400, 0,
];

const TABLE6_2: [i32; 64] = [
    1800, 1800, 1800, 2048, 3072, 4096, 5000, 5056, 5184, 5240, 6144, 6880, 9624, 12880, 14952,
    18040, 20480, 22920, 25600, 28040, 32560, 35840, 40960, 45832, 51200, 56320, 63488, 67704,
    75776, 89088, 102400, 0, // Unused.
    0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 5, 6, 6, 6, 7,
];

const DELTA_TABLE: [i32; 66] = [
    1024, 1031, 1053, 1076, 1099, 1123, 1148, 1172, 1198, 1224, 1251, 1278, 1306, 1334, 1363, 1393,
    1423, 1454, 1485, 1518, 1551, 1584, 1619, 1654, 1690, 1726, 1764, 1802, 1841, 1881, 1922, 1964,
    2007, -1024, -1031, -1053, -1076, -1099, -1123, -1148, -1172, -1198, -1224, -1251, -1278,
    -1306, -1334, -1363, -1393, -1423, -1454, -1485, -1518, -1551, -1584, -1619, -1654, -1690,
    -1726, -1764, -1802, -1841, -1881, -1922, -1964, -2007,
];

/// Parsed 0x30-byte UBI ADPCM stream header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AdpcmHeader {
    /// Total number of packed codes across all channels (the stream's `sample_count` field).
    pub sample_count: u32,
    /// Number of subframes (two per full frame except possibly the last).
    pub subframe_count: u32,
    /// Codes in the final subframe.
    pub codes_per_subframe_last: u32,
    /// Codes in a regular subframe.
    pub codes_per_subframe: u32,
    /// Subframes per frame (always 2 in this format).
    pub subframes_per_frame: u32,
    /// Bit width of one code (4 or 6).
    pub bits_per_sample: u32,
    /// Channel count (1 or 2).
    pub channels: u32,
}

impl AdpcmHeader {
    /// Per-channel decoded sample count (`sample_count / channels`).
    pub fn per_channel_samples(&self) -> u32 {
        self.sample_count / self.channels.max(1)
    }
}

/// Running state for the 6-bit decoder. The 0x34-byte frame block also carries coefficients and
/// history used only by the (unimplemented here) 4-bit mode; those fields are skipped.
#[derive(Debug, Clone, Copy, Default)]
struct ChannelState {
    step1: i32,
    hist1: i32,
    delta1: i32,
}

fn u32_at(data: &[u8], off: usize) -> Option<u32> {
    data.get(off..off + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn i16_at(data: &[u8], off: usize) -> i32 {
    data.get(off..off + 2)
        .map(|b| i16::from_le_bytes([b[0], b[1]]) as i32)
        .unwrap_or(0)
}

fn i32_at(data: &[u8], off: usize) -> i32 {
    u32_at(data, off).map_or(0, |v| v as i32)
}

/// Parses and validates a UBI ADPCM stream header.
pub fn parse_header(data: &[u8]) -> Result<AdpcmHeader> {
    if data.len() < HEADER_SIZE {
        return Err(AudioError::at(
            AudioErrorKind::Truncated,
            0,
            format!("ADPCM stream shorter than {} bytes", HEADER_SIZE),
        ));
    }
    let signature = u32_at(data, 0).unwrap_or(0);
    if signature != 0x08 {
        return Err(AudioError::at(
            AudioErrorKind::BadAdpcmHeader,
            0,
            format!("signature {signature:#x} != 0x8"),
        ));
    }
    let codes_per_subframe_last = u32_at(data, 0x0c).unwrap_or(0);
    let codes_per_subframe = u32_at(data, 0x10).unwrap_or(0);
    let subframes_per_frame = u32_at(data, 0x14).unwrap_or(0);
    let bits_per_sample = u32_at(data, 0x24).unwrap_or(0);
    let channels = u32_at(data, 0x2c).unwrap_or(0);

    if codes_per_subframe_last as usize > CODES_PER_SUBFRAME_MAX
        || codes_per_subframe as usize > CODES_PER_SUBFRAME_MAX
        || (codes_per_subframe_last == 0 && codes_per_subframe == 0)
    {
        return Err(AudioError::at(
            AudioErrorKind::BadAdpcmHeader,
            0x0c,
            format!(
                "codes per subframe last={codes_per_subframe_last} regular={codes_per_subframe}"
            ),
        ));
    }
    if subframes_per_frame != 2 {
        return Err(AudioError::at(
            AudioErrorKind::BadAdpcmHeader,
            0x14,
            format!("subframes per frame {subframes_per_frame} != 2"),
        ));
    }
    if bits_per_sample != 4 && bits_per_sample != 6 {
        return Err(AudioError::at(
            AudioErrorKind::BadAdpcmHeader,
            0x24,
            format!("bits per sample {bits_per_sample}"),
        ));
    }
    if !(1..=2).contains(&channels) {
        return Err(AudioError::at(
            AudioErrorKind::BadAdpcmHeader,
            0x2c,
            format!("channels {channels}"),
        ));
    }

    Ok(AdpcmHeader {
        sample_count: u32_at(data, 0x04).unwrap_or(0),
        subframe_count: u32_at(data, 0x08).unwrap_or(0),
        codes_per_subframe_last,
        codes_per_subframe,
        subframes_per_frame,
        bits_per_sample,
        channels,
    })
}

fn read_channel_state(data: &[u8], off: usize) -> ChannelState {
    ChannelState {
        step1: i32_at(data, off + 0x04),
        hist1: i16_at(data, off + 0x20),
        delta1: i16_at(data, off + 0x28),
    }
}

fn clamp16(v: i32) -> i32 {
    v.clamp(-32768, 32767)
}

/// Expands one 6-bit code against a channel's running state.
///
/// Reference `expand_code_6bit` (vgmstream @ 7dc938fa, `ubi_adpcm_decoder.c`). All arithmetic
/// wraps at 32 bits like the reference C; the truncated `i16` stores match the reference's
/// `int16_t` fields. The derivation of the test vectors is documented in `adpcm_tests`.
fn expand_code_6bit(code: u8, st: &mut ChannelState) -> i16 {
    let code_signed = code as i32 - 31;
    let step0_index = code_signed.unsigned_abs() as usize;
    let step0_next = TABLE6_1[step0_index].wrapping_add(st.step1);

    let mut step0 = (st.step1 & 0xFFFF).wrapping_mul(246);
    step0 = step0.wrapping_add(TABLE6_2[step0_index]) >> 8;
    step0 = step0.clamp(271, 2560);

    let mut delta0 = 0i32;
    let masked = step0_next & !0xFF;
    if masked.wrapping_sub(1) & i32::MIN == 0 {
        let delta0_index =
            (((step0_next >> 3) & 0x1F) + if code_signed < 0 { 33 } else { 0 }) as usize;
        let delta0_shift = ((step0_next >> 8) & 0xFF).clamp(0, 31) as u32;
        delta0 = DELTA_TABLE[delta0_index].wrapping_shl(delta0_shift) >> 10;
    }

    let sample_new = delta0.wrapping_add(st.delta1).wrapping_add(st.hist1) as i16;

    st.hist1 = sample_new as i32;
    st.step1 = step0;
    st.delta1 = delta0 as i16 as i32;
    sample_new
}

/// Reads a 32-bit LE word at `off`, treating out-of-range reads as zero.
fn word_or_zero(data: &[u8], off: usize) -> u32 {
    u32_at(data, off).unwrap_or(0)
}

/// Unpacks `count` codes of `bits` bits each, most-significant-bit first within LE words.
fn unpack_codes(data: &[u8], count: usize, bits: u32, out: &mut Vec<u8>) {
    out.clear();
    out.reserve(count);
    let mut input: u64 = 0;
    let mut available: u32 = 0;
    let mut pos = 0usize;
    let mask: u64 = if bits == 6 { 0x3f } else { 0x0f };
    for _ in 0..count {
        if available < bits {
            input = (input << 32) | word_or_zero(data, pos) as u64;
            pos += 4;
            available += 32;
        }
        available -= bits;
        out.push(((input >> available) & mask) as u8);
    }
}

fn clamp_i16(v: i32) -> i16 {
    clamp16(v) as i16
}

#[allow(clippy::too_many_arguments)]
fn decode_subframe_mono(
    st: &mut [ChannelState; 2],
    codes: &[u8],
    out: &mut Vec<i16>,
    count: usize,
) {
    for &code in codes.iter().take(count) {
        out.push(expand_code_6bit(code, &mut st[0]));
    }
}

fn decode_subframe_stereo(
    st: &mut [ChannelState; 2],
    codes: &[u8],
    out: &mut Vec<i16>,
    count: usize,
) {
    let groups = count.div_ceil(8);
    for g in 0..groups {
        let base = g * 8;
        let code = |i: usize| -> u8 { codes.get(base + i).copied().unwrap_or(31) };
        let left = [
            expand_code_6bit(code(0), &mut st[0]),
            expand_code_6bit(code(2), &mut st[0]),
            expand_code_6bit(code(4), &mut st[0]),
            expand_code_6bit(code(6), &mut st[0]),
        ];
        let right = [
            expand_code_6bit(code(1), &mut st[1]),
            expand_code_6bit(code(3), &mut st[1]),
            expand_code_6bit(code(5), &mut st[1]),
            expand_code_6bit(code(7), &mut st[1]),
        ];
        let decoded = [
            clamp_i16(left[0] as i32 + right[0] as i32),
            clamp_i16(left[0] as i32 - right[0] as i32),
            clamp_i16(left[1] as i32 + right[1] as i32),
            clamp_i16(left[1] as i32 - right[1] as i32),
            clamp_i16(left[2] as i32 + right[2] as i32),
            clamp_i16(left[2] as i32 - right[2] as i32),
            clamp_i16(left[3] as i32 + right[3] as i32),
            clamp_i16(left[3] as i32 - right[3] as i32),
        ];
        let take = count.saturating_sub(base).min(8);
        out.extend_from_slice(&decoded[..take]);
    }
}

/// Incremental UBI ADPCM decoder: decodes one frame (two subframes) at a time, so a caller can
/// stream a long `.hsc` entry in bounded chunks instead of decoding the whole payload at once.
///
/// The format re-reads the per-frame channel state block at the start of every frame, so frame
/// boundaries are independent decoding units; splitting the stream at a frame boundary cannot
/// change the output. This is what makes chunked decoding bit-exact with [`decode`]
/// (see the `streamed_chunks_equal_full_decode` test).
pub struct AdpcmStream {
    header: AdpcmHeader,
    pos: usize,
    subframe_number: u32,
    st: [ChannelState; 2],
    channels: u16,
    sample_rate: u32,
    produced: usize,
}

impl AdpcmStream {
    /// Builds an incremental decoder for one UBI ADPCM stream. `channels`/`sample_rate` come from
    /// the bank's RIFF header and are validated against the stream header. The stream header is
    /// parsed here; sample bytes are supplied to [`AdpcmStream::next_samples`] so the decoder owns
    /// no copy. `pos` starts immediately after the 0x30-byte header at `data`'s start.
    pub fn new(data: &[u8], channels: u16, sample_rate: u32) -> Result<Self> {
        let header = parse_header(data)?;
        if header.bits_per_sample != 6 {
            return Err(AudioError::at(
                AudioErrorKind::UnsupportedCodec,
                0x24,
                format!(
                    "UBI ADPCM {}-bit not implemented (PC corpus is 6-bit)",
                    header.bits_per_sample
                ),
            ));
        }
        if header.channels as u16 != channels {
            return Err(AudioError::at(
                AudioErrorKind::BadAdpcmHeader,
                0x2c,
                format!("stream channels {} != bank {}", header.channels, channels),
            ));
        }
        Ok(Self {
            header,
            pos: HEADER_SIZE,
            subframe_number: 0,
            st: [ChannelState::default(); 2],
            channels,
            sample_rate,
            produced: 0,
        })
    }

    /// Parsed stream header (counts, codec bit depth, channel count).
    pub fn header(&self) -> AdpcmHeader {
        self.header
    }

    /// Channel count from the bank RIFF header (validated against the stream header).
    pub fn channels(&self) -> u16 {
        self.channels
    }

    /// Sample rate from the bank RIFF header.
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Interleaved sample count declared by the stream.
    pub fn total_samples(&self) -> usize {
        self.header.sample_count as usize
    }

    /// Interleaved samples decoded so far.
    pub fn produced(&self) -> usize {
        self.produced
    }

    /// Restarts decoding from the first frame (used for looping playback).
    pub fn rewind(&mut self) {
        self.pos = HEADER_SIZE;
        self.subframe_number = 0;
        self.st = [ChannelState::default(); 2];
        self.produced = 0;
    }

    /// Decodes the next frame (two subframes) from `data` into `out`. Returns `false` when the
    /// stream is exhausted. The frame's own state block is read fresh, so this never depends on a
    /// previous call's channel state.
    fn decode_frame(&mut self, data: &[u8], out: &mut Vec<i16>) -> Result<bool> {
        if self.subframe_number >= self.header.subframe_count {
            return Ok(false);
        }
        let (code_count_a, code_count_b) = if self.subframe_number + 1 == self.header.subframe_count
        {
            (self.header.codes_per_subframe_last as usize, 0)
        } else if self.subframe_number + 2 == self.header.subframe_count {
            (
                self.header.codes_per_subframe as usize,
                self.header.codes_per_subframe_last as usize,
            )
        } else {
            (
                self.header.codes_per_subframe as usize,
                self.header.codes_per_subframe as usize,
            )
        };

        let setup = CHANNEL_STATE_SIZE * self.header.channels as usize;
        if self.pos + setup > data.len() {
            return Err(AudioError::at(
                AudioErrorKind::Truncated,
                self.pos,
                format!(
                    "frame setup past end ({} of {})",
                    self.pos + setup,
                    data.len()
                ),
            ));
        }
        for (c, state) in self
            .st
            .iter_mut()
            .enumerate()
            .take(self.header.channels as usize)
        {
            *state = read_channel_state(data, self.pos + c * CHANNEL_STATE_SIZE);
        }
        let mut p = self.pos + setup;
        let bits = self.header.bits_per_sample;
        let mut codes = Vec::with_capacity(CODES_PER_SUBFRAME_MAX);

        let size_a = (bits as usize * code_count_a / 8) + usize::from(code_count_a > 0);
        unpack_codes(&data[p.min(data.len())..], code_count_a, bits, &mut codes);
        if self.header.channels == 1 {
            decode_subframe_mono(&mut self.st, &codes, out, code_count_a);
        } else {
            decode_subframe_stereo(&mut self.st, &codes, out, code_count_a);
        }
        p += size_a;

        let size_b = (bits as usize * code_count_b / 8) + usize::from(code_count_b > 0);
        unpack_codes(&data[p.min(data.len())..], code_count_b, bits, &mut codes);
        if self.header.channels == 1 {
            decode_subframe_mono(&mut self.st, &codes, out, code_count_b);
        } else {
            decode_subframe_stereo(&mut self.st, &codes, out, code_count_b);
        }
        p += size_b;

        self.pos = p;
        self.subframe_number += 2;
        self.produced = out.len();
        Ok(true)
    }

    /// Decodes up to `max_samples` interleaved samples from `data` (the whole stream file,
    /// including the 0x30 header parsed by [`AdpcmStream::new`]). `max_samples` is rounded down to
    /// a whole frame. Returns an empty vector at end of stream.
    pub fn next_samples(&mut self, data: &[u8], max_samples: usize) -> Result<Vec<i16>> {
        let mut out: Vec<i16> = Vec::new();
        let ch = self.channels.max(1) as usize;
        let target = max_samples / ch * ch;
        while out.len() < target && self.subframe_number < self.header.subframe_count {
            if !self.decode_frame(data, &mut out)? {
                break;
            }
        }
        Ok(out)
    }
}

/// Decodes a complete UBI ADPCM stream (0x30 header included) to interleaved PCM16.
///
/// `channels` and `sample_rate` come from the bank's RIFF header and are validated against the
/// stream header. The decoded length is required to equal the declared `sample_count`. This is
/// the whole-file convenience wrapper over [`AdpcmStream`].
pub fn decode(data: &[u8], channels: u16, sample_rate: u32) -> Result<PcmAudio> {
    let declared = parse_header(data)?.sample_count as usize;
    let mut stream = AdpcmStream::new(data, channels, sample_rate)?;
    let mut out: Vec<i16> = Vec::with_capacity(declared);
    loop {
        let chunk = stream.next_samples(data, CODES_PER_SUBFRAME_MAX * channels.max(1) as usize)?;
        if chunk.is_empty() {
            break;
        }
        out.extend_from_slice(&chunk);
    }
    if out.len() != declared {
        return Err(AudioError::new(
            AudioErrorKind::SampleCountMismatch,
            format!("decoded {} samples, declared {}", out.len(), declared),
        ));
    }
    Ok(PcmAudio {
        channels,
        sample_rate,
        samples: out,
    })
}

#[cfg(test)]
mod adpcm_tests;
