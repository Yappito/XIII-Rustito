//! Tests for chunked decoding: the key invariant is that decoding a streamed entry in chunks of
//! any frame size yields exactly the same samples as one whole-file decode, because UBI ADPCM
//! frame state is re-read per frame.

use std::sync::Arc;

use super::*;
use crate::{decode_entry, hx};

/// Builds a synthetic 6-bit UBI ADPCM stream with a valid header and deterministic payload bytes.
/// The values are not a semantically real recording; the invariant under test (chunked decode ==
/// full decode) holds for any payload because the decoder re-reads each frame's state block.
fn synth_adpcm(channels: u32, codes_per_subframe: u32, subframes: u32) -> Vec<u8> {
    let total_codes = (subframes * codes_per_subframe) as usize;
    let mut out = vec![0u8; 0x30];
    out[0..4].copy_from_slice(&0x08u32.to_le_bytes());
    out[4..8].copy_from_slice(&(total_codes as u32).to_le_bytes());
    out[8..12].copy_from_slice(&subframes.to_le_bytes());
    out[12..16].copy_from_slice(&codes_per_subframe.to_le_bytes());
    out[16..20].copy_from_slice(&codes_per_subframe.to_le_bytes());
    out[20..24].copy_from_slice(&2u32.to_le_bytes()); // subframes_per_frame
    out[0x24..0x28].copy_from_slice(&6u32.to_le_bytes()); // bits
    out[0x2c..0x30].copy_from_slice(&channels.to_le_bytes());
    // Frames: per-channel 0x34 state block, then two subframes of packed codes.
    let size_a = codes_per_subframe as usize * 6 / 8 + 1;
    for frame in 0..(subframes / 2) {
        for ch in 0..channels {
            let mut state = [0u8; 0x34];
            state[0x04..0x08].copy_from_slice(&1024u32.to_le_bytes());
            // A per-(frame,channel) nonzero history so frame boundaries are visible in output.
            state[0x20..0x22]
                .copy_from_slice(&(((frame * 97 + ch * 13) as i32 % 4000) as i16).to_le_bytes());
            state[0x28..0x2a].copy_from_slice(&(-500i16).to_le_bytes());
            out.extend_from_slice(&state);
        }
        let base = out.len() as u8;
        for sf in 0..2u8 {
            for i in 0..size_a {
                out.push(
                    base.wrapping_mul(31)
                        .wrapping_add((i as u8).wrapping_mul(17))
                        ^ sf,
                );
            }
        }
    }
    out
}

#[test]
fn streamed_chunks_equal_full_decode() {
    let channels = 1u32;
    let codes = 200u32;
    let subframes = 6u32; // three frames
    let data = synth_adpcm(channels, codes, subframes);

    let full = crate::adpcm::decode(&data, 1, 22050).expect("full decode");

    // Chunked decode with several frame sizes; every one must be bit-exact.
    for chunk_frames in [1usize, 2, 3, 5, 100] {
        let spec = WaveSpec {
            codec: Codec::UbiAdpcm,
            channels: 1,
            sample_rate: 22050,
            data: DataLocation::Internal(crate::Span {
                start: 0,
                end: data.len(),
            }),
        };
        let stream = WaveStream::new(spec, Arc::from(data.clone()), 0, data.len()).expect("stream");
        let chunked = decode_stream(stream, chunk_frames).expect("chunked decode");
        assert_eq!(
            chunked, full,
            "chunk_frames={chunk_frames} differs from the full decode"
        );
    }
}

#[test]
fn chunk_frames_never_splits_a_frame() {
    // Stereo: every chunk's sample count must be a multiple of 2.
    let data = synth_adpcm(2, 100, 4);
    let len = data.len();
    let spec = WaveSpec {
        codec: Codec::UbiAdpcm,
        channels: 2,
        sample_rate: 22050,
        data: DataLocation::Internal(crate::Span { start: 0, end: len }),
    };
    let mut stream = WaveStream::new(spec, Arc::from(data), 0, len).expect("stream");
    loop {
        let chunk = stream.next_chunk(1).expect("chunk");
        if chunk.frames == 0 {
            break;
        }
        assert_eq!(chunk.samples.len() % 2, 0, "chunk split a stereo frame");
        assert_eq!(chunk.samples.len(), chunk.frames * 2);
    }
}

#[test]
fn pcm_stream_reads_entry_range() {
    // A small internal PCM range: 8 samples of a known pattern.
    let samples: Vec<i16> = vec![100, -100, 200, -200, 300, -300, 400, -400];
    let mut bytes = Vec::new();
    for s in &samples {
        bytes.extend_from_slice(&s.to_le_bytes());
    }
    let spec = WaveSpec {
        codec: Codec::Pcm,
        channels: 2,
        sample_rate: 8000,
        data: DataLocation::Internal(crate::Span {
            start: 0,
            end: bytes.len(),
        }),
    };
    let mut stream = WaveStream::new(spec, Arc::from(bytes), 0, 16).expect("stream");
    let c1 = stream.next_chunk(1).expect("chunk1");
    assert_eq!(c1.frames, 1);
    assert_eq!(c1.samples, vec![100, -100]);
    let c2 = stream.next_chunk(100).expect("rest");
    assert_eq!(c2.samples, vec![200, -200, 300, -300, 400, -400]);
    assert!(stream.is_finished());
    assert_eq!(stream.next_chunk(1).expect("end").frames, 0);
}

#[test]
fn stream_entry_matches_decode_entry_on_a_synthetic_bank() {
    // A synthetic bank with one internal PCM wave, parsed by the real parser.
    let data = synth_pcm_bank();
    let bank = hx::parse_bank(&data, &hx::HxLimits::default()).expect("parse");
    let full = decode_entry(&bank, 0, &data, None).expect("full");
    let stream = stream_entry(&bank, 0, &data, None).expect("stream");
    let chunked = decode_stream(stream, 1).expect("chunked");
    assert_eq!(chunked, full);
}

/// One internal-PCM wave bank (same shape as the library tests' synth).
fn synth_pcm_bank() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(b"CPCWaveFileIdObj");
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.extend_from_slice(&3u32.to_le_bytes());
    body.extend_from_slice(&0u32.to_le_bytes());
    body.push(0);
    body.extend_from_slice(b"RIFF");
    body.extend_from_slice(&40u32.to_le_bytes());
    body.extend_from_slice(b"WAVE");
    body.extend_from_slice(b"fmt ");
    body.extend_from_slice(&16u32.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&1u16.to_le_bytes());
    body.extend_from_slice(&8000u32.to_le_bytes());
    body.extend_from_slice(&16000u32.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(&16u16.to_le_bytes());
    body.extend_from_slice(b"data");
    body.extend_from_slice(&8u32.to_le_bytes());
    body.extend_from_slice(&10_000i16.to_le_bytes());
    body.extend_from_slice(&(-10_000i16).to_le_bytes());
    body.extend_from_slice(&5_000i16.to_le_bytes());
    body.extend_from_slice(&(-5_000i16).to_le_bytes());
    let wave_size = body.len();

    let base = 4usize;
    let index_off = base + body.len();
    let mut index = Vec::new();
    index.extend_from_slice(b"INDX");
    index.extend_from_slice(&2u32.to_le_bytes());
    index.extend_from_slice(&1u32.to_le_bytes());
    index.extend_from_slice(&16u32.to_le_bytes());
    index.extend_from_slice(b"CPCWaveFileIdObj");
    index.extend_from_slice(&0u32.to_le_bytes());
    index.extend_from_slice(&0u32.to_le_bytes());
    index.extend_from_slice(&(base as u32).to_le_bytes());
    index.extend_from_slice(&(wave_size as u32).to_le_bytes());
    index.extend_from_slice(&0u32.to_le_bytes());
    index.extend_from_slice(&0u32.to_le_bytes());
    index.extend_from_slice(&0u32.to_le_bytes());

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&(index_off as u32).to_le_bytes());
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(&index);
    bytes
}
