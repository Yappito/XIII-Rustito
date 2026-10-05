//! Minimal RIFF/WAVE writer for decoded PCM16 audio.
//!
//! Output is a canonical 44-byte `RIFF`/`WAVE`/`fmt `/`data` file with little-endian 16-bit
//! samples. This is the format the rest of the workspace (and Bevy's `wav` feature) reads.

use crate::PcmAudio;

/// Serializes PCM16 audio into a complete `.wav` byte stream.
pub fn write_wav(audio: &PcmAudio) -> Vec<u8> {
    let channels = audio.channels.max(1);
    let data_bytes = audio.samples.len() * 2;
    let byte_rate = audio.sample_rate * channels as u32 * 2;
    let block_align = channels * 2;

    let mut out = Vec::with_capacity(44 + data_bytes);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_bytes as u32).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&audio.sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_bytes as u32).to_le_bytes());
    for sample in &audio.samples {
        out.extend_from_slice(&sample.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_canonical_riff_header_and_data() {
        let audio = PcmAudio {
            channels: 1,
            sample_rate: 8000,
            samples: vec![1, -1],
        };
        let wav = write_wav(&audio);
        assert_eq!(wav.len(), 44 + 4);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes([wav[4], wav[5], wav[6], wav[7]]), 40);
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1);
        assert_eq!(u16::from_le_bytes([wav[22], wav[23]]), 1);
        assert_eq!(
            u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]),
            8000
        );
        assert_eq!(
            u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]),
            16000
        );
        assert_eq!(u16::from_le_bytes([wav[32], wav[33]]), 2);
        assert_eq!(u16::from_le_bytes([wav[34], wav[35]]), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]), 4);
        assert_eq!(i16::from_le_bytes([wav[44], wav[45]]), 1);
        assert_eq!(i16::from_le_bytes([wav[46], wav[47]]), -1);
    }

    #[test]
    fn writes_stereo_byte_rate() {
        let audio = PcmAudio {
            channels: 2,
            sample_rate: 44100,
            samples: vec![0; 8],
        };
        let wav = write_wav(&audio);
        assert_eq!(
            u32::from_le_bytes([wav[28], wav[29], wav[30], wav[31]]),
            44100 * 2 * 2
        );
        assert_eq!(u16::from_le_bytes([wav[32], wav[33]]), 4);
        assert_eq!(&wav[36..40], b"data");
    }
}
