//! Property-block coverage over an installation (`xiii-tool coverage`).
//!
//! For every export of every package the state frame and tagged-property block are attempted
//! (for `Core.Class` exports: the class defaults after the native class data, via `xiii-script`)
//! and aggregated per class. Only metadata is collected (class/struct/property names, counts,
//! sizes, offsets and error kinds); property values are never recorded.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::Path;

use serde_json::{Value, json};
use xiii_package::{
    Limits, ObjectProperties, ObjectRef, Package, PropertyType, PropertyValue, RF_HAS_STACK,
    RawReason,
};
use xiii_script::{ScriptLimits, ScriptObject, read_script_object};

use crate::corpus::tagged_files;
use crate::props::{error_kind_key, raw_reason_key};

/// Per-class aggregate.
#[derive(Debug, Clone, Default)]
pub struct ClassStats {
    /// Exports of this class.
    pub exports: u64,
    /// Exports with no payload (not attempted).
    pub zero_size: u64,
    /// Exports attempted.
    pub attempted: u64,
    /// Blocks decoded with no anomalous values.
    pub ok: u64,
    /// Blocks decoded but containing at least one anomalous value.
    pub ok_with_anomalies: u64,
    /// Blocks that failed structurally.
    pub failed: u64,
    /// First failure: `package export-path: error`.
    pub first_error: Option<String>,
    /// Failure kinds.
    pub error_kinds: BTreeMap<String, u64>,
    /// Decoded exports with an empty native tail.
    pub tail_zero: u64,
    /// Total tail bytes over decoded exports.
    pub tail_bytes: u64,
    /// Largest tail.
    pub tail_max: u64,
    /// Total payload bytes over attempted exports.
    pub payload_bytes: u64,
    /// Total state-frame + property bytes over decoded exports.
    pub consumed_bytes: u64,
    /// Exports flagged RF_HasStack.
    pub has_stack: u64,
    /// Decoded property count.
    pub properties: u64,
}

impl ClassStats {
    /// Decoded (ok + ok with anomalies).
    pub fn decoded(&self) -> u64 {
        self.ok + self.ok_with_anomalies
    }

    /// Mean tail length over decoded exports.
    pub fn mean_tail(&self) -> f64 {
        if self.decoded() == 0 {
            0.0
        } else {
            self.tail_bytes as f64 / self.decoded() as f64
        }
    }

    /// Coarse category for summaries.
    pub fn category(&self) -> &'static str {
        if self.attempted == 0 {
            "no-payload"
        } else if self.failed == self.attempted {
            "failing"
        } else if self.failed > 0 {
            "partly-failing"
        } else if self.tail_zero == self.decoded() {
            "props-only"
        } else {
            "props+tail"
        }
    }
}

/// Whole-corpus aggregate.
#[derive(Debug, Clone, Default)]
pub struct Coverage {
    /// Packages scanned.
    pub packages: u64,
    /// Package table parse errors.
    pub package_errors: Vec<(String, String)>,
    /// Per class.
    pub classes: BTreeMap<String, ClassStats>,
    /// Property type -> count.
    pub property_types: BTreeMap<String, u64>,
    /// `Type:size-code` -> count.
    pub size_codes: BTreeMap<String, u64>,
    /// Decoded struct name -> count.
    pub decoded_structs: BTreeMap<String, u64>,
    /// Unknown struct name -> (size -> count).
    pub raw_structs: BTreeMap<String, BTreeMap<u32, u64>>,
    /// `Type reason` -> count, for anomalous values.
    pub anomalies: BTreeMap<String, u64>,
    /// First anomalies: package, export path, property, type, size, reason.
    pub anomaly_examples: Vec<String>,
    /// Array properties: count -> number of tags whose element bytes are 0 / non-zero.
    pub arrays: u64,
    /// Tags with a non-zero static-array index.
    pub array_index_nonzero: u64,
    /// Tags with an array index above 127 (two-byte form).
    pub array_index_two_byte: u64,
    /// Tags with an array index above 16383 (four-byte form).
    pub array_index_four_byte: u64,
    /// Largest array index.
    pub array_index_max: u32,
    /// Bool tags with value false / true.
    pub bools: [u64; 2],
    /// Declared size of bool tags -> count (bools carry no value bytes).
    pub bool_sizes: BTreeMap<u32, u64>,
    /// Decoded blocks terminated by the first `None` name entry / by a later duplicate.
    pub terminator_first_none: [u64; 2],
    /// Examples of `Core.Class` defaults decoded after the native class data (xiii-script).
    pub class_payload_parsed: Vec<String>,
    /// State frames with a null / non-null node.
    pub frame_node_null: u64,
    /// State frames with a non-null node.
    pub frame_node_set: u64,
    /// State frames whose node equals the state node.
    pub frame_node_is_state: u64,
    /// Code offsets -> count.
    pub frame_offsets: BTreeMap<i32, u64>,
    /// Probe masks -> count.
    pub frame_probe_masks: BTreeMap<u64, u64>,
    /// State frame byte length -> count.
    pub frame_lengths: BTreeMap<usize, u64>,
    /// Object/class references: null, import, export.
    pub refs: [u64; 3],
    /// Unknown-struct values whose bytes also parse exactly as a tagged block / do not.
    pub raw_structs_tagged_like: [u64; 2],
}

const MAX_EXAMPLES: usize = 40;

/// Scans `root` read-only and aggregates property coverage.
pub fn scan(root: &Path) -> io::Result<Coverage> {
    let limits = Limits::default();
    let mut cov = Coverage::default();
    for (rel, path) in tagged_files(root)? {
        let data = fs::read(&path)?;
        cov.packages += 1;
        let p = match Package::parse(&data, &limits) {
            Ok(p) => p,
            Err(e) => {
                cov.package_errors.push((rel, e.to_string()));
                continue;
            }
        };
        add_package(&mut cov, &rel, &p, &data, &limits);
    }
    Ok(cov)
}

/// State frame and property block of an export. For `Core.Class` exports the defaults follow
/// the native UField/UStruct/UState/UClass data, which `xiii-script` decodes first; the
/// resulting block ends exactly at the payload end (consumed = whole payload).
fn read_leading_properties(
    p: &Package,
    data: &[u8],
    i: usize,
    limits: &Limits,
) -> Result<ObjectProperties, (String, String, Option<u64>)> {
    let e = &p.exports()[i];
    let is_class = p
        .export_class_path(i)
        .is_some_and(|c| c.eq_ignore_ascii_case("Core.Class"));
    if !is_class {
        return p
            .read_object_properties(data, i, limits)
            .map_err(|err| (error_kind_key(&err.kind), err.to_string(), err.offset));
    }
    match read_script_object(p, data, i, &ScriptLimits::default(), limits) {
        Ok(ScriptObject::Class(c)) => Ok(ObjectProperties {
            export: i as u32,
            payload: e.serial_span().unwrap_or(c.defaults.span),
            state_frame: c.state_frame,
            block: c.defaults,
        }),
        Ok(_) => Err(("NotAClass".into(), "not a class".into(), None)),
        Err(err) => Err((
            format!("Script:{}", err.kind_key()),
            err.to_string(),
            err.offset.map(|o| o as u64),
        )),
    }
}

/// Adds one parsed package to the aggregate.
pub fn add_package(cov: &mut Coverage, rel: &str, p: &Package, data: &[u8], limits: &Limits) {
    for (i, e) in p.exports().iter().enumerate() {
        let class = p.export_class_path(i).unwrap_or("?").to_owned();
        let st = cov.classes.entry(class).or_default();
        st.exports += 1;
        if e.flags & RF_HAS_STACK != 0 {
            st.has_stack += 1;
        }
        if e.serial_size == 0 {
            st.zero_size += 1;
            continue;
        }
        st.attempted += 1;
        st.payload_bytes += u64::from(e.serial_size);
        let path = p.object_path(ObjectRef::Export(i as u32)).unwrap_or("?");
        match read_leading_properties(p, data, i, limits) {
            Err((kind_key, err, offset)) => {
                st.failed += 1;
                *st.error_kinds.entry(kind_key).or_default() += 1;
                if st.first_error.is_none() {
                    let rel_off = offset
                        .map(|o| {
                            format!(
                                " (payload+{})",
                                o.saturating_sub(u64::from(e.serial_offset))
                            )
                        })
                        .unwrap_or_default();
                    st.first_error = Some(format!("{rel} {path}: {err}{rel_off}"));
                }
            }
            Ok(o) => {
                let tail = o.tail().len() as u64;
                st.tail_bytes += tail;
                st.tail_max = st.tail_max.max(tail);
                if tail == 0 {
                    st.tail_zero += 1;
                }
                st.consumed_bytes += o.consumed() as u64;
                let first_none = p
                    .names()
                    .iter()
                    .position(|n| n.text.eq_ignore_ascii_case("None"));
                let is_first = first_none == Some(o.block.terminator.index() as usize);
                cov.terminator_first_none[usize::from(!is_first)] += 1;
                if e.class.is_null() && cov.class_payload_parsed.len() < MAX_EXAMPLES {
                    cov.class_payload_parsed.push(format!(
                        "{rel} {path}: {} default properties, block ends at payload end ({} bytes)",
                        o.block.properties.len(),
                        o.payload.len()
                    ));
                }
                st.properties += o.block.properties.len() as u64;
                let anomalous = o.block.properties.iter().any(|q| q.anomaly().is_some());
                if anomalous {
                    st.ok_with_anomalies += 1;
                } else {
                    st.ok += 1;
                }
                if let Some(sf) = &o.state_frame {
                    if sf.node.is_null() {
                        cov.frame_node_null += 1;
                    } else {
                        cov.frame_node_set += 1;
                    }
                    if sf.node == sf.state_node {
                        cov.frame_node_is_state += 1;
                    }
                    if let Some(off) = sf.offset {
                        *cov.frame_offsets.entry(off).or_default() += 1;
                    }
                    *cov.frame_probe_masks.entry(sf.probe_mask).or_default() += 1;
                    *cov.frame_lengths.entry(sf.span.len()).or_default() += 1;
                }
                for q in &o.block.properties {
                    let type_name = q.kind.name();
                    *cov.property_types.entry(type_name.to_owned()).or_default() += 1;
                    *cov.size_codes
                        .entry(format!("{type_name}:{}", (q.info >> 4) & 7))
                        .or_default() += 1;
                    if q.array_index != 0 {
                        cov.array_index_nonzero += 1;
                    }
                    if q.array_index > 127 {
                        cov.array_index_two_byte += 1;
                    }
                    if q.array_index > 16383 {
                        cov.array_index_four_byte += 1;
                    }
                    cov.array_index_max = cov.array_index_max.max(q.array_index);
                    match &q.value {
                        PropertyValue::Bool(b) => {
                            cov.bools[usize::from(*b)] += 1;
                            *cov.bool_sizes.entry(q.size).or_default() += 1;
                        }
                        PropertyValue::Array { .. } => cov.arrays += 1,
                        PropertyValue::Object(r) | PropertyValue::Class(r) => {
                            cov.refs[match r {
                                ObjectRef::Null => 0,
                                ObjectRef::Import(_) => 1,
                                ObjectRef::Export(_) => 2,
                            }] += 1;
                        }
                        PropertyValue::Struct(_) if q.kind == PropertyType::Struct => {
                            let n = q.struct_name.map_or("?", |s| p.name(s));
                            *cov.decoded_structs.entry(n.to_owned()).or_default() += 1;
                        }
                        PropertyValue::Raw(RawReason::UnknownStruct) => {
                            let tagged = p
                                .read_property_block(
                                    data,
                                    q.value_span.start,
                                    q.value_span.end,
                                    limits,
                                )
                                .is_ok_and(|b| b.span.end == q.value_span.end);
                            cov.raw_structs_tagged_like[usize::from(!tagged)] += 1;
                            let n = q.struct_name.map_or("?", |s| p.name(s));
                            *cov.raw_structs
                                .entry(n.to_owned())
                                .or_default()
                                .entry(q.size)
                                .or_default() += 1;
                        }
                        _ => {}
                    }
                    if let Some(reason) = q.anomaly() {
                        let label = match q.struct_name {
                            Some(s) => format!("Struct<{}>", p.name(s)),
                            None => type_name.to_owned(),
                        };
                        *cov.anomalies
                            .entry(format!("{label} {}", raw_reason_key(reason)))
                            .or_default() += 1;
                        if cov.anomaly_examples.len() < MAX_EXAMPLES {
                            cov.anomaly_examples.push(format!(
                                "{rel} {path}: {} {label} size {} at {}: {}",
                                p.property_name(q),
                                q.size,
                                q.tag_span.start,
                                raw_reason_key(reason)
                            ));
                        }
                    }
                }
            }
        }
    }
}

/// Totals over all classes.
pub fn totals(cov: &Coverage) -> ClassStats {
    let mut t = ClassStats::default();
    for s in cov.classes.values() {
        t.exports += s.exports;
        t.zero_size += s.zero_size;
        t.attempted += s.attempted;
        t.ok += s.ok;
        t.ok_with_anomalies += s.ok_with_anomalies;
        t.failed += s.failed;
        t.tail_zero += s.tail_zero;
        t.tail_bytes += s.tail_bytes;
        t.tail_max = t.tail_max.max(s.tail_max);
        t.payload_bytes += s.payload_bytes;
        t.consumed_bytes += s.consumed_bytes;
        t.has_stack += s.has_stack;
        t.properties += s.properties;
        for (k, n) in &s.error_kinds {
            *t.error_kinds.entry(k.clone()).or_default() += n;
        }
    }
    t
}

fn stats_json(s: &ClassStats) -> Value {
    json!({
        "category": s.category(),
        "exports": s.exports,
        "zero_size": s.zero_size,
        "attempted": s.attempted,
        "ok": s.ok,
        "ok_with_anomalies": s.ok_with_anomalies,
        "failed": s.failed,
        "first_error": s.first_error,
        "error_kinds": s.error_kinds,
        "has_stack": s.has_stack,
        "properties": s.properties,
        "payload_bytes": s.payload_bytes,
        "consumed_bytes": s.consumed_bytes,
        "tail_zero": s.tail_zero,
        "tail_bytes": s.tail_bytes,
        "tail_mean": (s.mean_tail() * 10.0).round() / 10.0,
        "tail_max": s.tail_max,
    })
}

/// Metadata-only JSON report.
pub fn to_json(cov: &Coverage, label: &str) -> Value {
    let mut categories: BTreeMap<&str, u64> = BTreeMap::new();
    for s in cov.classes.values() {
        *categories.entry(s.category()).or_default() += 1;
    }
    let raw_structs: BTreeMap<&String, BTreeMap<String, u64>> = cov
        .raw_structs
        .iter()
        .map(|(k, v)| (k, v.iter().map(|(s, n)| (s.to_string(), *n)).collect()))
        .collect();
    let top_offsets: BTreeMap<String, u64> = cov
        .frame_offsets
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    let masks: BTreeMap<String, u64> = cov
        .frame_probe_masks
        .iter()
        .map(|(k, v)| (format!("0x{k:016x}"), *v))
        .collect();
    let lengths: BTreeMap<String, u64> = cov
        .frame_lengths
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    json!({
        "label": label,
        "tool": concat!("xiii-tool ", env!("CARGO_PKG_VERSION"), " coverage"),
        "content_policy": "metadata only: class/struct/property names, counts, sizes, offsets and error kinds; no property values",
        "notes": [
            "Every non-class export payload is attempted as [state frame if RF_HasStack] + tagged-property block from payload offset 0.",
            "Core.Class payloads start with native UField/UStruct/UState/UClass data (decoded by crates/xiii-script, XIII v100/licensee-58 layout); the class defaults are the tagged-property block after it and must end exactly at the payload end. For classes, consumed = whole payload.",
            "tail = payload bytes after the property block (class-native data, not decoded here)."
        ],
        "packages": cov.packages,
        "package_errors": cov.package_errors,
        "totals": stats_json(&totals(cov)),
        "class_categories": categories,
        "classes": cov.classes.iter().map(|(k, v)| (k.clone(), stats_json(v))).collect::<serde_json::Map<_, _>>(),
        "property_types": cov.property_types,
        "size_codes": cov.size_codes,
        "decoded_structs": cov.decoded_structs,
        "raw_structs_by_size": raw_structs,
        "anomalies": cov.anomalies,
        "anomaly_examples": cov.anomaly_examples,
        "array_properties": cov.arrays,
        "array_index": {
            "nonzero": cov.array_index_nonzero,
            "above_127": cov.array_index_two_byte,
            "above_16383": cov.array_index_four_byte,
            "max": cov.array_index_max,
        },
        "bools": {"false": cov.bools[0], "true": cov.bools[1]},
        "bool_declared_sizes": cov.bool_sizes.iter().map(|(k, v)| (k.to_string(), *v)).collect::<BTreeMap<_, _>>(),
        "class_defaults_examples": cov.class_payload_parsed,
        "terminators": {
            "first_none_name": cov.terminator_first_none[0],
            "later_duplicate_none_name": cov.terminator_first_none[1],
        },
        "unknown_structs_parsing_as_tagged_blocks": {
            "yes": cov.raw_structs_tagged_like[0],
            "no": cov.raw_structs_tagged_like[1],
        },
        "object_refs": {"null": cov.refs[0], "import": cov.refs[1], "export": cov.refs[2]},
        "state_frames": {
            "node_null": cov.frame_node_null,
            "node_set": cov.frame_node_set,
            "node_equals_state_node": cov.frame_node_is_state,
            "offsets": top_offsets,
            "probe_masks": masks,
            "byte_lengths": lengths,
        },
    })
}
