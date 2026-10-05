//! Synthetic bank tests. Fixtures are built in code; no game bytes are used.

use super::*;

struct Fixture {
    bytes: Vec<u8>,
    wave_off: usize,
    wavres_off: usize,
    program_off: usize,
    index_off: usize,
}

fn u32b(v: u32) -> [u8; 4] {
    v.to_le_bytes()
}
fn u16b(v: u16) -> [u8; 2] {
    v.to_le_bytes()
}

fn put(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(bytes);
}

/// Builds a one-wave bank with a `WavRes` name and a `Program` record.
fn build_bank() -> Fixture {
    let mut body = Vec::new();
    let base = 4usize;

    // Wave record.
    let wave_body = body.len();
    put(&mut body, &u32b(16));
    put(&mut body, b"CPCWaveFileIdObj");
    put(&mut body, &u32b(0x1111_1111));
    put(&mut body, &u32b(0x2222_2222));
    put(&mut body, &u32b(3)); // flag type
    put(&mut body, &u32b(0)); // parent id
    put(&mut body, &[0]); // stream mode: internal
    let riff_start = body.len();
    put(&mut body, b"RIFF");
    put(&mut body, &u32b(40)); // RIFF size (WAVE + fmt + data)
    put(&mut body, b"WAVE");
    put(&mut body, b"fmt ");
    put(&mut body, &u32b(16));
    put(&mut body, &u16b(1)); // PCM
    put(&mut body, &u16b(1)); // mono
    put(&mut body, &u32b(8000));
    put(&mut body, &u32b(16_000));
    put(&mut body, &u16b(2));
    put(&mut body, &u16b(16));
    put(&mut body, b"data");
    put(&mut body, &u32b(4));
    let data_off = body.len();
    put(&mut body, &10_000i16.to_le_bytes());
    let negative: i16 = -10_000;
    put(&mut body, &negative.to_le_bytes());
    let wave_size = body.len() - wave_body;
    assert_eq!(data_off - riff_start, 44);
    let _ = riff_start;

    // WavRes record.
    let wavres_body = body.len();
    put(&mut body, &u32b(13));
    put(&mut body, b"CPCWavResData");
    put(&mut body, &u32b(0x3333_3333));
    put(&mut body, &u32b(0x4444_4444));
    put(&mut body, &u32b(0)); // flags
    put(&mut body, &u32b(9)); // name size
    put(&mut body, b"TestSound");
    let wavres_size = body.len() - wavres_body;

    // Program record (header bytes never parsed).
    let program_body = body.len();
    put(&mut body, &u32b(15));
    put(&mut body, b"CProgramResData");
    put(&mut body, &[0u8; 8]);
    let program_size = body.len() - program_body;

    let wave_off = base + wave_body;
    let wavres_off = base + wavres_body;
    let program_off = base + program_body;
    let index_off = base + body.len();
    let mut index = Vec::new();
    put(&mut index, b"INDX");
    put(&mut index, &u32b(2)); // index type
    put(&mut index, &u32b(3)); // entry count
    // Entry 0: wave.
    put(&mut index, &u32b(16));
    put(&mut index, b"CPCWaveFileIdObj");
    put(&mut index, &u32b(0x1111_1111));
    put(&mut index, &u32b(0x2222_2222));
    put(&mut index, &u32b(wave_off as u32));
    put(&mut index, &u32b(wave_size as u32));
    put(&mut index, &u32b(0)); // unknown
    put(&mut index, &u32b(0)); // links
    put(&mut index, &u32b(0)); // languages
    // Entry 1: WavRes linking to the wave.
    put(&mut index, &u32b(13));
    put(&mut index, b"CPCWavResData");
    put(&mut index, &u32b(0x3333_3333));
    put(&mut index, &u32b(0x4444_4444));
    put(&mut index, &u32b(wavres_off as u32));
    put(&mut index, &u32b(wavres_size as u32));
    put(&mut index, &u32b(0));
    put(&mut index, &u32b(1)); // one link
    put(&mut index, &u32b(0x1111_1111));
    put(&mut index, &u32b(0x2222_2222));
    put(&mut index, &u32b(0));
    // Entry 2: program.
    put(&mut index, &u32b(15));
    put(&mut index, b"CProgramResData");
    put(&mut index, &u32b(0x5555_5555));
    put(&mut index, &u32b(0x6666_6666));
    put(&mut index, &u32b(program_off as u32));
    put(&mut index, &u32b(program_size as u32));
    put(&mut index, &u32b(0));
    put(&mut index, &u32b(0));
    put(&mut index, &u32b(0));

    let mut bytes = Vec::new();
    put(&mut bytes, &u32b(index_off as u32));
    bytes.extend_from_slice(&body);
    bytes.extend_from_slice(&index);

    Fixture {
        bytes,
        wave_off,
        wavres_off,
        program_off,
        index_off,
    }
}

#[test]
fn parses_internal_wave_links_name_and_covers_all_bytes() {
    let fx = build_bank();
    let bank = parse_bank(&fx.bytes, &HxLimits::default()).expect("parse");
    assert_eq!(bank.index_type, 2);
    assert_eq!(bank.entries.len(), 3);
    assert_eq!(bank.file_size, fx.bytes.len());
    assert_eq!(
        bank.covered_bytes,
        fx.bytes.len(),
        "index + record headers should cover the whole synthetic bank"
    );
    assert!(bank.unparsed.is_empty(), "unparsed: {:?}", bank.unparsed);

    let wave = bank.entries[0].as_wave().expect("wave");
    assert_eq!(wave.codec, Codec::Pcm);
    assert_eq!(wave.channels, 1);
    assert_eq!(wave.sample_rate, 8000);
    assert_eq!(wave.name.as_deref(), Some("TestSound"));
    assert_eq!(
        wave.data,
        DataLocation::Internal(Span {
            start: fx.wave_off + 81,
            end: fx.wave_off + 85
        })
    );
    assert_eq!(bank.entries[1].index, 1);
    assert_eq!(bank.entries[2].header_span.start, fx.program_off);
    assert_eq!(bank.entries[0].header_span.start, fx.wave_off);
    assert_eq!(bank.entries[1].header_span.start, fx.wavres_off);

    // Name and index selectors both resolve to the wave entry.
    assert_eq!(bank.find_entry("TestSound"), Some(0));
    assert_eq!(bank.find_entry("testsound"), Some(0));
    assert_eq!(bank.find_entry("0"), Some(0));
    assert_eq!(bank.find_entry("missing"), None);

    let audio = crate::decode_entry(&bank, 0, &fx.bytes, None).expect("decode");
    assert_eq!(audio.channels, 1);
    assert_eq!(audio.sample_rate, 8000);
    assert_eq!(audio.samples, vec![10_000, -10_000]);
}

#[test]
fn rejects_truncated_bank_without_panicking() {
    let fx = build_bank();
    for len in 0..fx.bytes.len() {
        // Any prefix may parse only if it happens to contain the full structures; we require
        // that it never panics and that clearly-short prefixes error.
        let result = parse_bank(&fx.bytes[..len], &HxLimits::default());
        if len < fx.index_off {
            assert!(
                result.is_err(),
                "prefix of {len} bytes should not parse to a complete bank"
            );
        }
    }
}

#[test]
fn rejects_out_of_range_record_header() {
    let mut fx = build_bank();
    // Entry 0 header_offset field: index start + class field (4+16) + cuuid (8).
    let field = fx.index_off + 12 + 28;
    fx.bytes[field..field + 4].copy_from_slice(&u32b(0xFFFF_FFFF));
    let err = parse_bank(&fx.bytes, &HxLimits::default()).unwrap_err();
    assert_eq!(err.kind(), AudioErrorKind::BadSize);
}

#[test]
fn rejects_out_of_range_index_pointer() {
    let mut fx = build_bank();
    fx.bytes[0..4].copy_from_slice(&u32b(0xFFFF_FFFF));
    let err = parse_bank(&fx.bytes, &HxLimits::default()).unwrap_err();
    assert!(matches!(
        err.kind(),
        AudioErrorKind::Truncated | AudioErrorKind::BadTag
    ));
}

#[test]
fn rejects_bad_index_tag() {
    let mut fx = build_bank();
    fx.bytes[fx.index_off..fx.index_off + 4].copy_from_slice(b"XXXX");
    let err = parse_bank(&fx.bytes, &HxLimits::default()).unwrap_err();
    assert_eq!(err.kind(), AudioErrorKind::BadTag);
}

#[test]
fn rejects_unknown_class() {
    let mut fx = build_bank();
    fx.bytes[fx.index_off + 12 + 4..fx.index_off + 12 + 4 + 16]
        .copy_from_slice(b"CPCNotARealThing");
    let err = parse_bank(&fx.bytes, &HxLimits::default()).unwrap_err();
    assert_eq!(err.kind(), AudioErrorKind::UnknownClass);
}

#[test]
fn entry_limits_reject_absurd_counts() {
    let mut fx = build_bank();
    // Set entry count to 1_000_000 (past the default 1<<20 limit).
    let count_field = fx.index_off + 8;
    fx.bytes[count_field..count_field + 4].copy_from_slice(&u32b(1 << 21));
    let err = parse_bank(&fx.bytes, &HxLimits::default()).unwrap_err();
    assert_eq!(err.kind(), AudioErrorKind::BadSize);
}
