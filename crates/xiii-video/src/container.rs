//! Bink 1 container reader (`BIK`, revision `i`).
//!
//! Layout measured on the 19 XIII cutscenes and cross-checked with the MultimediaWiki
//! *Bink Container* prose description: a 44-byte main header, then a per-track audio header
//! block (12 bytes per track in the XIII corpus), then a little-endian frame-offset table with
//! one entry per frame plus a trailing end entry. Bit 0 of an offset marks a keyframe.
//!
//! The reader is strictly bounded: every read is checked against the buffer and it returns
//! [`VideoError`] instead of panicking on file data.

use crate::error::{Result, VideoError, VideoErrorKind};

/// Revision byte of the Bink 1 streams this crate understands.
pub const REVISION_I: u8 = b'i';

/// Video flags (bytes 36..40).
pub const FLAG_ALPHA: u32 = 0x0010_0000;
/// Grayscale flag.
pub const FLAG_GRAY: u32 = 0x0002_0000;

fn u16_at(d: &[u8], o: usize) -> Result<u16> {
    let s = d
        .get(o..o + 2)
        .ok_or_else(|| VideoError::at(VideoErrorKind::Truncated, o as u64, "u16"))?;
    Ok(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(d: &[u8], o: usize) -> Result<u32> {
    let s = d
        .get(o..o + 4)
        .ok_or_else(|| VideoError::at(VideoErrorKind::Truncated, o as u64, "u32"))?;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

/// Main header of a Bink 1 file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Header {
    /// Codec revision byte (always `b'i'` here).
    pub revision: u8,
    /// `file size not including the first 8 bytes` (bytes 4..8).
    pub file_size_field: u32,
    /// Frame count at offset 8 (normally authoritative).
    pub frames_a: u32,
    /// Largest single frame in bytes.
    pub largest_frame: u32,
    /// Frame count at offset 16 (a duplicate; differs on one XIII file).
    pub frames_b: u32,
    /// Coded video width.
    pub width: u32,
    /// Coded video height.
    pub height: u32,
    /// Frames-per-second numerator (bytes 28..32).
    pub fps_num: u32,
    /// Frames-per-second denominator (bytes 32..36).
    pub fps_den: u32,
    /// Video flags (bytes 36..40).
    pub flags: u32,
    /// Number of audio tracks.
    pub audio_tracks: u32,
}

impl Header {
    /// True when the file carries an alpha plane.
    pub fn has_alpha(&self) -> bool {
        self.flags & FLAG_ALPHA != 0
    }

    /// True when the file is grayscale.
    pub fn has_gray(&self) -> bool {
        self.flags & FLAG_GRAY != 0
    }

    /// Frames per second as a floating point ratio (`0.0` when the denominator is zero).
    pub fn fps(&self) -> f64 {
        if self.fps_den == 0 {
            0.0
        } else {
            f64::from(self.fps_num) / f64::from(self.fps_den)
        }
    }
}

/// One audio track header entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioTrack {
    /// Channel count from the per-track field (`1` or `2`).
    pub channels: u16,
    /// Sample rate in Hz.
    pub sample_rate: u16,
    /// Raw flags word (bit 13 stereo, bit 12 DCT/FFT, bits 14/15 unknown).
    pub flags: u16,
    /// Track id.
    pub id: u32,
}

impl AudioTrack {
    /// True when the stereo flag is set.
    pub fn stereo(&self) -> bool {
        self.flags & 0x2000 != 0
    }
    /// True when the track uses the DCT (rather than FFT) Bink Audio algorithm.
    pub fn audio_dct(&self) -> bool {
        self.flags & 0x1000 != 0
    }
}

/// One frame-offset table entry resolved to an absolute byte range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameEntry {
    /// Absolute file offset of the frame's data.
    pub offset: u64,
    /// Frame byte length (`next.offset - offset`).
    pub size: u64,
    /// Keyframe bit (bit 0 of the table entry).
    pub keyframe: bool,
}

/// One decoded audio packet reference inside a frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioPacketRef {
    /// Offset of the packet payload (after the two u32 header words).
    pub offset: u64,
    /// Packet payload length in bytes.
    pub size: u64,
    /// Declared sample count.
    pub samples: u32,
}

/// A parsed Bink 1 file.
#[derive(Debug, Clone)]
pub struct BikFile {
    /// Main header.
    pub header: Header,
    /// Audio track headers.
    pub audio: Vec<AudioTrack>,
    /// Frame ranges, one per frame.
    pub frames: Vec<FrameEntry>,
    /// File byte offset where the frame data of frame 0 begins (end of the index table).
    pub data_start: u64,
    /// True when the frame count had to be taken from the duplicate header field.
    pub used_duplicate_frame_count: bool,
    /// Total file length in bytes.
    pub len: u64,
}

impl BikFile {
    /// Parses a Bink 1 file from its bytes. The buffer must be the whole file.
    pub fn parse(d: &[u8]) -> Result<Self> {
        if d.len() < 44 {
            return Err(VideoError::at(
                VideoErrorKind::Truncated,
                0,
                format!("file is {} bytes, need at least 44", d.len()),
            ));
        }
        if &d[0..3] != b"BIK" {
            return Err(VideoError::at(
                VideoErrorKind::BadSignature,
                0,
                "not a Bink 1 file (missing 'BIK')",
            ));
        }
        let revision = d[3];
        if revision != REVISION_I {
            return Err(VideoError::at(
                VideoErrorKind::BadSignature,
                3,
                format!("unsupported Bink revision 0x{revision:02x}, expected 0x69 ('i')"),
            ));
        }
        let header = Header {
            revision,
            file_size_field: u32_at(d, 4)?,
            frames_a: u32_at(d, 8)?,
            largest_frame: u32_at(d, 12)?,
            frames_b: u32_at(d, 16)?,
            width: u32_at(d, 20)?,
            height: u32_at(d, 24)?,
            fps_num: u32_at(d, 28)?,
            fps_den: u32_at(d, 32)?,
            flags: u32_at(d, 36)?,
            audio_tracks: u32_at(d, 40)?,
        };
        if header.width == 0 || header.height == 0 || header.width > 32767 || header.height > 32767
        {
            return Err(VideoError::at(
                VideoErrorKind::BadValue,
                20,
                format!("invalid dimensions {}x{}", header.width, header.height),
            ));
        }
        if header.audio_tracks > 256 {
            return Err(VideoError::at(
                VideoErrorKind::BadValue,
                40,
                format!("{} audio tracks (> 256)", header.audio_tracks),
            ));
        }

        // Audio headers: the XIII corpus uses 12 bytes per track. The wiki documents three
        // per-track groups (unknown+channels, sample_rate+flags, id) which sum to 12 bytes.
        let n_audio = header.audio_tracks as usize;
        let audio_bytes = n_audio
            .checked_mul(12)
            .ok_or_else(|| VideoError::at(VideoErrorKind::BadValue, 44, "audio header overflow"))?;
        let table_base = 44usize
            .checked_add(audio_bytes)
            .ok_or_else(|| VideoError::at(VideoErrorKind::BadValue, 44, "table offset overflow"))?;

        let mut audio = Vec::with_capacity(n_audio);
        for t in 0..n_audio {
            let o = 44 + t * 12;
            audio.push(AudioTrack {
                channels: u16_at(d, o + 2)?,
                sample_rate: u16_at(d, o + 4)?,
                flags: u16_at(d, o + 6)?,
                id: u32_at(d, o + 8)?,
            });
        }

        // Resolve the frame count. The offset-8 field is authoritative (the wiki's "number of
        // frames"); the duplicate at offset 16 is only tried if that index is structurally
        // invalid. Cine14's table is zero-padded past the real count, so entries are validated
        // (monotone, in range) rather than compared against the first offset.
        let (frames, used_duplicate) = if index_valid(d, table_base, header.frames_a) {
            (header.frames_a, false)
        } else if index_valid(d, table_base, header.frames_b) {
            (header.frames_b, true)
        } else {
            return Err(VideoError::at(
                VideoErrorKind::BadValue,
                table_base as u64,
                format!(
                    "frame index is invalid for both frame counts ({} and {})",
                    header.frames_a, header.frames_b
                ),
            ));
        };

        let table_len = (frames as usize + 1).checked_mul(4).ok_or_else(|| {
            VideoError::at(
                VideoErrorKind::BadValue,
                table_base as u64,
                "index overflow",
            )
        })?;
        let table_end = table_base.checked_add(table_len).ok_or_else(|| {
            VideoError::at(
                VideoErrorKind::BadValue,
                table_base as u64,
                "index overflow",
            )
        })?;
        if table_end > d.len() {
            return Err(VideoError::at(
                VideoErrorKind::Truncated,
                table_base as u64,
                format!("frame index needs {table_end} bytes, file is {}", d.len()),
            ));
        }

        let mut raw = Vec::with_capacity(frames as usize + 1);
        for i in 0..=frames as usize {
            raw.push(u32_at(d, table_base + i * 4)?);
        }

        let end_of_file = d.len() as u64;
        let mut entries: Vec<FrameEntry> = Vec::with_capacity(frames as usize);
        for i in 0..frames as usize {
            let a = u64::from(raw[i] & !1);
            let b = if i + 1 < raw.len() {
                let next = u64::from(raw[i + 1] & !1);
                // The trailing entry is the end of the last frame; clamp to file length.
                if next < a {
                    end_of_file
                } else {
                    next.min(end_of_file)
                }
            } else {
                end_of_file
            };
            if a < table_end as u64 || a > end_of_file || b > end_of_file || b < a {
                return Err(VideoError::at(
                    VideoErrorKind::BadValue,
                    table_base as u64 + i as u64 * 4,
                    format!("frame {i} range {a}..{b} is outside the file"),
                ));
            }
            entries.push(FrameEntry {
                offset: a,
                size: b - a,
                keyframe: raw[i] & 1 != 0,
            });
        }

        Ok(BikFile {
            header,
            audio,
            frames: entries,
            data_start: table_end as u64,
            used_duplicate_frame_count: used_duplicate,
            len: end_of_file,
        })
    }

    /// Number of frames in the index.
    pub fn frame_count(&self) -> usize {
        self.frames.len()
    }

    /// Splits one frame's payload into per-track audio packets and the trailing video packet.
    ///
    /// Audio packets are located but not decoded. Bounds are checked against `frame`.
    pub fn frame_packets(&self, data: &[u8], frame: usize) -> Result<FramePackets> {
        let f = *self.frames.get(frame).ok_or_else(|| {
            VideoError::new(VideoErrorKind::BadValue, format!("no frame {frame}"))
        })?;
        let start = usize::try_from(f.offset)
            .map_err(|_| VideoError::at(VideoErrorKind::BadValue, f.offset, "offset too large"))?;
        let end = usize::try_from(f.offset + f.size)
            .map_err(|_| VideoError::at(VideoErrorKind::BadValue, f.offset, "offset too large"))?;
        if end > data.len() {
            return Err(VideoError::at(
                VideoErrorKind::Truncated,
                f.offset,
                format!("frame {frame} ends at {end}, past buffer {}", data.len()),
            ));
        }
        let mut o = start;
        let mut packets = Vec::with_capacity(self.audio.len());
        for (t, _track) in self.audio.iter().enumerate() {
            if o + 8 > end {
                return Err(VideoError::at(
                    VideoErrorKind::Truncated,
                    o as u64,
                    format!("frame {frame}: audio header {t} runs past the frame"),
                ));
            }
            let len_plus4 = u32_at(data, o)?;
            let samples = u32_at(data, o + 4)?;
            o += 8;
            if len_plus4 == 0 {
                packets.push(None);
                continue;
            }
            // The stored length counts the four-byte sample-count word as well.
            let payload = u64::from(len_plus4).saturating_sub(4);
            let payload = payload.min((end - o) as u64);
            packets.push(Some(AudioPacketRef {
                offset: o as u64,
                size: payload,
                samples,
            }));
            o = o.saturating_add(payload as usize);
        }
        if o > end {
            return Err(VideoError::at(
                VideoErrorKind::BadValue,
                o as u64,
                format!("frame {frame}: audio packets overflow the frame"),
            ));
        }
        Ok(FramePackets {
            audio: packets,
            video_offset: o as u64,
            video_size: (end - o) as u64,
        })
    }
}

/// Checks that an `n`-frame index fits and its offsets are monotone and in range. Trailing
/// entries may be zero-padded past the real frame count, so only `0..=n` is inspected.
fn index_valid(d: &[u8], table_base: usize, n: u32) -> bool {
    if n == 0 {
        return false;
    }
    let Some(end) = (n as usize + 1)
        .checked_mul(4)
        .and_then(|bytes| table_base.checked_add(bytes))
    else {
        return false;
    };
    if end > d.len() {
        return false;
    }
    let mut prev = table_base as u64;
    for i in 0..=n as usize {
        let off = u64::from(u32_at(d, table_base + i * 4).unwrap_or(0) & !1);
        if off < table_base as u64 || off > d.len() as u64 || off < prev {
            return false;
        }
        prev = off;
    }
    true
}

/// Per-frame split: audio packet references plus the video payload range.
#[derive(Debug, Clone)]
pub struct FramePackets {
    /// One optional packet reference per audio track, in track order.
    pub audio: Vec<Option<AudioPacketRef>>,
    /// Byte offset of the video payload.
    pub video_offset: u64,
    /// Byte length of the video payload.
    pub video_size: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synth(frames: u32, audio_tracks: u32, frames_b: u32) -> Vec<u8> {
        // Header + audio + index + one byte per frame.
        let table_base = 44 + audio_tracks as usize * 12;
        let data_start = table_base + (frames as usize + 1) * 4;
        let mut d = vec![0u8; 44];
        d[0..3].copy_from_slice(b"BIK");
        d[3] = b'i';
        d[4..8].copy_from_slice(&0u32.to_le_bytes());
        d[8..12].copy_from_slice(&frames.to_le_bytes());
        d[12..16].copy_from_slice(&0u32.to_le_bytes());
        d[16..20].copy_from_slice(&frames_b.to_le_bytes());
        d[20..24].copy_from_slice(&640u32.to_le_bytes());
        d[24..28].copy_from_slice(&480u32.to_le_bytes());
        d[28..32].copy_from_slice(&30u32.to_le_bytes());
        d[32..36].copy_from_slice(&1u32.to_le_bytes());
        d[36..40].copy_from_slice(&0u32.to_le_bytes());
        d[40..44].copy_from_slice(&audio_tracks.to_le_bytes());
        for _ in 0..audio_tracks {
            d.extend_from_slice(&[0u8; 12]);
        }
        assert_eq!(d.len(), table_base);
        d.resize(data_start, 0);
        // Index entries: frame k at data_start + 2*k (even, so bit 0 is the keyframe flag).
        for i in 0..=frames as usize {
            let off = data_start as u32 + i as u32 * 2;
            let v = if i == 0 { off | 1 } else { off };
            d[table_base + i * 4..table_base + i * 4 + 4].copy_from_slice(&v.to_le_bytes());
        }
        d.resize(data_start + frames as usize * 2, 0);
        d
    }

    #[test]
    fn parses_synthetic_container() {
        let d = synth(5, 1, 5);
        let f = BikFile::parse(&d).unwrap();
        assert_eq!(f.frame_count(), 5);
        assert!(f.frames[0].keyframe);
        assert!(!f.frames[1].keyframe);
        assert_eq!(f.audio.len(), 1);
        assert_eq!(f.header.width, 640);
        assert!((f.header.fps() - 30.0).abs() < 1e-9);
    }

    #[test]
    fn uses_offset8_frame_count() {
        // Offset 8 is authoritative even when the duplicate differs.
        let d = synth(5, 0, 5);
        let d2 = {
            let mut v = d.clone();
            v[16..20].copy_from_slice(&9u32.to_le_bytes());
            v
        };
        let f = BikFile::parse(&d2).unwrap();
        assert_eq!(f.frame_count(), 5);
        assert!(!f.used_duplicate_frame_count);
    }

    #[test]
    fn falls_back_to_duplicate_when_primary_index_is_invalid() {
        // Break the end entry of the 5-frame index, then rely on the duplicate count of 2.
        let mut d = synth(5, 0, 5);
        d[8..12].copy_from_slice(&5u32.to_le_bytes());
        d[16..20].copy_from_slice(&2u32.to_le_bytes());
        let table_base = 44usize;
        let end_entry = table_base + 5 * 4;
        d[end_entry..end_entry + 4].copy_from_slice(&0u32.to_le_bytes());
        let f = BikFile::parse(&d).unwrap();
        assert_eq!(f.frame_count(), 2);
        assert!(f.used_duplicate_frame_count);
    }

    #[test]
    fn rejects_bad_signature_and_revision() {
        let mut d = synth(3, 0, 3);
        d[0] = b'X';
        assert_eq!(
            BikFile::parse(&d).unwrap_err().kind(),
            VideoErrorKind::BadSignature
        );
        let mut d = synth(3, 0, 3);
        d[3] = b'f';
        assert_eq!(
            BikFile::parse(&d).unwrap_err().kind(),
            VideoErrorKind::BadSignature
        );
    }

    #[test]
    fn rejects_truncation_and_bad_offsets() {
        assert_eq!(
            BikFile::parse(&[0u8; 10]).unwrap_err().kind(),
            VideoErrorKind::Truncated
        );
        let mut d = synth(3, 0, 3);
        // Corrupt an index entry to point before the file start.
        d[44..48].copy_from_slice(&0x0000_0001u32.to_le_bytes());
        assert_eq!(
            BikFile::parse(&d).unwrap_err().kind(),
            VideoErrorKind::BadValue
        );
    }
}
