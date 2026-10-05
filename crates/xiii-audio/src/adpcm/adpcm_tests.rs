//! UBI ADPCM tests with hand-derived vectors. No game bytes are used.

use super::*;

fn push_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_le_bytes());
}

/// Packs 6-bit codes MSB-first into 32-bit LE words, matching the reference unpacking.
fn pack6(codes: &[u8]) -> Vec<u8> {
    let mut bits = Vec::new();
    for &c in codes {
        for k in (0..6).rev() {
            bits.push((c >> k) & 1);
        }
    }
    while bits.len() % 32 != 0 {
        bits.push(0);
    }
    let mut out = Vec::new();
    for chunk in bits.chunks(32) {
        let mut w = 0u32;
        for &b in chunk {
            w = (w << 1) | b as u32;
        }
        push_u32(&mut out, w);
    }
    out
}

/// Builds a one-frame mono 6-bit stream with `channels`, `sample_count`, `cpl`/`cps` and codes.
#[allow(clippy::too_many_arguments)]
fn build_stream(
    channels: u32,
    sample_count: u32,
    subframe_count: u32,
    cpl: u32,
    cps: u32,
    bits: u32,
    step1: i32,
    codes: &[u8],
) -> Vec<u8> {
    let mut out = Vec::new();
    push_u32(&mut out, 0x08); // signature
    push_u32(&mut out, sample_count);
    push_u32(&mut out, subframe_count);
    push_u32(&mut out, cpl);
    push_u32(&mut out, cps);
    push_u32(&mut out, 2); // subframes per frame
    push_u32(&mut out, 0); // unknown18
    push_u32(&mut out, 0); // unknown1c
    push_u32(&mut out, 0); // unknown20
    push_u32(&mut out, bits);
    push_u32(&mut out, 1); // unknown28
    push_u32(&mut out, channels);
    assert_eq!(out.len(), 0x30);

    // Channel states: signature 0x02 and a nonzero step so the first vector is deterministic.
    for _ in 0..channels {
        push_u32(&mut out, 0x02);
        out.extend_from_slice(&step1.to_le_bytes());
        out.extend_from_slice(&[0u8; CHANNEL_STATE_SIZE - 8]);
    }
    out.extend_from_slice(&pack6(codes));
    out
}

#[test]
fn expand_code_6bit_matches_hand_derived_vector() {
    // Derivation (states: step1=1024, delta1=hist1=0):
    //   code 31 -> code_signed 0, table1[0]=-1e8, table2[0]=1800.
    //     step0 = (1024*246 + 1800) >> 8 = 991 (clamped); delta0 = 0 since step0_next is negative.
    //     sample = 0 + 0 + 0 = 0; step1 = 991, delta1 = 0, hist1 = 0.
    //   code 63 -> code_signed 32, table1[32]=1800, table2[32]=0.
    //     step0 = (991*246 + 0) >> 8 = 952; step0_next = 1800 + 991 = 2791.
    //     delta0_index = ((2791>>3)&31)+0 = 28, delta0_shift = 10, delta_table[28] = 1841.
    //     sample = 1841 + 0 + 0 = 1841; delta1 = hist1 = 1841.
    //   code 0 -> code_signed -31, table1[31]=1132, table2[31]=102400.
    //     step0 = (952*246 + 102400) >> 8 = 1314; step0_next = 1132 + 952 = 2084.
    //     delta0_index = ((2084>>3)&31)+33 = 4+33 = 37, shift = 8, delta_table[37] = -1099.
    //     delta0 = (-1099 << 8) >> 10 = -275; sample = -275 + 1841 + 1841 = 3407.
    let mut st = ChannelState {
        step1: 1024,
        ..ChannelState::default()
    };
    let got = [
        expand_code_6bit(31, &mut st),
        expand_code_6bit(63, &mut st),
        expand_code_6bit(0, &mut st),
    ];
    assert_eq!(got, [0, 1841, 3407]);
}

#[test]
fn unpack_codes_reads_msb_first_across_word_boundary() {
    // 0x7FF00000 holds codes 31, 63, 0 (documented in the reference) and is stored LE.
    let word = 0x7FF0_0000u32.to_le_bytes();
    let mut codes = Vec::new();
    unpack_codes(&word, 3, 6, &mut codes);
    assert_eq!(codes, vec![31, 63, 0]);

    // A full 6-bit code word round-trips through pack6.
    let values: Vec<u8> = (0..24).collect();
    let packed = pack6(&values);
    let mut out = Vec::new();
    unpack_codes(&packed, values.len(), 6, &mut out);
    assert_eq!(out, values);
}

#[test]
fn decodes_synthetic_mono_stream() {
    let stream = build_stream(1, 3, 1, 3, 3, 6, 1024, &[31, 63, 0]);
    let header = parse_header(&stream).expect("header");
    assert_eq!(header.channels, 1);
    assert_eq!(header.per_channel_samples(), 3);
    let audio = decode(&stream, 1, 8000).expect("decode");
    assert_eq!(audio.channels, 1);
    assert_eq!(audio.sample_rate, 8000);
    assert_eq!(audio.samples, vec![0, 1841, 3407]);
    assert_eq!(audio.frames(), 3);
}

#[test]
fn rejects_four_bit_mode_explicitly() {
    let stream = build_stream(1, 2, 1, 2, 2, 4, 1024, &[1, 2]);
    let err = decode(&stream, 1, 8000).unwrap_err();
    assert_eq!(err.kind(), AudioErrorKind::UnsupportedCodec);
}

#[test]
fn rejects_channel_mismatch() {
    let stream = build_stream(2, 4, 1, 4, 4, 6, 1024, &[31; 8]);
    let err = decode(&stream, 1, 8000).unwrap_err();
    assert_eq!(err.kind(), AudioErrorKind::BadAdpcmHeader);
}

#[test]
fn rejects_declared_sample_count_mismatch() {
    // Three codes decode but the header claims four.
    let stream = build_stream(1, 4, 1, 3, 3, 6, 1024, &[31, 63, 0]);
    let err = decode(&stream, 1, 8000).unwrap_err();
    assert_eq!(err.kind(), AudioErrorKind::SampleCountMismatch);
}

#[test]
fn rejects_bad_signature_and_truncated_header() {
    let mut stream = build_stream(1, 3, 1, 3, 3, 6, 1024, &[31, 63, 0]);
    stream[0] = 0x07;
    assert_eq!(
        parse_header(&stream).unwrap_err().kind(),
        AudioErrorKind::BadAdpcmHeader
    );
    assert_eq!(
        parse_header(&stream[..0x10]).unwrap_err().kind(),
        AudioErrorKind::Truncated
    );
}

#[test]
fn rejects_bad_subframes_per_frame() {
    let mut stream = build_stream(1, 3, 1, 3, 3, 6, 1024, &[31, 63, 0]);
    stream[0x14..0x18].copy_from_slice(&3u32.to_le_bytes());
    assert_eq!(
        parse_header(&stream).unwrap_err().kind(),
        AudioErrorKind::BadAdpcmHeader
    );
}
