//! Synthetic tests for export payloads: state frame, tagged properties, value decoding and
//! malformed input. No proprietary bytes are used.

use crate::tests::{Builder, RawExport, compact, latin1, utf16};
use crate::*;

// Name indices of the fixture package.
const NONE: i32 = 0;
const VECTOR: i32 = 8;
const ROTATOR: i32 = 9;
const COLOR: i32 = 10;
const SCALE: i32 = 11;
const POINT_REGION: i32 = 12;
const BOX: i32 = 13;
const MY_STRUCT: i32 = 14;
const PROP: i32 = 15;
const OTHER: i32 = 16;
const NONE_DUP: i32 = 17;
const FUNC: i32 = 19;
/// Raw reference of the imported `MyPkg.Rock` static mesh.
const ROCK_REF: i32 = -5;

/// Package with one export (`Thing`, payload `payload`, flags `flags`) and a zero-sized one.
fn fixture(payload: Vec<u8>, flags: u32) -> (Vec<u8>, Package) {
    let mut b = Builder::new();
    for n in [
        "None",
        "Core",
        "Engine",
        "Package",
        "Class",
        "StaticMesh",
        "Rock",
        "Thing",
        "Vector",
        "Rotator",
        "Color",
        "Scale",
        "PointRegion",
        "Box",
        "MyStruct",
        "Prop",
        "Other",
        "None",
        "MyPkg",
        "Func",
    ] {
        b.name(n);
    }
    let core = b.import(1, 3, 0, 1);
    let engine = b.import(1, 3, 0, 2);
    let static_mesh = b.import(1, 4, engine, 5);
    let my_pkg = b.import(1, 3, 0, 18);
    let rock = b.import(2, 5, my_pkg, 6);
    assert_eq!((core, rock), (-1, ROCK_REF));
    let mut thing = RawExport::new(static_mesh, 0, 7, 0);
    thing.payload = payload;
    thing.flags = flags;
    b.export(thing);
    b.export(RawExport::new(static_mesh, 0, OTHER, 0));
    let bytes = b.build().bytes;
    let package = Package::parse(&bytes, &Limits::default()).expect("fixture parses");
    (bytes, package)
}

fn read_with(payload: Vec<u8>, flags: u32, limits: &Limits) -> (Package, Result<ObjectProperties>) {
    let (bytes, p) = fixture(payload, flags);
    let r = p.read_object_properties(&bytes, 0, limits);
    (p, r)
}

fn read(payload: Vec<u8>) -> (Package, Result<ObjectProperties>) {
    read_with(payload, 0, &Limits::default())
}

fn ok_props(payload: Vec<u8>) -> (Package, ObjectProperties) {
    let (p, r) = read(payload);
    (p, r.expect("property block decodes"))
}

/// Tag header: name, info, optional struct name, explicit size bytes, array index bytes.
fn tag(name: i32, info: u8, struct_name: Option<i32>, size: &[u8], index: &[u8]) -> Vec<u8> {
    let mut t = compact(name);
    t.push(info);
    if let Some(s) = struct_name {
        t.extend(compact(s));
    }
    t.extend(size);
    t.extend(index);
    t
}

fn f32s(values: &[f32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn i32s(values: &[i32]) -> Vec<u8> {
    values.iter().flat_map(|v| v.to_le_bytes()).collect()
}

fn single(payload_prop: Vec<u8>) -> (Package, Property) {
    let mut payload = payload_prop;
    payload.extend(compact(NONE));
    let (p, o) = ok_props(payload);
    assert_eq!(o.block.properties.len(), 1);
    assert_eq!(o.tail().len(), 0);
    let prop = o.block.properties[0].clone();
    (p, prop)
}

fn single_value(payload_prop: Vec<u8>) -> PropertyValue {
    single(payload_prop).1.value
}

// ---------------------------------------------------------------------------------------------
// Values of each type
// ---------------------------------------------------------------------------------------------

#[test]
fn scalar_types_decode() {
    let mut t = tag(PROP, 0x01, None, &[], &[]);
    t.push(7);
    assert_eq!(single_value(t), PropertyValue::Byte(7));

    let mut t = tag(PROP, 0x22, None, &[], &[]);
    t.extend(i32s(&[-123_456]));
    assert_eq!(single_value(t), PropertyValue::Int(-123_456));

    let mut t = tag(PROP, 0x24, None, &[], &[]);
    t.extend(f32s(&[2.5]));
    assert_eq!(single_value(t), PropertyValue::Float(2.5));

    let mut t = tag(PROP, 0x05, None, &[], &[]);
    t.extend(compact(ROCK_REF));
    assert_eq!(single_value(t), PropertyValue::Object(ObjectRef::Import(4)));

    let mut t = tag(PROP, 0x08, None, &[], &[]);
    t.extend(compact(1));
    assert_eq!(single_value(t), PropertyValue::Class(ObjectRef::Export(0)));

    let mut t = tag(PROP, 0x05, None, &[], &[]);
    t.extend(compact(0));
    assert_eq!(single_value(t), PropertyValue::Object(ObjectRef::Null));

    let mut t = tag(PROP, 0x06, None, &[], &[]);
    t.extend(compact(OTHER));
    let (p, prop) = single(t);
    match prop.value {
        PropertyValue::Name(n) => assert_eq!(p.name(n), "Other"),
        v => panic!("{v:?}"),
    }
    assert_eq!(p.property_name(&prop), "Prop");
    assert_eq!(prop.kind, PropertyType::Name);

    let mut t = tag(PROP, 0x17, None, &[], &[]);
    t.extend(compact(1));
    t.extend(compact(FUNC));
    match single_value(t) {
        PropertyValue::Delegate { object, function } => {
            assert_eq!(object, ObjectRef::Export(0));
            assert_eq!(function.index(), FUNC as u32);
        }
        v => panic!("{v:?}"),
    }
}

#[test]
fn strings_decode_latin1_and_utf16() {
    let s = latin1("plage00");
    let mut t = tag(PROP, 0x5d, None, &[s.len() as u8], &[]);
    t.extend(&s);
    assert_eq!(single_value(t), PropertyValue::Str("plage00".into()));

    let s = utf16("Zo\u{e9}");
    let mut t = tag(PROP, 0x6d, None, &(s.len() as u16).to_le_bytes(), &[]);
    t.extend(&s);
    assert_eq!(single_value(t), PropertyValue::Str("Zo\u{e9}".into()));

    // Empty string (length 0) in a one-byte value.
    let mut t = tag(PROP, 0x0d, None, &[], &[]);
    t.push(0);
    assert_eq!(single_value(t), PropertyValue::Str(String::new()));
}

#[test]
fn known_structs_decode_when_size_matches() {
    let mut t = tag(PROP, 0x3a, Some(VECTOR), &[], &[]);
    t.extend(f32s(&[1.0, -2.0, 3.5]));
    assert_eq!(
        single_value(t),
        PropertyValue::Struct(StructValue::Vector([1.0, -2.0, 3.5]))
    );

    let mut t = tag(PROP, 0x3a, Some(ROTATOR), &[], &[]);
    t.extend(i32s(&[0, -3072, 16384]));
    assert_eq!(
        single_value(t),
        PropertyValue::Struct(StructValue::Rotator([0, -3072, 16384]))
    );

    let mut t = tag(PROP, 0x2a, Some(COLOR), &[], &[]);
    t.extend([1, 2, 3, 4]);
    assert_eq!(
        single_value(t),
        PropertyValue::Struct(StructValue::Color([1, 2, 3, 4]))
    );

    let mut t = tag(PROP, 0x5a, Some(SCALE), &[17], &[]);
    t.extend(f32s(&[1.0, 1.0, 1.0, 0.0]));
    t.push(5);
    assert_eq!(
        single_value(t),
        PropertyValue::Struct(StructValue::Scale {
            scale: [1.0; 3],
            sheer_rate: 0.0,
            sheer_axis: 5
        })
    );

    let mut t = tag(PROP, 0x5a, Some(POINT_REGION), &[6], &[]);
    t.extend(compact(1));
    t.extend(i32s(&[25]));
    t.push(1);
    assert_eq!(
        single_value(t),
        PropertyValue::Struct(StructValue::PointRegion {
            zone: ObjectRef::Export(0),
            leaf: 25,
            zone_number: 1
        })
    );

    // Vector/Rotator property types (11/12) share the struct layouts.
    let mut t = tag(PROP, 0x3b, None, &[], &[]);
    t.extend(f32s(&[4.0, 5.0, 6.0]));
    assert_eq!(
        single_value(t),
        PropertyValue::Struct(StructValue::Vector([4.0, 5.0, 6.0]))
    );
    let mut t = tag(PROP, 0x3c, None, &[], &[]);
    t.extend(i32s(&[1, 2, 3]));
    assert_eq!(
        single_value(t),
        PropertyValue::Struct(StructValue::Rotator([1, 2, 3]))
    );
}

#[test]
fn nested_struct_spans_are_bounded() {
    // Box contains two vectors and a byte; the next property must start right after it.
    let mut payload = tag(PROP, 0x5a, Some(BOX), &[25], &[]);
    let box_value_at = payload.len();
    payload.extend(f32s(&[-1.0, -2.0, -3.0, 1.0, 2.0, 3.0]));
    payload.push(1);
    // Unknown struct with an i32 size: raw span, then a following int.
    let unknown_tag_at = payload.len();
    payload.extend(tag(PROP, 0x7a, Some(MY_STRUCT), &i32s(&[3]), &[]));
    let unknown_value_at = payload.len();
    payload.extend([9, 9, 9]);
    payload.extend(tag(OTHER, 0x22, None, &[], &[]));
    payload.extend(i32s(&[42]));
    payload.extend(compact(NONE));
    let total = payload.len();
    let (p, o) = ok_props(payload);
    let base = p.exports()[0].serial_offset as usize;
    let props = &o.block.properties;
    assert_eq!(props.len(), 3);
    assert_eq!(
        props[0].value,
        PropertyValue::Struct(StructValue::Box {
            min: [-1.0, -2.0, -3.0],
            max: [1.0, 2.0, 3.0],
            valid: 1
        })
    );
    assert_eq!(props[0].value_span.start, base + box_value_at);
    assert_eq!(props[0].value_span.len(), 25);
    assert_eq!(props[1].tag_span.start, base + unknown_tag_at);
    assert_eq!(props[1].value_span.start, base + unknown_value_at);
    assert_eq!(props[1].value_span.len(), 3);
    assert_eq!(props[1].value, PropertyValue::Raw(RawReason::UnknownStruct));
    assert!(
        props[1].anomaly().is_none(),
        "unknown struct is not an anomaly"
    );
    assert_eq!(props[2].value, PropertyValue::Int(42));
    assert_eq!(o.block.span.end, base + total);
    for w in props.windows(2) {
        assert_eq!(w[0].value_span.end, w[1].tag_span.start);
    }
}

#[test]
fn known_struct_with_wrong_size_stays_raw() {
    let mut t = tag(PROP, 0x2a, Some(VECTOR), &[], &[]);
    t.extend([0; 4]);
    let (_, prop) = single(t);
    assert_eq!(
        prop.value,
        PropertyValue::Raw(RawReason::SizeMismatch { expected: 12 })
    );
    assert!(prop.anomaly().is_some());
}

#[test]
fn arrays_maps_and_fixed_arrays_keep_spans() {
    let mut t = tag(PROP, 0x59, None, &[9], &[]);
    t.extend(compact(2));
    t.extend(i32s(&[10, 20]));
    let (p, prop) = single(t);
    let base = p.exports()[0].serial_offset as usize;
    match prop.value {
        PropertyValue::Array { count, elements } => {
            assert_eq!(count, 2);
            assert_eq!(elements.len(), 8);
            assert_eq!(elements.start, prop.value_span.start + 1);
            assert_eq!(elements.end, prop.value_span.end);
            assert!(elements.start > base);
        }
        v => panic!("{v:?}"),
    }
    for info in [0x2e, 0x2f] {
        let mut t = tag(PROP, info, None, &[], &[]);
        t.extend([1, 2, 3, 4]);
        assert_eq!(
            single_value(t),
            PropertyValue::Raw(RawReason::UnsupportedType)
        );
    }
    // Negative array count is invalid but bounded.
    let mut t = tag(PROP, 0x09, None, &[], &[]);
    t.extend(compact(-1));
    assert!(matches!(
        single_value(t),
        PropertyValue::Raw(RawReason::Invalid(ErrorKind::CountOutOfRange { .. }))
    ));
}

// ---------------------------------------------------------------------------------------------
// Size, array-index and bool encodings
// ---------------------------------------------------------------------------------------------

#[test]
fn size_codes_select_implicit_and_explicit_sizes() {
    // Codes 0..=4 are implicit 1, 2, 4, 12, 16 (unknown struct keeps exactly that many bytes).
    for (code, size) in [(0u8, 1usize), (1, 2), (2, 4), (3, 12), (4, 16)] {
        let mut t = tag(PROP, 0x0a | (code << 4), Some(MY_STRUCT), &[], &[]);
        t.extend(vec![0xee; size]);
        let (_, prop) = single(t);
        assert_eq!((prop.size as usize, prop.value_span.len()), (size, size));
    }
    // 5: u8, 6: u16, 7: i32.
    for (code, size_bytes, size) in [
        (5u8, vec![200u8], 200usize),
        (6, 300u16.to_le_bytes().to_vec(), 300),
        (7, 70_000i32.to_le_bytes().to_vec(), 70_000),
    ] {
        let mut t = tag(PROP, 0x0a | (code << 4), Some(MY_STRUCT), &size_bytes, &[]);
        t.extend(vec![0xee; size]);
        let (_, prop) = single(t);
        assert_eq!((prop.size as usize, prop.value_span.len()), (size, size));
    }
}

#[test]
fn array_index_encodings() {
    for (index_bytes, expected) in [
        (vec![5u8], 5u32),
        (vec![0x7f], 127),
        (vec![0x81, 0x2c], 300),
        (vec![0xbf, 0xff], 0x3fff),
        (vec![0xc0, 0x01, 0x86, 0xa0], 100_000),
        (vec![0xff, 0xff, 0xff, 0xff], 0x3fff_ffff),
    ] {
        let mut t = tag(PROP, 0xa2, None, &[], &index_bytes);
        t.extend(i32s(&[1]));
        let (_, prop) = single(t);
        assert_eq!(prop.array_index, expected, "{index_bytes:02x?}");
        assert_eq!(prop.value, PropertyValue::Int(1));
    }
    // The index follows the struct name and explicit size.
    let mut t = tag(PROP, 0xda, Some(MY_STRUCT), &[2], &[3]);
    t.extend([0, 0]);
    let (_, prop) = single(t);
    assert_eq!((prop.array_index, prop.size), (3, 2));
}

#[test]
fn bool_value_lives_in_the_info_bit() {
    // Corpus form: size code 5 with an explicit size byte of 0; bit 7 is the value, no index.
    let (_, prop) = single(tag(PROP, 0xd3, None, &[0], &[]));
    assert_eq!(
        (prop.value.clone(), prop.size, prop.array_index),
        (PropertyValue::Bool(true), 0, 0)
    );
    assert!(prop.value_span.is_empty());
    let (_, prop) = single(tag(PROP, 0x53, None, &[0], &[]));
    assert_eq!(prop.value, PropertyValue::Bool(false));
    // Size code 0 (nominal 1): still no value bytes; the next byte is the next tag.
    let mut payload = tag(PROP, 0x83, None, &[], &[]);
    payload.extend(tag(OTHER, 0x03, None, &[], &[]));
    payload.extend(compact(NONE));
    let (_, o) = ok_props(payload);
    let values: Vec<_> = o.block.properties.iter().map(|q| q.value.clone()).collect();
    assert_eq!(
        values,
        [PropertyValue::Bool(true), PropertyValue::Bool(false)]
    );
}

// ---------------------------------------------------------------------------------------------
// State frame, terminator, tail
// ---------------------------------------------------------------------------------------------

#[test]
fn state_frame_with_node_has_offset() {
    let mut payload = compact(-3); // node: Engine.StaticMesh class import
    payload.extend(compact(-3)); // state node
    payload.extend(u64::MAX.to_le_bytes());
    payload.extend(0x0033_0030u32.to_le_bytes());
    payload.extend(compact(-1)); // offset
    let frame_len = payload.len();
    payload.extend(tag(PROP, 0x22, None, &[], &[]));
    payload.extend(i32s(&[3]));
    payload.extend(compact(NONE));
    let block_end = payload.len();
    payload.extend([0xaa; 7]); // native tail
    let (p, r) = read_with(payload.clone(), RF_HAS_STACK | 1, &Limits::default());
    let o = r.unwrap();
    let base = p.exports()[0].serial_offset as usize;
    let sf = o.state_frame.as_ref().unwrap();
    assert_eq!(sf.node, ObjectRef::Import(2));
    assert_eq!(sf.state_node, ObjectRef::Import(2));
    assert_eq!(sf.probe_mask, u64::MAX);
    assert_eq!(sf.latent_action, 0x0033_0030);
    assert_eq!(sf.offset, Some(-1));
    assert_eq!(
        sf.span,
        Span {
            start: base,
            end: base + frame_len
        }
    );
    assert_eq!(o.block.properties.len(), 1);
    assert_eq!(o.consumed(), block_end);
    assert_eq!(
        o.tail(),
        Span {
            start: base + block_end,
            end: base + payload.len()
        }
    );
    // Without the flag the same bytes are read as tags and fail (no guessing).
    let (_, r) = read_with(payload, 0, &Limits::default());
    assert!(r.is_err());
}

#[test]
fn state_frame_without_node_has_no_offset() {
    let mut payload = vec![0, 0];
    payload.extend(7u64.to_le_bytes());
    payload.extend(0u32.to_le_bytes());
    payload.extend(compact(NONE));
    let (_, r) = read_with(payload, RF_HAS_STACK, &Limits::default());
    let o = r.unwrap();
    let sf = o.state_frame.clone().unwrap();
    assert_eq!(
        (sf.node, sf.offset, sf.span.len()),
        (ObjectRef::Null, None, 14)
    );
    assert_eq!(o.consumed(), 15);
}

#[test]
fn state_frame_errors_are_contextual() {
    let mut payload = compact(-3);
    payload.extend(compact(-3));
    payload.extend([0xff; 5]); // truncated probe mask
    let (p, r) = read_with(payload, RF_HAS_STACK, &Limits::default());
    let err = r.unwrap_err();
    assert_eq!(err.field, Some("state_frame.probe_mask"));
    assert_eq!((err.table, err.index), (Some(Table::Payload), Some(0)));
    let base = p.exports()[0].serial_offset as u64;
    assert_eq!(err.offset, Some(base + 2));

    let (_, r) = read_with(compact(99), RF_HAS_STACK, &Limits::default());
    let err = r.unwrap_err();
    assert!(matches!(
        err.kind,
        ErrorKind::ObjectRefOutOfRange { raw: 99, .. }
    ));
    assert_eq!(err.field, Some("state_frame.node"));
}

#[test]
fn duplicate_none_entries_terminate() {
    let mut payload = tag(PROP, 0x22, None, &[], &[]);
    payload.extend(i32s(&[1]));
    payload.extend(compact(NONE_DUP));
    payload.push(0x55); // tail
    let (_, o) = ok_props(payload);
    assert_eq!(o.block.terminator.index(), NONE_DUP as u32);
    assert_eq!(o.tail().len(), 1);
}

#[test]
fn empty_block_is_just_none() {
    let (_, o) = ok_props(compact(NONE));
    assert!(o.block.properties.is_empty());
    assert_eq!((o.consumed(), o.tail().len()), (1, 0));
}

// ---------------------------------------------------------------------------------------------
// Malformed blocks
// ---------------------------------------------------------------------------------------------

fn err_of(payload: Vec<u8>) -> (Package, PackageError) {
    let (p, r) = read(payload);
    (p, r.expect_err("expected a property error"))
}

#[test]
fn missing_none_terminator() {
    let mut payload = tag(PROP, 0x22, None, &[], &[]);
    payload.extend(i32s(&[1]));
    let len = payload.len() as u64;
    let (p, err) = err_of(payload);
    assert_eq!(err.kind, ErrorKind::MissingPropertyTerminator);
    assert_eq!(
        err.offset,
        Some(u64::from(p.exports()[0].serial_offset) + len)
    );
    assert_eq!((err.table, err.index), (Some(Table::Payload), Some(0)));
    let text = err.to_string();
    assert!(text.starts_with("payload[0] at offset"), "{text}");
}

#[test]
fn truncated_tags() {
    // Name only.
    let (_, err) = err_of(compact(PROP));
    assert!(matches!(err.kind, ErrorKind::UnexpectedEof { .. }));
    assert_eq!(err.field, Some("property.info"));
    // Truncated explicit size.
    let (_, err) = err_of(tag(PROP, 0x7a, Some(MY_STRUCT), &[1, 0], &[]));
    assert_eq!(err.field, Some("property.size"));
    // Truncated two-byte array index.
    let (_, err) = err_of(tag(PROP, 0xa2, None, &[], &[0x81]));
    assert_eq!(err.field, Some("property.array_index"));
    // Value shorter than declared.
    let mut t = tag(PROP, 0x22, None, &[], &[]);
    t.extend([1, 2]);
    let (_, err) = err_of(t);
    assert_eq!(
        err.kind,
        ErrorKind::PropertySizeOutOfRange {
            size: 4,
            available: 2
        }
    );
    // Truncated compact name.
    let (_, err) = err_of(vec![0x40]);
    assert_eq!(err.field, Some("property.name"));
}

#[test]
fn oversized_and_negative_sizes() {
    let mut t = tag(PROP, 0x7a, Some(MY_STRUCT), &i32s(&[1_000_000]), &[]);
    t.extend([0; 16]);
    t.extend(compact(NONE));
    let (_, err) = err_of(t);
    assert!(matches!(
        err.kind,
        ErrorKind::PropertySizeOutOfRange {
            size: 1_000_000,
            ..
        }
    ));
    assert_eq!(err.field, Some("property.size"));
    let (_, err) = err_of(tag(PROP, 0x7a, Some(MY_STRUCT), &i32s(&[-1]), &[]));
    assert!(matches!(
        err.kind,
        ErrorKind::PropertySizeOutOfRange { size: -1, .. }
    ));
}

#[test]
fn bad_name_indices() {
    let (_, err) = err_of(compact(99));
    assert_eq!(
        err.kind,
        ErrorKind::NameIndexOutOfRange {
            index: 99,
            count: 20
        }
    );
    assert_eq!(err.field, Some("property.name"));
    let (_, err) = err_of(compact(-1));
    assert!(matches!(
        err.kind,
        ErrorKind::NameIndexOutOfRange { index: -1, .. }
    ));
    let (_, err) = err_of(tag(PROP, 0x3a, Some(99), &[], &[]));
    assert_eq!(err.field, Some("property.struct_name"));
}

#[test]
fn invalid_type_and_property_limit() {
    let (_, err) = err_of(tag(PROP, 0x00, None, &[], &[]));
    assert_eq!(err.kind, ErrorKind::InvalidPropertyType { code: 0 });

    let mut payload = Vec::new();
    for _ in 0..3 {
        payload.extend(tag(PROP, 0x53, None, &[0], &[]));
    }
    payload.extend(compact(NONE));
    let limits = Limits {
        max_properties: 3,
        ..Limits::default()
    };
    let (_, r) = read_with(payload.clone(), 0, &limits);
    assert_eq!(r.unwrap_err().kind, ErrorKind::TooManyProperties { max: 3 });
    let limits = Limits {
        max_properties: 4,
        ..Limits::default()
    };
    assert!(read_with(payload, 0, &limits).1.is_ok());
}

#[test]
fn bad_values_are_bounded_raw_and_decoding_continues() {
    let mut payload = Vec::new();
    // Int declared with 2 bytes.
    payload.extend(tag(PROP, 0x12, None, &[], &[]));
    payload.extend([1, 2]);
    // Object reference out of range.
    payload.extend(tag(PROP, 0x05, None, &[], &[]));
    payload.extend(compact(50));
    // Name value out of range.
    payload.extend(tag(PROP, 0x06, None, &[], &[]));
    payload.extend(compact(60));
    // String followed by stray bytes inside its declared size.
    let s = latin1("ab");
    payload.extend(tag(PROP, 0x5d, None, &[s.len() as u8 + 2], &[]));
    payload.extend(&s);
    payload.extend([0, 0]);
    // String whose length exceeds its value.
    payload.extend(tag(PROP, 0x1d, None, &[], &[]));
    payload.extend([0x05, 0x41]);
    payload.extend(tag(OTHER, 0x22, None, &[], &[]));
    payload.extend(i32s(&[5]));
    payload.extend(compact(NONE));
    let (_, o) = ok_props(payload);
    let v: Vec<_> = o.block.properties.iter().map(|q| q.value.clone()).collect();
    assert_eq!(
        v[0],
        PropertyValue::Raw(RawReason::SizeMismatch { expected: 4 })
    );
    assert!(matches!(
        v[1],
        PropertyValue::Raw(RawReason::Invalid(ErrorKind::ObjectRefOutOfRange {
            raw: 50,
            ..
        }))
    ));
    assert!(matches!(
        v[2],
        PropertyValue::Raw(RawReason::Invalid(ErrorKind::NameIndexOutOfRange {
            index: 60,
            ..
        }))
    ));
    assert_eq!(
        v[3],
        PropertyValue::Raw(RawReason::TrailingBytes { consumed: 4 })
    );
    assert!(matches!(v[4], PropertyValue::Raw(RawReason::Invalid(_))));
    assert_eq!(v[5], PropertyValue::Int(5));
    assert_eq!(
        o.block
            .properties
            .iter()
            .filter(|q| q.anomaly().is_some())
            .count(),
        5
    );
}

// ---------------------------------------------------------------------------------------------
// Payload access
// ---------------------------------------------------------------------------------------------

#[test]
fn payload_slicing_is_bounded_and_checked() {
    let payload = compact(NONE);
    let (bytes, p) = fixture(payload.clone(), 0);
    assert_eq!(p.export_payload(&bytes, 0).unwrap(), payload.as_slice());
    let err = p.export_payload(&bytes, 1).unwrap_err();
    assert_eq!(err.kind, ErrorKind::EmptyPayload);
    assert_eq!((err.table, err.index), (Some(Table::Payload), Some(1)));
    assert!(matches!(
        p.export_payload(&bytes, 2).unwrap_err().kind,
        ErrorKind::ExportIndexOutOfRange { index: 2, count: 2 }
    ));
    assert!(matches!(
        p.export_payload(&bytes[1..], 0).unwrap_err().kind,
        ErrorKind::BufferLengthMismatch { .. }
    ));
    assert!(
        p.read_object_properties(&bytes, 1, &Limits::default())
            .is_err()
    );
}

#[test]
fn explicit_block_range() {
    let mut payload = vec![0xde, 0xad]; // native prefix
    payload.extend(tag(PROP, 0x22, None, &[], &[]));
    payload.extend(i32s(&[9]));
    payload.extend(compact(NONE));
    let (bytes, p) = fixture(payload.clone(), 0);
    let start = p.exports()[0].serial_offset as usize;
    let end = start + payload.len();
    let block = p
        .read_property_block(&bytes, start + 2, end, &Limits::default())
        .unwrap();
    assert_eq!(block.properties[0].value, PropertyValue::Int(9));
    assert_eq!(
        block.span,
        Span {
            start: start + 2,
            end
        }
    );
    // The end bound is respected: cutting the terminator off fails.
    let err = p
        .read_property_block(&bytes, start + 2, end - 1, &Limits::default())
        .unwrap_err();
    assert_eq!(err.kind, ErrorKind::MissingPropertyTerminator);
    assert_eq!(err.offset, Some((end - 1) as u64));
    assert!(
        p.read_property_block(&bytes, end, start, &Limits::default())
            .is_err()
    );
    assert!(
        p.read_property_block(&bytes, 0, bytes.len() + 1, &Limits::default())
            .is_err()
    );
}

#[test]
fn mutated_payloads_never_panic() {
    let mut payload = compact(-3);
    payload.extend(compact(-3));
    payload.extend(u64::MAX.to_le_bytes());
    payload.extend(0u32.to_le_bytes());
    payload.extend(compact(-1));
    payload.extend(tag(PROP, 0x5a, Some(BOX), &[25], &[]));
    payload.extend(vec![0; 25]);
    payload.extend(tag(PROP, 0x5d, None, &[4], &[]));
    payload.extend(latin1("ab"));
    payload.push(0);
    payload.extend(tag(PROP, 0xa2, None, &[], &[0x81, 0x2c]));
    payload.extend(i32s(&[1]));
    payload.extend(tag(PROP, 0x59, None, &[5], &[]));
    payload.extend(compact(1));
    payload.extend(i32s(&[1]));
    payload.extend(compact(NONE));
    let mut state = 0x9e37_79b9_7f4a_7c15u64;
    let mut next = || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    for pos in 0..payload.len() {
        for mask in [0x01u8, 0x40, 0x80, 0xff] {
            let mut m = payload.clone();
            m[pos] ^= mask;
            let _ = read_with(m, RF_HAS_STACK, &Limits::default());
        }
    }
    for _ in 0..3000 {
        let mut m = payload.clone();
        for _ in 0..(next() % 4 + 1) {
            let pos = (next() as usize) % m.len();
            m[pos] = next() as u8;
        }
        m.truncate((next() as usize) % (m.len() + 1));
        let _ = read_with(m.clone(), RF_HAS_STACK, &Limits::default());
        let _ = read_with(m, 0, &Limits::default());
    }
}
