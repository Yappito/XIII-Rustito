//! Human-readable and JSON reports for one parsed package.

use std::fmt::Write as _;

use serde_json::{Map, Value, json};
use xiii_package::{ObjectRef, Package};

/// Keys produced by the Python research probe (`tools/probe_install.py`) for each package,
/// excluding `exports_metadata`. These are compared field-by-field against inventories.
pub const PROBE_KEYS: [&str; 11] = [
    "version",
    "licensee_version",
    "package_flags",
    "names",
    "imports",
    "exports",
    "table_spans",
    "table_validation",
    "export_classes",
    "zero_size_exports",
    "imported_packages",
];

fn span(s: xiii_package::Span) -> Value {
    json!([s.start, s.end])
}

/// Probe-compatible package object (same keys and value shapes as the Python inventory).
pub fn probe_json(p: &Package) -> Value {
    let s = p.summary();
    let spans = p.table_spans();
    json!({
        "version": s.version,
        "licensee_version": s.licensee,
        "package_flags": s.package_flags,
        "names": s.names.count,
        "imports": s.imports.count,
        "exports": s.exports.count,
        "table_spans": {
            "names": span(spans.names),
            "imports": span(spans.imports),
            "exports": span(spans.exports),
        },
        "table_validation": "passed",
        "export_classes": p.export_class_counts(),
        "zero_size_exports": p.zero_size_export_counts(),
        "imported_packages": p.imported_packages(),
    })
}

/// Full JSON report: probe-compatible fields plus summary details the probe did not read.
pub fn package_json(p: &Package, include_exports: bool) -> Value {
    let s = p.summary();
    let mut v = probe_json(p);
    let obj = v.as_object_mut().expect("probe_json returns an object");
    let unaccounted: Vec<Value> = p.unaccounted_ranges().into_iter().map(span).collect();
    let overlaps: Vec<Value> = p.overlapping_ranges().into_iter().map(span).collect();
    obj.insert("file_bytes".into(), json!(p.file_len()));
    obj.insert("guid".into(), json!(s.guid_string()));
    obj.insert(
        "generations".into(),
        Value::Array(
            s.generations
                .iter()
                .map(|g| json!({"exports": g.export_count, "names": g.name_count}))
                .collect(),
        ),
    );
    obj.insert(
        "latest_generation_matches_tables".into(),
        json!(s.latest_generation_matches_tables()),
    );
    obj.insert("header_end".into(), json!(s.header_end));
    obj.insert("header_gap".into(), json!(s.header_gap()));
    obj.insert(
        "table_offsets".into(),
        json!({"names": s.names.offset, "imports": s.imports.offset, "exports": s.exports.offset}),
    );
    obj.insert("unaccounted_ranges".into(), Value::Array(unaccounted));
    obj.insert("overlapping_ranges".into(), Value::Array(overlaps));
    if include_exports {
        obj.insert("exports_metadata".into(), Value::Array(exports_json(p)));
    }
    v
}

/// One JSON object per export (metadata only, never payload bytes).
pub fn exports_json(p: &Package) -> Vec<Value> {
    p.exports()
        .iter()
        .enumerate()
        .map(|(i, e)| {
            let mut m = Map::new();
            m.insert("index".into(), json!(i));
            m.insert(
                "path".into(),
                json!(p.object_path(ObjectRef::Export(i as u32))),
            );
            m.insert("class".into(), json!(p.export_class_path(i)));
            m.insert("class_ref".into(), json!(e.class.raw()));
            m.insert("super_ref".into(), json!(e.super_ref.raw()));
            m.insert("outer".into(), json!(e.outer.raw()));
            m.insert("flags".into(), json!(e.flags));
            m.insert("serial_offset".into(), json!(e.serial_offset));
            m.insert("serial_size".into(), json!(e.serial_size));
            Value::Object(m)
        })
        .collect()
}

/// Plain-text report.
pub fn package_text(label: &str, p: &Package, include_exports: bool) -> String {
    let s = p.summary();
    let spans = p.table_spans();
    let mut out = String::new();
    let _ = writeln!(out, "package: {label} ({} bytes)", p.file_len());
    let _ = writeln!(
        out,
        "version {} licensee {} flags 0x{:08x}",
        s.version, s.licensee, s.package_flags
    );
    let _ = writeln!(out, "guid {}", s.guid_string());
    let _ = writeln!(
        out,
        "generations {} (latest matches tables: {})",
        s.generations.len(),
        match s.latest_generation_matches_tables() {
            Some(true) => "yes",
            Some(false) => "NO",
            None => "n/a",
        }
    );
    for (i, g) in s.generations.iter().enumerate() {
        let _ = writeln!(
            out,
            "  [{i}] exports {} names {}",
            g.export_count, g.name_count
        );
    }
    let _ = writeln!(
        out,
        "summary ends at {}; first table at {}; gap {} bytes",
        s.header_end,
        s.first_table_offset(),
        s.header_gap()
    );
    for (label, loc, sp) in [
        ("names", s.names, spans.names),
        ("imports", s.imports, spans.imports),
        ("exports", s.exports, spans.exports),
    ] {
        let _ = writeln!(
            out,
            "{label:<8} {:>7} entries  bytes [{}, {})",
            loc.count, sp.start, sp.end
        );
    }
    let unaccounted = p.unaccounted_ranges();
    let total: usize = unaccounted.iter().map(|r| r.len()).sum();
    let _ = writeln!(
        out,
        "unaccounted bytes {total} in {} ranges; overlapping ranges {}",
        unaccounted.len(),
        p.overlapping_ranges().len()
    );
    for r in unaccounted.iter().take(8) {
        let _ = writeln!(out, "  [{}, {}) {} bytes", r.start, r.end, r.len());
    }
    let _ = writeln!(
        out,
        "imported packages: {}",
        p.imported_packages().join(", ")
    );
    let zero = p.zero_size_export_counts();
    let _ = writeln!(out, "export classes (count, zero-size):");
    for (class, count) in p.export_class_counts() {
        let z = zero.get(&class).copied().unwrap_or(0);
        let _ = writeln!(out, "  {count:>6} {z:>6}  {class}");
    }
    if include_exports {
        let _ = writeln!(out, "exports (index, flags, offset, size, class, path):");
        for (i, e) in p.exports().iter().enumerate() {
            let _ = writeln!(
                out,
                "  {i:>6} 0x{:08x} {:>10} {:>9}  {}  {}",
                e.flags,
                e.serial_offset,
                e.serial_size,
                p.export_class_path(i).unwrap_or("?"),
                p.object_path(ObjectRef::Export(i as u32)).unwrap_or("?"),
            );
        }
    }
    out
}
