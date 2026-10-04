//! Text and JSON rendering of decoded export properties (`xiii-tool props`).
//!
//! Output can contain package content (strings, names, numbers); it is meant for local
//! inspection and must not be saved into the repository.

use std::fmt::Write as _;

use serde_json::{Value, json};
use xiii_package::{
    Limits, ObjectProperties, ObjectRef, Package, PackageError, Property, PropertyValue,
    RawReason, StructValue,
};

/// `Class'Path'` for a reference (`None` for null). Imports start with their root package.
pub fn ref_text(p: &Package, r: ObjectRef) -> String {
    match r {
        ObjectRef::Null => "None".to_owned(),
        ObjectRef::Import(i) => {
            let class = p.imports().get(i as usize).map_or("?", |o| p.name(o.class_name));
            format!("{class}'{}'", p.object_path(r).unwrap_or("?"))
        }
        ObjectRef::Export(i) => {
            let class = p.export_class_path(i as usize).unwrap_or("?");
            let class = class.rsplit('.').next().unwrap_or(class);
            format!("{class}'{}' (export {i})", p.object_path(r).unwrap_or("?"))
        }
    }
}

fn ref_json(p: &Package, r: ObjectRef) -> Value {
    match r {
        ObjectRef::Null => Value::Null,
        ObjectRef::Import(i) => json!({
            "import": i,
            "path": p.object_path(r),
            "class": p.imports().get(i as usize).map(|o| p.name(o.class_name)),
        }),
        ObjectRef::Export(i) => json!({
            "export": i,
            "path": p.object_path(r),
            "class": p.export_class_path(i as usize),
        }),
    }
}

/// Human-readable raw reason.
pub fn raw_reason_text(r: &RawReason) -> String {
    match r {
        RawReason::UnsupportedType => "unsupported type".to_owned(),
        RawReason::UnknownStruct => "unknown struct layout".to_owned(),
        RawReason::SizeMismatch { expected } => format!("size mismatch (expected {expected})"),
        RawReason::TrailingBytes { consumed } => {
            format!("value used {consumed} bytes, trailing bytes remain")
        }
        RawReason::Invalid(kind) => format!("invalid: {kind}"),
    }
}

/// Short stable identifier for a raw reason, used in aggregate reports.
pub fn raw_reason_key(r: &RawReason) -> String {
    match r {
        RawReason::UnsupportedType => "unsupported_type".to_owned(),
        RawReason::UnknownStruct => "unknown_struct".to_owned(),
        RawReason::SizeMismatch { expected } => format!("size_mismatch(expected {expected})"),
        RawReason::TrailingBytes { .. } => "trailing_bytes".to_owned(),
        RawReason::Invalid(kind) => format!("invalid({})", error_kind_key(kind)),
    }
}

/// Variant name of an error kind (e.g. `MissingPropertyTerminator`).
pub fn error_kind_key(kind: &xiii_package::ErrorKind) -> String {
    let d = format!("{kind:?}");
    d.split([' ', '{', '(']).next().unwrap_or("").to_owned()
}

fn struct_text(s: &StructValue, p: &Package) -> String {
    match s {
        StructValue::Vector(v) => format!("(X={},Y={},Z={})", v[0], v[1], v[2]),
        StructValue::Rotator(r) => format!("(Pitch={},Yaw={},Roll={})", r[0], r[1], r[2]),
        StructValue::Color(c) => format!("(bytes={},{},{},{})", c[0], c[1], c[2], c[3]),
        StructValue::Scale {
            scale,
            sheer_rate,
            sheer_axis,
        } => format!(
            "(Scale=(X={},Y={},Z={}),SheerRate={sheer_rate},SheerAxis={sheer_axis})",
            scale[0], scale[1], scale[2]
        ),
        StructValue::Plane(v) | StructValue::Sphere(v) => {
            format!("(X={},Y={},Z={},W={})", v[0], v[1], v[2], v[3])
        }
        StructValue::Box { min, max, valid } => format!(
            "(Min=({},{},{}),Max=({},{},{}),IsValid={valid})",
            min[0], min[1], min[2], max[0], max[1], max[2]
        ),
        StructValue::Range(r) => format!("(Min={},Max={})", r[0], r[1]),
        StructValue::RangeVector(r) => format!(
            "(X=({},{}),Y=({},{}),Z=({},{}))",
            r[0][0], r[0][1], r[1][0], r[1][1], r[2][0], r[2][1]
        ),
        StructValue::Guid(g) => format!("{:08X}-{:08X}-{:08X}-{:08X}", g[0], g[1], g[2], g[3]),
        StructValue::PointRegion {
            zone,
            leaf,
            zone_number,
        } => format!(
            "(Zone={},iLeaf={leaf},ZoneNumber={zone_number})",
            ref_text(p, *zone)
        ),
    }
}

fn struct_json(s: &StructValue, p: &Package) -> Value {
    match s {
        StructValue::Vector(v) => json!({"x": v[0], "y": v[1], "z": v[2]}),
        StructValue::Rotator(r) => json!({"pitch": r[0], "yaw": r[1], "roll": r[2]}),
        StructValue::Color(c) => json!({"bytes": c}),
        StructValue::Scale {
            scale,
            sheer_rate,
            sheer_axis,
        } => json!({"scale": scale, "sheer_rate": sheer_rate, "sheer_axis": sheer_axis}),
        StructValue::Plane(v) | StructValue::Sphere(v) => json!(v),
        StructValue::Box { min, max, valid } => json!({"min": min, "max": max, "valid": valid}),
        StructValue::Range(r) => json!({"min": r[0], "max": r[1]}),
        StructValue::RangeVector(r) => json!(r),
        StructValue::Guid(g) => json!(g),
        StructValue::PointRegion {
            zone,
            leaf,
            zone_number,
        } => json!({"zone": ref_json(p, *zone), "leaf": leaf, "zone_number": zone_number}),
    }
}

/// Value as text.
pub fn value_text(p: &Package, prop: &Property) -> String {
    match &prop.value {
        PropertyValue::Byte(b) => b.to_string(),
        PropertyValue::Int(i) => i.to_string(),
        PropertyValue::Bool(b) => b.to_string(),
        PropertyValue::Float(f) => f.to_string(),
        PropertyValue::Object(r) | PropertyValue::Class(r) => ref_text(p, *r),
        PropertyValue::Name(n) => format!("'{}'", p.name(*n)),
        PropertyValue::Str(s) => format!("{s:?}"),
        PropertyValue::Delegate { object, function } => {
            format!("{}.{}", ref_text(p, *object), p.name(*function))
        }
        PropertyValue::Struct(s) => struct_text(s, p),
        PropertyValue::Array { count, elements } => {
            format!("array count={count} element bytes={}", elements.len())
        }
        PropertyValue::Raw(r) => format!(
            "<raw {} bytes: {}>",
            prop.value_span.len(),
            raw_reason_text(r)
        ),
    }
}

fn value_json(p: &Package, prop: &Property) -> Value {
    match &prop.value {
        PropertyValue::Byte(b) => json!(b),
        PropertyValue::Int(i) => json!(i),
        PropertyValue::Bool(b) => json!(b),
        PropertyValue::Float(f) => json!(f),
        PropertyValue::Object(r) | PropertyValue::Class(r) => ref_json(p, *r),
        PropertyValue::Name(n) => json!(p.name(*n)),
        PropertyValue::Str(s) => json!(s),
        PropertyValue::Delegate { object, function } => {
            json!({"object": ref_json(p, *object), "function": p.name(*function)})
        }
        PropertyValue::Struct(s) => struct_json(s, p),
        PropertyValue::Array { count, elements } => {
            json!({"array_count": count, "element_span": [elements.start, elements.end]})
        }
        PropertyValue::Raw(r) => json!({"raw_bytes": prop.value_span.len(), "reason": raw_reason_text(r)}),
    }
}

fn type_label(p: &Package, prop: &Property) -> String {
    match prop.struct_name {
        Some(s) => format!("Struct<{}>", p.name(s)),
        None => prop.kind.name().trim_end_matches("Property").to_owned(),
    }
}

/// Finds an export by zero-based index or by (case-insensitive) path.
pub fn find_export(p: &Package, selector: &str) -> Option<usize> {
    if let Ok(i) = selector.parse::<usize>() {
        return (i < p.exports().len()).then_some(i);
    }
    (0..p.exports().len()).find(|&i| {
        p.object_path(ObjectRef::Export(i as u32))
            .is_some_and(|path| path.eq_ignore_ascii_case(selector))
    })
}

/// Decodes one export; zero-sized exports yield `None`.
pub fn decode(
    p: &Package,
    data: &[u8],
    export: usize,
) -> Option<Result<ObjectProperties, PackageError>> {
    (p.exports()[export].serial_size > 0)
        .then(|| p.read_object_properties(data, export, &Limits::default()))
}

/// Plain-text dump for the selected exports.
pub fn props_text(p: &Package, data: &[u8], exports: &[usize]) -> String {
    let mut out = String::new();
    for &i in exports {
        let e = &p.exports()[i];
        let _ = writeln!(
            out,
            "[{i}] {} {} (payload {} bytes at {})",
            p.export_class_path(i).unwrap_or("?"),
            p.object_path(ObjectRef::Export(i as u32)).unwrap_or("?"),
            e.serial_size,
            e.serial_offset
        );
        match decode(p, data, i) {
            None => {
                let _ = writeln!(out, "  (no payload)");
            }
            Some(Err(err)) => {
                let _ = writeln!(out, "  ERROR {err}");
            }
            Some(Ok(o)) => {
                if let Some(sf) = &o.state_frame {
                    let _ = writeln!(
                        out,
                        "  state frame: node {} state {} probe 0x{:016x} latent 0x{:08x} offset {:?} ({} bytes)",
                        ref_text(p, sf.node),
                        ref_text(p, sf.state_node),
                        sf.probe_mask,
                        sf.latent_action,
                        sf.offset,
                        sf.span.len()
                    );
                }
                for prop in &o.block.properties {
                    let idx = if prop.array_index != 0 {
                        format!("[{}]", prop.array_index)
                    } else {
                        String::new()
                    };
                    let _ = writeln!(
                        out,
                        "  {}{idx} {} = {}",
                        p.property_name(prop),
                        type_label(p, prop),
                        value_text(p, prop)
                    );
                }
                let _ = writeln!(
                    out,
                    "  properties {} consumed {} of {} bytes; native tail {} bytes",
                    o.block.properties.len(),
                    o.consumed(),
                    o.payload.len(),
                    o.tail().len()
                );
            }
        }
    }
    out
}

/// JSON dump for the selected exports.
pub fn props_json(p: &Package, data: &[u8], exports: &[usize]) -> Value {
    let list: Vec<Value> = exports
        .iter()
        .map(|&i| {
            let e = &p.exports()[i];
            let mut v = json!({
                "index": i,
                "path": p.object_path(ObjectRef::Export(i as u32)),
                "class": p.export_class_path(i),
                "flags": e.flags,
                "serial_offset": e.serial_offset,
                "serial_size": e.serial_size,
            });
            let obj = v.as_object_mut().expect("object");
            match decode(p, data, i) {
                None => {}
                Some(Err(err)) => {
                    obj.insert("error".into(), json!(err.to_string()));
                }
                Some(Ok(o)) => {
                    if let Some(sf) = &o.state_frame {
                        obj.insert(
                            "state_frame".into(),
                            json!({
                                "node": ref_json(p, sf.node),
                                "state_node": ref_json(p, sf.state_node),
                                "probe_mask": sf.probe_mask,
                                "latent_action": sf.latent_action,
                                "offset": sf.offset,
                                "bytes": sf.span.len(),
                            }),
                        );
                    }
                    let props: Vec<Value> = o
                        .block
                        .properties
                        .iter()
                        .map(|prop| {
                            json!({
                                "name": p.property_name(prop),
                                "type": prop.kind.name(),
                                "struct": prop.struct_name.map(|s| p.name(s)),
                                "size": prop.size,
                                "array_index": prop.array_index,
                                "offset": prop.tag_span.start,
                                "value": value_json(p, prop),
                            })
                        })
                        .collect();
                    obj.insert("properties".into(), Value::Array(props));
                    obj.insert("consumed".into(), json!(o.consumed()));
                    obj.insert("tail".into(), json!(o.tail().len()));
                }
            }
            v
        })
        .collect();
    json!({ "exports": list })
}
