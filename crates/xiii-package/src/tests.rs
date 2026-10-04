//! Unit tests on synthetic, generated packages. No proprietary bytes are used.

use crate::*;

// ---------------------------------------------------------------------------------------------
// Synthetic package builder
// ---------------------------------------------------------------------------------------------

/// Encodes an old-style Unreal compact index (inverse of `Cursor::compact_index`).
fn compact(value: i32) -> Vec<u8> {
    let mut magnitude = u64::from(value.unsigned_abs());
    let mut first = (magnitude & 0x3f) as u8;
    if value < 0 {
        first |= 0x80;
    }
    magnitude >>= 6;
    if magnitude != 0 {
        first |= 0x40;
    }
    let mut out = vec![first];
    for index in 1..5 {
        if magnitude == 0 {
            break;
        }
        if index == 4 {
            out.push(magnitude as u8);
            magnitude = 0;
        } else {
            let mut byte = (magnitude & 0x7f) as u8;
            magnitude >>= 7;
            if magnitude != 0 {
                byte |= 0x80;
            }
            out.push(byte);
        }
    }
    out
}

fn latin1(text: &str) -> Vec<u8> {
    let bytes: Vec<u8> = text
        .chars()
        .map(|c| u8::try_from(u32::from(c)).unwrap())
        .collect();
    let mut out = compact(i32::try_from(bytes.len() + 1).unwrap());
    out.extend(bytes);
    out.push(0);
    out
}

fn utf16(text: &str) -> Vec<u8> {
    let units: Vec<u16> = text.encode_utf16().collect();
    let mut out = compact(-i32::try_from(units.len() + 1).unwrap());
    for u in units {
        out.extend(u.to_le_bytes());
    }
    out.extend([0, 0]);
    out
}

#[derive(Clone)]
struct RawImport {
    class_package: i32,
    class_name: i32,
    outer: i32,
    name: i32,
}

#[derive(Clone)]
struct RawExport {
    class: i32,
    super_ref: i32,
    outer: i32,
    name: i32,
    flags: u32,
    payload: Vec<u8>,
    /// Overrides the computed (size, offset) pair; offset is written only when size != 0.
    span_override: Option<(i32, i32)>,
}

impl RawExport {
    fn new(class: i32, outer: i32, name: i32, payload_len: usize) -> Self {
        Self {
            class,
            super_ref: 0,
            outer,
            name,
            flags: 0x0007_0004,
            payload: (0..payload_len).map(|i| i as u8).collect(),
            span_override: None,
        }
    }
}

#[derive(Clone)]
struct Builder {
    version: u16,
    licensee: u16,
    flags: u32,
    guid: [u8; 16],
    /// `None` writes one generation matching the final counts.
    generations: Option<Vec<(i32, i32)>>,
    /// Bytes inserted between the summary and the first table.
    header_padding: Vec<u8>,
    names: Vec<(Vec<u8>, u32)>,
    imports: Vec<RawImport>,
    exports: Vec<RawExport>,
}

struct Built {
    bytes: Vec<u8>,
    name_offset: usize,
    import_offset: usize,
    export_offset: usize,
}

impl Builder {
    fn new() -> Self {
        Self {
            version: 100,
            licensee: 58,
            flags: 1,
            guid: *b"0123456789abcdef",
            generations: None,
            header_padding: Vec::new(),
            names: Vec::new(),
            imports: Vec::new(),
            exports: Vec::new(),
        }
    }

    fn name(&mut self, text: &str) -> i32 {
        self.names.push((latin1(text), 0x0007_0010));
        i32::try_from(self.names.len() - 1).unwrap()
    }

    /// Adds an import and returns its raw (negative) reference.
    fn import(&mut self, class_package: i32, class_name: i32, outer: i32, name: i32) -> i32 {
        self.imports.push(RawImport {
            class_package,
            class_name,
            outer,
            name,
        });
        -i32::try_from(self.imports.len()).unwrap()
    }

    /// Adds an export and returns its raw (positive) reference.
    fn export(&mut self, export: RawExport) -> i32 {
        self.exports.push(export);
        i32::try_from(self.exports.len()).unwrap()
    }

    fn build(&self) -> Built {
        let generations = self
            .generations
            .clone()
            .unwrap_or_else(|| vec![(self.exports.len() as i32, self.names.len() as i32)]);
        let mut b = Vec::new();
        b.extend(PACKAGE_TAG.to_le_bytes());
        b.extend(self.version.to_le_bytes());
        b.extend(self.licensee.to_le_bytes());
        b.extend(self.flags.to_le_bytes());
        b.extend([0u8; 24]); // patched below
        b.extend(self.guid);
        b.extend((generations.len() as i32).to_le_bytes());
        for (e, n) in &generations {
            b.extend(e.to_le_bytes());
            b.extend(n.to_le_bytes());
        }
        b.extend(&self.header_padding);

        let name_offset = b.len();
        for (encoded, flags) in &self.names {
            b.extend(encoded);
            b.extend(flags.to_le_bytes());
        }
        let mut payload_offsets = Vec::new();
        for e in &self.exports {
            payload_offsets.push(b.len());
            b.extend(&e.payload);
        }
        let import_offset = b.len();
        for i in &self.imports {
            b.extend(compact(i.class_package));
            b.extend(compact(i.class_name));
            b.extend(i.outer.to_le_bytes());
            b.extend(compact(i.name));
        }
        let export_offset = b.len();
        for (e, payload_offset) in self.exports.iter().zip(payload_offsets) {
            b.extend(compact(e.class));
            b.extend(compact(e.super_ref));
            b.extend(e.outer.to_le_bytes());
            b.extend(compact(e.name));
            b.extend(e.flags.to_le_bytes());
            let (size, offset) = e
                .span_override
                .unwrap_or((e.payload.len() as i32, payload_offset as i32));
            b.extend(compact(size));
            if size != 0 {
                b.extend(compact(offset));
            }
        }
        let header = [
            self.names.len() as i32,
            name_offset as i32,
            self.exports.len() as i32,
            export_offset as i32,
            self.imports.len() as i32,
            import_offset as i32,
        ];
        for (i, v) in header.iter().enumerate() {
            b[12 + 4 * i..16 + 4 * i].copy_from_slice(&v.to_le_bytes());
        }
        Built {
            bytes: b,
            name_offset,
            import_offset,
            export_offset,
        }
    }
}

fn set_i32(bytes: &mut [u8], at: usize, value: i32) {
    bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
}

/// A small package resembling a real one: root package imports, imported classes, a local
/// class, a group with nested objects, zero-sized and sized exports.
fn sample() -> Builder {
    let mut b = Builder::new();
    let none = b.name("None");
    let core = b.name("Core");
    let engine = b.name("Engine");
    let package = b.name("Package");
    let class = b.name("Class");
    let texture = b.name("Texture");
    let object = b.name("Object");
    let my_group = b.name("MyGroup");
    let tex1 = b.name("Tex1");
    let tex2 = b.name("Tex2");
    let my_class = b.name("MyClass");
    let inst = b.name("MyClass0");
    let _ = none;
    let i_core = b.import(core, package, 0, core);
    let i_engine = b.import(core, package, 0, engine);
    let i_texture = b.import(core, class, i_engine, texture);
    let i_object = b.import(core, class, i_core, object);
    let i_package = b.import(core, class, i_core, package);
    let mut local_class = RawExport::new(0, 0, my_class, 10);
    local_class.super_ref = i_object;
    let e_class = b.export(local_class);
    let e_group = b.export(RawExport::new(i_package, 0, my_group, 0));
    b.export(RawExport::new(i_texture, e_group, tex1, 20));
    b.export(RawExport::new(i_texture, e_group, tex2, 0));
    b.export(RawExport::new(e_class, 0, inst, 3));
    b
}

fn parse(bytes: &[u8]) -> Result<Package> {
    Package::parse(bytes, &Limits::default())
}

fn kind(result: Result<Package>) -> ErrorKind {
    result.expect_err("expected a parse error").kind
}

fn decode_compact(bytes: &[u8]) -> Result<(i32, usize)> {
    let mut c = Cursor::new(bytes);
    let v = c.compact_index()?;
    Ok((v, c.pos()))
}

// ---------------------------------------------------------------------------------------------
// Compact index
// ---------------------------------------------------------------------------------------------

#[test]
fn compact_index_round_trips_boundaries() {
    let values = [
        0,
        1,
        -1,
        63,
        -63,
        64,
        -64,
        8191,
        8192,
        (1 << 20) - 1,
        1 << 20,
        (1 << 27) - 1,
        1 << 27,
        -(1 << 27),
        i32::MAX,
        -i32::MAX,
        i32::MIN,
    ];
    for v in values {
        let encoded = compact(v);
        let (decoded, used) = decode_compact(&encoded).unwrap();
        assert_eq!(decoded, v, "value {v} encoded {encoded:02x?}");
        assert_eq!(used, encoded.len());
    }
    // Encoded lengths at the 6/13/20/27-bit boundaries.
    assert_eq!(compact(63).len(), 1);
    assert_eq!(compact(64).len(), 2);
    assert_eq!(compact(8191).len(), 2);
    assert_eq!(compact(8192).len(), 3);
    assert_eq!(compact((1 << 27) - 1).len(), 4);
    assert_eq!(compact(1 << 27).len(), 5);
    assert_eq!(compact(i32::MIN), vec![0xc0, 0x80, 0x80, 0x80, 0x10]);
}

#[test]
fn compact_index_known_encodings() {
    assert_eq!(decode_compact(&[0x00]).unwrap(), (0, 1));
    assert_eq!(decode_compact(&[0x3f]).unwrap(), (63, 1));
    assert_eq!(decode_compact(&[0x81]).unwrap(), (-1, 1));
    assert_eq!(decode_compact(&[0x40, 0x01]).unwrap(), (64, 2));
    // Not LEB128: the low 6 bits come first, then 7-bit groups.
    assert_eq!(decode_compact(&[0x45, 0x02]).unwrap(), (5 | (2 << 6), 2));
    // Stops at the first byte without a continuation bit; trailing data is left unread.
    assert_eq!(decode_compact(&[0x01, 0xff]).unwrap(), (1, 1));
}

#[test]
fn compact_index_accepts_negative_zero_and_non_minimal_like_probe() {
    assert_eq!(decode_compact(&[0x80]).unwrap(), (0, 1));
    assert_eq!(decode_compact(&[0x40, 0x00]).unwrap(), (0, 2));
    assert_eq!(
        decode_compact(&[0x41, 0x80, 0x80, 0x80, 0x00]).unwrap(),
        (1, 5)
    );
}

#[test]
fn compact_index_rejects_out_of_range_magnitudes() {
    // +2^31: magnitude 0x80000000 = bit 31, which lives in byte 4 (shift 27) as 0x10.
    let err = decode_compact(&[0x40, 0x80, 0x80, 0x80, 0x10]).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::CompactIndexOutOfRange {
            magnitude: 0x8000_0000,
            negative: false
        }
    );
    assert_eq!(err.offset, Some(0));
    // -(2^32 - 1).
    let err = decode_compact(&[0xff, 0xff, 0xff, 0xff, 0x1f]).unwrap_err();
    assert!(matches!(
        err.kind,
        ErrorKind::CompactIndexOutOfRange { negative: true, .. }
    ));
}

#[test]
fn compact_index_rejects_overlong_fifth_byte() {
    for fifth in [0x20u8, 0x40, 0x80, 0xff] {
        let err = decode_compact(&[0x40, 0x80, 0x80, 0x80, fifth]).unwrap_err();
        assert_eq!(
            err.kind,
            ErrorKind::CompactIndexOverflow,
            "fifth byte {fifth:#x}"
        );
    }
}

#[test]
fn compact_index_truncated_reports_offset() {
    for bytes in [&[][..], &[0x40], &[0x40, 0x80], &[0x40, 0x80, 0x80, 0x80]] {
        let err = decode_compact(bytes).unwrap_err();
        assert!(matches!(
            err.kind,
            ErrorKind::UnexpectedEof {
                needed: 1,
                available: 0
            }
        ));
        assert_eq!(err.offset, Some(bytes.len() as u64));
    }
}

// ---------------------------------------------------------------------------------------------
// Strings
// ---------------------------------------------------------------------------------------------

fn decode_string(bytes: &[u8], max: u32) -> Result<String> {
    Cursor::new(bytes).fstring(max)
}

#[test]
fn fstring_encodings() {
    assert_eq!(decode_string(&latin1("Engine"), 64).unwrap(), "Engine");
    assert_eq!(
        decode_string(&latin1("caf\u{e9}"), 64).unwrap(),
        "caf\u{e9}"
    );
    assert_eq!(
        decode_string(&utf16("Zo\u{e9}\u{4e2d}"), 64).unwrap(),
        "Zo\u{e9}\u{4e2d}"
    );
    assert_eq!(decode_string(&[0x00], 64).unwrap(), "");
    // Length 1 = terminator only.
    assert_eq!(decode_string(&[0x01, 0x00], 64).unwrap(), "");
    assert_eq!(decode_string(&[0x81, 0x00, 0x00], 64).unwrap(), "");
    // Interior NUL is preserved as data, as in the probe.
    assert_eq!(decode_string(&[0x03, b'a', 0, 0], 64).unwrap(), "a\0");
}

#[test]
fn fstring_unterminated() {
    let err = decode_string(&[0x02, b'a', b'b'], 64).unwrap_err();
    assert_eq!(err.kind, ErrorKind::UnterminatedString);
    let err = decode_string(&[0x82, b'a', 0, b'b', 0], 64).unwrap_err();
    assert_eq!(err.kind, ErrorKind::UnterminatedString);
    // A UTF-16 string ending in a single zero byte is still unterminated.
    let err = decode_string(&[0x81, 0x00, 0x01], 64).unwrap_err();
    assert_eq!(err.kind, ErrorKind::UnterminatedString);
}

#[test]
fn fstring_invalid_utf16() {
    let mut bytes = compact(-2);
    bytes.extend(0xd800u16.to_le_bytes());
    bytes.extend([0, 0]);
    assert_eq!(
        decode_string(&bytes, 64).unwrap_err().kind,
        ErrorKind::InvalidUtf16
    );
}

#[test]
fn fstring_length_checked_before_reading() {
    let mut bytes = compact(1 << 30);
    bytes.push(b'x');
    let err = decode_string(&bytes, 1_000_000).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::StringTooLong {
            length: 1 << 30,
            max: 1_000_000
        }
    );
    let err = decode_string(&compact(i32::MIN), u32::MAX).unwrap_err();
    // Within the limit but impossible to satisfy from the buffer: EOF, no allocation.
    assert!(matches!(err.kind, ErrorKind::UnexpectedEof { .. }));
    let err = decode_string(&compact(-1000), 999).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::StringTooLong {
            length: -1000,
            max: 999
        }
    );
    let err = decode_string(&[0x05, b'a', b'b'], 64).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::UnexpectedEof {
            needed: 5,
            available: 2
        }
    );
    assert_eq!(err.offset, Some(1));
}

// ---------------------------------------------------------------------------------------------
// Valid package
// ---------------------------------------------------------------------------------------------

#[test]
fn sample_package_parses_with_paths_classes_and_spans() {
    let built = sample().build();
    let p = parse(&built.bytes).unwrap();
    let s = p.summary();
    assert_eq!((s.version, s.licensee, s.package_flags), (100, 58, 1));
    assert_eq!(
        (s.names.count, s.imports.count, s.exports.count),
        (12, 5, 5)
    );
    assert_eq!(s.names.offset as usize, built.name_offset);
    assert_eq!(s.guid, *b"0123456789abcdef");
    assert_eq!(
        s.generations,
        vec![Generation {
            export_count: 5,
            name_count: 12
        }]
    );
    assert_eq!(s.header_end, 64);
    assert_eq!(s.header_gap(), 0);
    assert_eq!(s.latest_generation_matches_tables(), Some(true));
    assert_eq!(s.guid_string(), "33323130-37363534-62613938-66656463");

    assert_eq!(p.names()[1].text, "Core");
    assert_eq!(p.names()[1].flags, 0x0007_0010);
    assert_eq!(
        p.names()[0].span,
        Span {
            start: 64,
            end: 64 + 6 + 4
        }
    );
    let spans = p.table_spans();
    assert_eq!(spans.imports.start, built.import_offset);
    assert_eq!(spans.exports.start, built.export_offset);
    assert_eq!(spans.exports.end, built.bytes.len());

    assert_eq!(p.object_path(ObjectRef::Import(2)), Some("Engine.Texture"));
    assert_eq!(p.object_path(ObjectRef::Export(2)), Some("MyGroup.Tex1"));
    assert_eq!(p.object_path(ObjectRef::Null), None);
    assert_eq!(p.export_class_path(0), Some(NULL_CLASS_PATH));
    assert_eq!(p.export_class_path(2), Some("Engine.Texture"));
    assert_eq!(p.export_class_path(4), Some("MyClass"));
    assert_eq!(p.export_class_path(99), None);
    assert_eq!(p.exports()[0].super_ref, ObjectRef::Import(3));
    assert_eq!(p.exports()[0].super_ref.raw(), -4);
    assert_eq!(p.exports()[3].outer, ObjectRef::Export(1));
    assert_eq!(p.exports()[3].serial_offset, 0);
    assert_eq!(p.exports()[3].serial_span(), None);
    assert_eq!(p.exports()[2].serial_size, 20);
    assert_eq!(p.imported_packages(), vec!["Core", "Engine"]);

    let classes: Vec<(String, u32)> = p.export_class_counts().into_iter().collect();
    assert_eq!(
        classes,
        vec![
            ("Core.Class".into(), 1),
            ("Core.Package".into(), 1),
            ("Engine.Texture".into(), 2),
            ("MyClass".into(), 1),
        ]
    );
    let zero: Vec<(String, u32)> = p.zero_size_export_counts().into_iter().collect();
    assert_eq!(
        zero,
        vec![("Core.Package".into(), 1), ("Engine.Texture".into(), 1)]
    );
    assert!(p.unaccounted_ranges().is_empty());
    assert!(p.overlapping_ranges().is_empty());
}

#[test]
fn object_ref_raw_round_trip_and_range() {
    assert_eq!(ObjectRef::from_raw(0, 0, 0), Some(ObjectRef::Null));
    assert_eq!(ObjectRef::from_raw(1, 0, 1), Some(ObjectRef::Export(0)));
    assert_eq!(ObjectRef::from_raw(2, 0, 1), None);
    assert_eq!(ObjectRef::from_raw(-1, 1, 0), Some(ObjectRef::Import(0)));
    assert_eq!(ObjectRef::from_raw(-2, 1, 0), None);
    assert_eq!(
        ObjectRef::from_raw(i32::MIN, u32::MAX, 0),
        Some(ObjectRef::Import(i32::MAX as u32))
    );
    for raw in [-5, -1, 0, 1, 5] {
        assert_eq!(ObjectRef::from_raw(raw, 10, 10).unwrap().raw(), raw);
    }
}

#[test]
fn utf16_names_and_unicode_paths() {
    let mut b = Builder::new();
    b.names.push((utf16("\u{c9}t\u{e9}"), 0));
    b.export(RawExport::new(0, 0, 0, 0));
    let p = parse(&b.build().bytes).unwrap();
    assert_eq!(p.object_path(ObjectRef::Export(0)), Some("\u{c9}t\u{e9}"));
}

#[test]
fn empty_package_and_multiple_generations() {
    let mut b = Builder::new();
    b.generations = Some(vec![(3, 4), (5, 6), (0, 0)]);
    let p = parse(&b.build().bytes).unwrap();
    assert_eq!(p.summary().generations.len(), 3);
    assert_eq!(p.summary().header_end, 52 + 4 + 24);
    assert_eq!(p.summary().latest_generation_matches_tables(), Some(true));
    assert!(p.imported_packages().is_empty());

    b.generations = Some(vec![]);
    let p = parse(&b.build().bytes).unwrap();
    assert_eq!(p.summary().latest_generation_matches_tables(), None);

    let mut b = sample();
    b.generations = Some(vec![(1, 1)]);
    let p = parse(&b.build().bytes).unwrap();
    assert_eq!(p.summary().latest_generation_matches_tables(), Some(false));
}

#[test]
fn header_padding_is_reported_as_gap_and_unaccounted() {
    let mut b = sample();
    b.header_padding = vec![0xaa; 7];
    let p = parse(&b.build().bytes).unwrap();
    assert_eq!(p.summary().header_gap(), 7);
    assert_eq!(p.unaccounted_ranges(), vec![Span { start: 64, end: 71 }]);
}

#[test]
fn zero_size_export_does_not_read_offset() {
    let mut b = sample();
    // Size 0 with an override offset that would be invalid if it were read.
    b.exports[1].span_override = Some((0, -12345));
    let p = parse(&b.build().bytes).unwrap();
    assert_eq!(p.exports()[1].serial_size, 0);
    assert_eq!(p.exports()[2].object_name.index(), 8);
}

// ---------------------------------------------------------------------------------------------
// Malformed summaries
// ---------------------------------------------------------------------------------------------

#[test]
fn bad_magic_and_version() {
    let mut bytes = sample().build().bytes;
    bytes[0] ^= 1;
    assert!(matches!(kind(parse(&bytes)), ErrorKind::BadMagic { .. }));
    let mut b = sample();
    b.version = 69;
    b.licensee = 7;
    assert_eq!(
        kind(parse(&b.build().bytes)),
        ErrorKind::UnsupportedVersion {
            version: 69,
            licensee: 7
        }
    );
    assert!(!has_package_tag(&[0xc1, 0x83, 0x2a]));
    assert!(has_package_tag(&sample().build().bytes));
}

#[test]
fn every_truncation_fails_without_panic() {
    let bytes = sample().build().bytes;
    for len in 0..bytes.len() {
        let result = parse(&bytes[..len]);
        assert!(result.is_err(), "truncated to {len} unexpectedly parsed");
    }
    // Truncated summary reports the summary table and offset.
    let err = parse(&bytes[..30]).unwrap_err();
    assert_eq!(err.table, Some(Table::Summary));
    assert_eq!(err.field, Some("import_count"));
    assert_eq!(err.offset, Some(28));
}

#[test]
fn negative_and_excessive_counts_and_offsets() {
    let built = sample().build();
    let mut bytes = built.bytes.clone();
    set_i32(&mut bytes, 12, -1);
    let err = parse(&bytes).unwrap_err();
    assert!(matches!(
        err.kind,
        ErrorKind::CountOutOfRange { count: -1, .. }
    ));
    assert_eq!(err.field, Some("name_count"));

    let mut bytes = built.bytes.clone();
    set_i32(&mut bytes, 20, 1_000_001);
    assert!(matches!(
        kind(parse(&bytes)),
        ErrorKind::CountOutOfRange {
            count: 1_000_001,
            ..
        }
    ));

    let mut bytes = built.bytes.clone();
    let past_end = bytes.len() as i32 + 1;
    set_i32(&mut bytes, 32, past_end);
    let err = parse(&bytes).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::OffsetOutOfRange { .. }));
    assert_eq!(err.field, Some("import_offset"));

    let mut bytes = built.bytes.clone();
    set_i32(&mut bytes, 16, i32::MIN);
    assert!(matches!(
        kind(parse(&bytes)),
        ErrorKind::OffsetOutOfRange { .. }
    ));

    // A large but in-limit count is rejected before allocation because it cannot fit.
    let mut bytes = built.bytes.clone();
    set_i32(&mut bytes, 28, 1_000_000);
    let err = parse(&bytes).unwrap_err();
    assert!(matches!(
        err.kind,
        ErrorKind::TableExceedsData {
            count: 1_000_000,
            ..
        }
    ));
    assert_eq!(err.table, Some(Table::Imports));

    // Custom limits are honoured.
    let limits = Limits {
        max_names: 11,
        ..Limits::default()
    };
    let err = Package::parse(&built.bytes, &limits).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::CountOutOfRange {
            what: "table count",
            count: 12,
            max: 11
        }
    );
    let limits = Limits {
        max_string_units: 4,
        ..Limits::default()
    };
    let err = Package::parse(&built.bytes, &limits).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::StringTooLong { .. }));
    assert_eq!((err.table, err.index), (Some(Table::Names), Some(0)));
}

#[test]
fn generation_count_bounds() {
    let built = sample().build();
    for (value, expect_limit) in [(-1, true), (i32::MAX, true), (4096, false)] {
        let mut bytes = built.bytes.clone();
        set_i32(&mut bytes, 52, value);
        let err = parse(&bytes).unwrap_err();
        if expect_limit {
            assert!(
                matches!(err.kind, ErrorKind::CountOutOfRange { .. }),
                "{value}: {err}"
            );
            assert_eq!(err.field, Some("generation_count"));
        } else {
            assert!(
                matches!(err.kind, ErrorKind::TableExceedsData { .. }),
                "{value}: {err}"
            );
            assert_eq!(err.table, Some(Table::Generations));
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Malformed tables and references
// ---------------------------------------------------------------------------------------------

#[test]
fn invalid_name_references() {
    let mut b = sample();
    b.imports[2].name = 12;
    let err = parse(&b.build().bytes).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::NameIndexOutOfRange {
            index: 12,
            count: 12
        }
    );
    assert_eq!(
        (err.table, err.index, err.field),
        (Some(Table::Imports), Some(2), Some("object_name"))
    );

    let mut b = sample();
    b.imports[0].class_package = -1;
    let err = parse(&b.build().bytes).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::NameIndexOutOfRange {
            index: -1,
            count: 12
        }
    );

    let mut b = sample();
    b.exports[4].name = 1000;
    let err = parse(&b.build().bytes).unwrap_err();
    assert_eq!(
        (err.table, err.index, err.field),
        (Some(Table::Exports), Some(4), Some("object_name"))
    );
}

#[test]
fn invalid_object_references() {
    type Mutation = fn(&mut Builder);
    let cases: [(Mutation, Table, &str); 5] = [
        (|b| b.imports[1].outer = -6, Table::Imports, "outer"),
        (|b| b.exports[0].class = 6, Table::Exports, "class"),
        (|b| b.exports[0].super_ref = -6, Table::Exports, "super"),
        (|b| b.exports[2].outer = 6, Table::Exports, "outer"),
        (|b| b.exports[2].outer = i32::MIN, Table::Exports, "outer"),
    ];
    for (mutate, table, field) in cases {
        let mut b = sample();
        mutate(&mut b);
        let err = parse(&b.build().bytes).unwrap_err();
        assert!(
            matches!(err.kind, ErrorKind::ObjectRefOutOfRange { .. }),
            "{err}"
        );
        assert_eq!((err.table, err.field), (Some(table), Some(field)));
        assert!(err.offset.is_some());
    }
}

#[test]
fn outer_cycles_are_detected() {
    let mut b = sample();
    b.exports[1].outer = 3; // MyGroup -> Tex1 -> MyGroup
    let err = parse(&b.build().bytes).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::OuterCycle { .. }), "{err}");

    let mut b = sample();
    b.exports[0].outer = 1; // self
    assert_eq!(
        kind(parse(&b.build().bytes)),
        ErrorKind::OuterCycle {
            start: 1,
            repeated: 1
        }
    );

    let mut b = sample();
    b.imports[0].outer = -2;
    b.imports[1].outer = -1;
    let err = parse(&b.build().bytes).unwrap_err();
    assert!(matches!(err.kind, ErrorKind::OuterCycle { .. }));
    assert_eq!(err.table, Some(Table::Imports));
}

#[test]
fn outer_depth_limit() {
    let chain = |len: usize| {
        let mut b = Builder::new();
        let n = b.name("Node");
        for i in 0..len {
            b.export(RawExport::new(0, i as i32, n, 0));
        }
        b.build().bytes
    };
    // Default limit: 257 objects in one chain, matching the probe.
    let p = parse(&chain(257)).unwrap();
    assert_eq!(
        p.object_path(ObjectRef::Export(256))
            .unwrap()
            .split('.')
            .count(),
        257
    );
    let err = parse(&chain(258)).unwrap_err();
    assert_eq!(
        err.kind,
        ErrorKind::OuterDepthExceeded {
            start: 258,
            max: 257
        }
    );
    let limits = Limits {
        max_outer_chain: 3,
        ..Limits::default()
    };
    assert!(Package::parse(&chain(3), &limits).is_ok());
    assert!(Package::parse(&chain(4), &limits).is_err());
}

#[test]
fn export_spans_past_eof_or_negative() {
    let len = sample().build().bytes.len() as i32;
    let cases = [
        ((10, len - 5), "span"),
        ((10, i32::MAX), "span"),
        ((10, -1), "span"),
        ((i32::MAX, 0), "span"),
        ((-4, 70), "negative"),
    ];
    for ((size, offset), what) in cases {
        let mut b = sample();
        b.exports[2].span_override = Some((size, offset));
        let err = parse(&b.build().bytes).unwrap_err();
        match what {
            "span" => assert!(
                matches!(err.kind, ErrorKind::ExportSpanOutOfRange { .. }),
                "{err}"
            ),
            _ => assert_eq!(err.kind, ErrorKind::NegativeSerialSize { size }),
        }
        assert_eq!((err.table, err.index), (Some(Table::Exports), Some(2)));
    }
    // A span ending exactly at EOF is valid.
    let mut b = sample();
    b.exports[2].span_override = Some((len, 0));
    let built = b.build();
    assert_eq!(
        built.bytes.len(),
        len as usize,
        "override must keep the encoded length"
    );
    assert!(parse(&built.bytes).is_ok());
}

#[test]
fn error_display_includes_context() {
    let mut b = sample();
    b.exports[0].outer = 1;
    let text = parse(&b.build().bytes).unwrap_err().to_string();
    assert!(
        text.starts_with("exports[0].outer: outer chain from 1 revisits object 1"),
        "{text}"
    );

    let mut b = sample();
    b.imports[2].name = 99;
    let text = parse(&b.build().bytes).unwrap_err().to_string();
    assert!(text.contains("imports[2].object_name at offset"), "{text}");
    assert!(
        text.contains("name index 99 outside name table of 12"),
        "{text}"
    );
    let boxed: Box<dyn std::error::Error> = Box::new(parse(&[]).unwrap_err());
    assert!(boxed.to_string().contains("summary.tag at offset 0"));
}

#[test]
fn mutated_packages_never_panic() {
    let bytes = sample().build().bytes;
    // Deterministic xorshift so failures reproduce.
    let mut state = 0x2545_f491_4f6c_dd1du64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for pos in 0..bytes.len() {
        for mask in [0x01u8, 0x40, 0x80, 0xff] {
            let mut m = bytes.clone();
            m[pos] ^= mask;
            let _ = parse(&m);
        }
    }
    for _ in 0..20_000 {
        let mut m = bytes.clone();
        for _ in 0..(next() % 6 + 1) {
            let pos = (next() as usize) % m.len();
            m[pos] = next() as u8;
        }
        let cut = (next() as usize) % (m.len() + 1);
        let _ = parse(&m);
        let _ = parse(&m[..cut]);
    }
}
