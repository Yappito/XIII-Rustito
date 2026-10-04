//! Map dependency report with object-level import resolution (`xiii-tool deps`).
//!
//! Package lookup here is deliberately simple: a case-insensitive match of the imported root
//! package name against the file stems of every tagged package under the game directory. It
//! is a verification aid, not the installation resolver (which handles profiles/precedence).

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use xiii_package::{Limits, ObjectRef, Package, PropertyValue, StructValue};

use crate::corpus::tagged_files;
use crate::props::ref_text;

/// Tagged package files under a root, keyed by lower-case file stem.
pub fn index_packages(root: &Path) -> io::Result<BTreeMap<String, Vec<(String, PathBuf)>>> {
    let mut out: BTreeMap<String, Vec<(String, PathBuf)>> = BTreeMap::new();
    for (rel, path) in tagged_files(root)? {
        let stem = path
            .file_stem()
            .map(|s| s.to_string_lossy().to_lowercase())
            .unwrap_or_default();
        out.entry(stem).or_default().push((rel, path));
    }
    Ok(out)
}

/// One imported root package.
#[derive(Debug, Clone, Default)]
pub struct PackageDep {
    /// Name as written in the import table.
    pub name: String,
    /// Matching files (relative paths); empty when not found.
    pub files: Vec<String>,
    /// Imports below this root (excluding the root import itself).
    pub imports: u64,
    /// Imports found as exports with a matching class in the target package.
    pub resolved: u64,
    /// Object-property values in the map pointing at imports below this root.
    pub property_refs: u64,
    /// Explicit resolution failures.
    pub failures: Vec<String>,
    /// Top-level class imports with no export in the package: candidates for native-only
    /// (intrinsic) classes provided by the engine DLLs. Not counted as resolved.
    pub absent_classes: Vec<String>,
    /// Target package parse error.
    pub parse_error: Option<String>,
}

/// One `StaticMeshActor`-like sample.
#[derive(Debug, Clone, Default)]
pub struct ActorSample {
    /// Export path in the map.
    pub path: String,
    /// Class path.
    pub class: String,
    /// `StaticMesh` reference text.
    pub static_mesh: Option<String>,
    /// `Location`.
    pub location: Option<[f32; 3]>,
    /// `Rotation`.
    pub rotation: Option<[i32; 3]>,
    /// `DrawScale`.
    pub draw_scale: Option<f32>,
    /// `DrawScale3D`.
    pub draw_scale_3d: Option<[f32; 3]>,
}

/// Full dependency report for one map.
#[derive(Debug, Clone, Default)]
pub struct DepsReport {
    /// Map file label.
    pub map: String,
    /// Imported root packages, sorted by name.
    pub packages: Vec<PackageDep>,
    /// Exports whose property block failed to decode (path: error).
    pub property_failures: Vec<String>,
    /// Exports decoded.
    pub exports_decoded: u64,
    /// `Class.Property` -> (root package -> reference count), for references to imports.
    pub property_refs_by_name: BTreeMap<String, BTreeMap<String, u64>>,
    /// Object-property references to local exports.
    pub local_refs: u64,
    /// Null object-property references.
    pub null_refs: u64,
    /// Static mesh actor statistics: total, with StaticMesh import, Location, Rotation, DrawScale.
    pub static_mesh_actors: [u64; 5],
    /// First few static mesh actors.
    pub samples: Vec<ActorSample>,
}

impl DepsReport {
    /// Total non-root imports.
    pub fn imports(&self) -> u64 {
        self.packages.iter().map(|p| p.imports).sum()
    }

    /// Total resolved imports.
    pub fn resolved(&self) -> u64 {
        self.packages.iter().map(|p| p.resolved).sum()
    }

    /// Packages without a file.
    pub fn missing_packages(&self) -> Vec<&str> {
        self.packages
            .iter()
            .filter(|p| p.files.is_empty())
            .map(|p| p.name.as_str())
            .collect()
    }

    /// Top-level class imports absent from their package (native-only class candidates).
    pub fn absent_classes(&self) -> u64 {
        self.packages.iter().map(|p| p.absent_classes.len() as u64).sum()
    }

    /// Imports neither resolved nor absent-class candidates.
    pub fn failures(&self) -> u64 {
        self.packages.iter().map(|p| p.failures.len() as u64).sum()
    }

    /// True when every package was found and parsed and every import either resolved to an
    /// export with a matching class or is a top-level class absent from its package.
    pub fn is_clean(&self) -> bool {
        self.missing_packages().is_empty()
            && self.failures() == 0
            && self.packages.iter().all(|p| p.parse_error.is_none())
    }
}

fn root_of(p: &Package, r: ObjectRef) -> ObjectRef {
    let mut cur = r;
    // Outer chains were validated (bounded, acyclic) at parse time.
    while let Some(outer) = p.object_outer(cur) {
        if outer.is_null() {
            break;
        }
        cur = outer;
    }
    cur
}

/// Full class path of an export in `t` (`Core.Class` for null, package-qualified for local).
fn export_class_full(t: &Package, t_name: &str, export: usize) -> String {
    match t.exports()[export].class {
        ObjectRef::Null => xiii_package::NULL_CLASS_PATH.to_owned(),
        r @ ObjectRef::Import(_) => t.object_path(r).unwrap_or("?").to_owned(),
        r @ ObjectRef::Export(_) => format!("{t_name}.{}", t.object_path(r).unwrap_or("?")),
    }
}

/// Builds the dependency report for a map (`map_data` parsed as `map`).
pub fn analyze(
    label: &str,
    map: &Package,
    map_data: &[u8],
    index: &BTreeMap<String, Vec<(String, PathBuf)>>,
) -> DepsReport {
    let limits = Limits::default();
    let mut report = DepsReport {
        map: label.to_owned(),
        ..DepsReport::default()
    };

    // Group imports by root package.
    let mut by_root: BTreeMap<String, (String, Vec<u32>)> = BTreeMap::new();
    let mut import_root: HashMap<u32, (String, String)> = HashMap::new();
    for i in 0..map.imports().len() as u32 {
        let root = root_of(map, ObjectRef::Import(i));
        let name = map.object_name(root).unwrap_or("?").to_owned();
        let key = name.to_lowercase();
        let entry = by_root
            .entry(key.clone())
            .or_insert_with(|| (name.clone(), Vec::new()));
        if root != ObjectRef::Import(i) {
            entry.1.push(i);
        }
        import_root.insert(i, (key, name));
    }

    // Decode map properties and count references.
    let mut prop_refs: HashMap<String, u64> = HashMap::new();
    for (i, e) in map.exports().iter().enumerate() {
        if e.serial_size == 0 {
            continue;
        }
        let class = map.export_class_path(i).unwrap_or("?");
        let class_short = class.rsplit('.').next().unwrap_or(class);
        let path = map.object_path(ObjectRef::Export(i as u32)).unwrap_or("?");
        let o = match map.read_object_properties(map_data, i, &limits) {
            Ok(o) => o,
            Err(err) => {
                report.property_failures.push(format!("{path} ({class}): {err}"));
                continue;
            }
        };
        report.exports_decoded += 1;
        let is_sma = class_short.eq_ignore_ascii_case("StaticMeshActor");
        let mut sample = ActorSample {
            path: path.to_owned(),
            class: class.to_owned(),
            ..ActorSample::default()
        };
        for q in &o.block.properties {
            let pname = map.property_name(q);
            let mut note_ref = |r: ObjectRef| match r {
                ObjectRef::Null => report.null_refs += 1,
                ObjectRef::Export(_) => report.local_refs += 1,
                ObjectRef::Import(k) => {
                    let (key, name) = import_root.get(&k).cloned().unwrap_or_default();
                    *prop_refs.entry(key).or_default() += 1;
                    *report
                        .property_refs_by_name
                        .entry(format!("{class_short}.{pname}"))
                        .or_default()
                        .entry(name)
                        .or_default() += 1;
                }
            };
            match &q.value {
                PropertyValue::Object(r) | PropertyValue::Class(r) => note_ref(*r),
                PropertyValue::Delegate { object, .. } => note_ref(*object),
                PropertyValue::Struct(StructValue::PointRegion { zone, .. }) => note_ref(*zone),
                _ => {}
            }
            if is_sma {
                match (pname.to_ascii_lowercase().as_str(), &q.value) {
                    ("staticmesh", PropertyValue::Object(r)) => {
                        sample.static_mesh = Some(ref_text(map, *r));
                    }
                    ("location", PropertyValue::Struct(StructValue::Vector(v))) => {
                        sample.location = Some(*v);
                    }
                    ("rotation", PropertyValue::Struct(StructValue::Rotator(r))) => {
                        sample.rotation = Some(*r);
                    }
                    ("drawscale", PropertyValue::Float(f)) => sample.draw_scale = Some(*f),
                    ("drawscale3d", PropertyValue::Struct(StructValue::Vector(v))) => {
                        sample.draw_scale_3d = Some(*v);
                    }
                    _ => {}
                }
            }
        }
        if is_sma {
            report.static_mesh_actors[0] += 1;
            if sample.static_mesh.as_deref().is_some_and(|s| !s.contains("(export")) {
                report.static_mesh_actors[1] += 1;
            }
            report.static_mesh_actors[2] += u64::from(sample.location.is_some());
            report.static_mesh_actors[3] += u64::from(sample.rotation.is_some());
            report.static_mesh_actors[4] += u64::from(sample.draw_scale.is_some());
            if report.samples.len() < 5 {
                report.samples.push(sample);
            }
        }
    }

    // Resolve every import against its target package.
    for (key, (name, imports)) in by_root {
        let mut dep = PackageDep {
            name: name.clone(),
            imports: imports.len() as u64,
            property_refs: prop_refs.get(&key).copied().unwrap_or(0),
            ..PackageDep::default()
        };
        let files = index.get(&key).map(Vec::as_slice).unwrap_or(&[]);
        dep.files = files.iter().map(|(rel, _)| rel.clone()).collect();
        let Some((rel, path)) = files.first() else {
            dep.failures
                .push(format!("package '{name}' not found under the game directory"));
            report.packages.push(dep);
            continue;
        };
        let data = match fs::read(path) {
            Ok(d) => d,
            Err(e) => {
                dep.parse_error = Some(format!("{rel}: {e}"));
                report.packages.push(dep);
                continue;
            }
        };
        let target = match Package::parse(&data, &limits) {
            Ok(t) => t,
            Err(e) => {
                dep.parse_error = Some(format!("{rel}: {e}"));
                report.packages.push(dep);
                continue;
            }
        };
        let mut by_path: HashMap<String, Vec<usize>> = HashMap::new();
        for j in 0..target.exports().len() {
            if let Some(p) = target.object_path(ObjectRef::Export(j as u32)) {
                by_path.entry(p.to_lowercase()).or_default().push(j);
            }
        }
        for i in imports {
            let imp = &map.imports()[i as usize];
            let full = map.object_path(ObjectRef::Import(i)).unwrap_or("?");
            let rel_path = full.split_once('.').map_or("", |(_, rest)| rest);
            let want_class = format!(
                "{}.{}",
                map.name(imp.class_package),
                map.name(imp.class_name)
            );
            match by_path.get(&rel_path.to_lowercase()) {
                None if want_class.eq_ignore_ascii_case("Core.Class") && !rel_path.contains('.') => {
                    dep.absent_classes.push(full.to_owned());
                }
                None => dep
                    .failures
                    .push(format!("{want_class} {full}: no export '{rel_path}' in {rel}")),
                Some(cands) => {
                    let classes: Vec<String> = cands
                        .iter()
                        .map(|&j| export_class_full(&target, &name, j))
                        .collect();
                    if classes.iter().any(|c| c.eq_ignore_ascii_case(&want_class)) {
                        dep.resolved += 1;
                    } else {
                        dep.failures.push(format!(
                            "{want_class} {full}: export found in {rel} with class {}",
                            classes.join("/")
                        ));
                    }
                }
            }
        }
        report.packages.push(dep);
    }
    report
}

/// Plain-text report.
pub fn report_text(r: &DepsReport) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    let _ = writeln!(out, "map: {}", r.map);
    let _ = writeln!(
        out,
        "imported packages: {} ({} found, {} missing); imports below roots: {}, resolved to exports: {}, absent top-level classes: {}, failures: {}",
        r.packages.len(),
        r.packages.len() - r.missing_packages().len(),
        r.missing_packages().len(),
        r.imports(),
        r.resolved(),
        r.absent_classes(),
        r.failures()
    );
    let _ = writeln!(
        out,
        "{:<24} {:>7} {:>8} {:>9}  file",
        "package", "imports", "resolved", "prop-refs"
    );
    for p in &r.packages {
        let _ = writeln!(
            out,
            "{:<24} {:>7} {:>8} {:>9}  {}",
            p.name,
            p.imports,
            p.resolved,
            p.property_refs,
            if p.files.is_empty() {
                "NOT FOUND".to_owned()
            } else {
                p.files.join(", ")
            }
        );
        if let Some(e) = &p.parse_error {
            let _ = writeln!(out, "    PARSE ERROR {e}");
        }
        for f in &p.failures {
            let _ = writeln!(out, "    UNRESOLVED {f}");
        }
        if !p.absent_classes.is_empty() {
            let _ = writeln!(
                out,
                "    ABSENT CLASS (no export; native-only class candidate): {}",
                p.absent_classes.join(", ")
            );
        }
    }
    let _ = writeln!(
        out,
        "map exports decoded: {}; property-block failures: {}",
        r.exports_decoded,
        r.property_failures.len()
    );
    for f in r.property_failures.iter().take(20) {
        let _ = writeln!(out, "    PROPERTY FAILURE {f}");
    }
    let total_import_refs: u64 = r.packages.iter().map(|p| p.property_refs).sum();
    let _ = writeln!(
        out,
        "object/class property values: {total_import_refs} to imports, {} to local exports, {} null",
        r.local_refs, r.null_refs
    );
    let mut by_name: Vec<(&String, u64)> = r
        .property_refs_by_name
        .iter()
        .map(|(k, v)| (k, v.values().sum()))
        .collect();
    by_name.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let _ = writeln!(out, "top import-referencing properties:");
    for (name, n) in by_name.iter().take(15) {
        let pkgs: Vec<String> = r.property_refs_by_name[*name]
            .iter()
            .map(|(k, v)| format!("{k}:{v}"))
            .collect();
        let _ = writeln!(out, "  {n:>5} {name} -> {}", pkgs.join(", "));
    }
    let s = r.static_mesh_actors;
    let _ = writeln!(
        out,
        "StaticMeshActor: {} total, StaticMesh->import {}, Location {}, Rotation {}, DrawScale {}",
        s[0], s[1], s[2], s[3], s[4]
    );
    for a in &r.samples {
        let _ = writeln!(
            out,
            "  {} mesh={} loc={:?} rot={:?} scale={:?} scale3d={:?}",
            a.path,
            a.static_mesh.as_deref().unwrap_or("-"),
            a.location,
            a.rotation,
            a.draw_scale,
            a.draw_scale_3d
        );
    }
    let _ = writeln!(
        out,
        "result: {}",
        if r.is_clean() && r.property_failures.is_empty() {
            "OK"
        } else {
            "FAIL"
        }
    );
    out
}

/// JSON report.
pub fn report_json(r: &DepsReport) -> Value {
    json!({
        "map": r.map,
        "imports": r.imports(),
        "resolved": r.resolved(),
        "absent_classes": r.absent_classes(),
        "failures": r.failures(),
        "missing_packages": r.missing_packages(),
        "packages": r.packages.iter().map(|p| json!({
            "name": p.name,
            "files": p.files,
            "imports": p.imports,
            "resolved": p.resolved,
            "property_refs": p.property_refs,
            "failures": p.failures,
            "absent_classes": p.absent_classes,
            "parse_error": p.parse_error,
        })).collect::<Vec<_>>(),
        "exports_decoded": r.exports_decoded,
        "property_failures": r.property_failures,
        "property_refs_by_name": r.property_refs_by_name,
        "local_refs": r.local_refs,
        "null_refs": r.null_refs,
        "static_mesh_actors": {
            "total": r.static_mesh_actors[0],
            "static_mesh_import": r.static_mesh_actors[1],
            "location": r.static_mesh_actors[2],
            "rotation": r.static_mesh_actors[3],
            "draw_scale": r.static_mesh_actors[4],
        },
        "clean": r.is_clean() && r.property_failures.is_empty(),
    })
}
