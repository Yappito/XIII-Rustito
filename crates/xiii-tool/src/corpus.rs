//! Read-only installation scan and comparison against a saved research inventory.
//!
//! The walk mirrors `tools/probe_install.py`: regular files only (symlinks are not followed),
//! directories named `save`, `saves`, `profiles`, `cache` or `logs` are skipped, and a file is a
//! package when its first four bytes are the Unreal package tag. Unlike the probe, the tag is
//! checked for every file regardless of extension, so packages with unexpected extensions show
//! up as "not in inventory" instead of being silently ignored.

use std::collections::{BTreeMap, HashMap};
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde_json::Value;
use xiii_package::{Limits, Package, has_package_tag};

use crate::report::{PROBE_KEYS, probe_json};

const SKIPPED_DIRS: [&str; 5] = ["save", "saves", "profiles", "cache", "logs"];

/// Summary metrics for one successfully parsed package (not compared with inventories).
#[derive(Debug, Clone)]
pub struct ParsedInfo {
    /// Probe-compatible JSON.
    pub probe: Value,
    /// `(version, licensee)`.
    pub dialect: (u16, u16),
    /// Number of generation records.
    pub generation_count: usize,
    /// Whether the newest generation equals the current export/name counts.
    pub latest_generation_matches: Option<bool>,
    /// Bytes between the summary and the first table.
    pub header_gap: i64,
    /// Bytes not covered by summary, tables or export payloads.
    pub unaccounted_bytes: u64,
    /// Number of overlapping covered ranges.
    pub overlaps: usize,
}

/// Result of reading and parsing one package file.
#[derive(Debug, Clone)]
pub struct PackageOutcome {
    /// Path relative to the installation root, `/`-separated.
    pub rel_path: String,
    /// File size in bytes.
    pub bytes: u64,
    /// Parse result; the error string includes table/offset context.
    pub result: Result<ParsedInfo, String>,
}

/// Recursively scans `root` (read-only) and parses every file carrying the package tag.
/// Results are sorted by relative path.
pub fn scan(root: &Path, limits: &Limits) -> io::Result<Vec<PackageOutcome>> {
    let mut out = Vec::new();
    for (rel, path) in tagged_files(root)? {
        let data = fs::read(&path)?;
        let result = Package::parse(&data, limits)
            .map(|p| info(&p))
            .map_err(|e| e.to_string());
        out.push(PackageOutcome {
            rel_path: rel,
            bytes: data.len() as u64,
            result,
        });
    }
    Ok(out)
}

/// Recursively lists (read-only) every regular file under `root` whose first four bytes are the
/// package tag, as `(relative '/'-separated path, full path)`, sorted by relative path. Uses the
/// same skipped directories as [`scan`].
pub fn tagged_files(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for (rel, path) in all_files(root)? {
        if starts_with_tag(&path)? {
            out.push((rel, path));
        }
    }
    Ok(out)
}

/// Recursively lists (read-only) every regular file under `root`, sorted by relative path.
pub fn all_files(root: &Path) -> io::Result<Vec<(String, PathBuf)>> {
    let mut files = Vec::new();
    walk(root, root, &mut files)?;
    files.sort();
    Ok(files)
}

fn info(p: &Package) -> ParsedInfo {
    let s = p.summary();
    ParsedInfo {
        probe: probe_json(p),
        dialect: (s.version, s.licensee),
        generation_count: s.generations.len(),
        latest_generation_matches: s.latest_generation_matches_tables(),
        header_gap: s.header_gap(),
        unaccounted_bytes: p.unaccounted_ranges().iter().map(|r| r.len() as u64).sum(),
        overlaps: p.overlapping_ranges().len(),
    }
}

fn starts_with_tag(path: &Path) -> io::Result<bool> {
    let mut prefix = [0u8; 4];
    let mut file = File::open(path)?;
    let mut filled = 0;
    while filled < prefix.len() {
        let n = file.read(&mut prefix[filled..])?;
        if n == 0 {
            return Ok(false);
        }
        filled += n;
    }
    Ok(has_package_tag(&prefix))
}

fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, PathBuf)>) -> io::Result<()> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_lowercase();
        if SKIPPED_DIRS.contains(&name.as_str()) {
            continue;
        }
        if file_type.is_dir() {
            walk(root, &path, out)?;
        } else if file_type.is_file() {
            let rel = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            out.push((rel, path));
        }
    }
    Ok(())
}

/// Package entries of a probe inventory (`docs/evidence/*-inventory.json`), keyed by path.
#[derive(Debug, Clone, Default)]
pub struct Inventory {
    /// Inventory label.
    pub label: String,
    /// Relative path -> (file bytes, probe package object).
    pub packages: BTreeMap<String, (u64, Value)>,
    /// Relative path -> recorded probe error, for files the probe failed to parse.
    pub errors: BTreeMap<String, String>,
}

/// Loads a probe inventory JSON file.
pub fn load_inventory(path: &Path) -> Result<Inventory, String> {
    let text =
        fs::read_to_string(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let root: Value = serde_json::from_str(&text)
        .map_err(|e| format!("invalid JSON in {}: {e}", path.display()))?;
    let files = root
        .get("files")
        .and_then(Value::as_array)
        .ok_or_else(|| format!("{}: missing 'files' array", path.display()))?;
    let mut inv = Inventory {
        label: root
            .get("label")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        ..Inventory::default()
    };
    for f in files {
        let Some(rel) = f.get("path").and_then(Value::as_str) else {
            continue;
        };
        let bytes = f.get("bytes").and_then(Value::as_u64).unwrap_or(0);
        if let Some(pkg) = f.get("package") {
            inv.packages.insert(rel.to_owned(), (bytes, pkg.clone()));
        } else if let Some(err) = f.get("package_error").and_then(Value::as_str) {
            inv.errors.insert(rel.to_owned(), err.to_owned());
        }
    }
    Ok(inv)
}

/// Outcome of comparing a scan with an inventory.
#[derive(Debug, Clone, Default)]
pub struct Comparison {
    /// Packages whose every compared field matched.
    pub matched: usize,
    /// Path -> list of human-readable field differences.
    pub mismatches: Vec<(String, Vec<String>)>,
    /// Inventory packages not found (or not tagged) on disk.
    pub missing_on_disk: Vec<String>,
    /// Tagged files on disk absent from the inventory.
    pub not_in_inventory: Vec<String>,
    /// Paths matched only case-insensitively (informational).
    pub case_only_matches: Vec<String>,
}

impl Comparison {
    /// True when everything matched and nothing is missing or extra.
    pub fn is_clean(&self) -> bool {
        self.mismatches.is_empty()
            && self.missing_on_disk.is_empty()
            && self.not_in_inventory.is_empty()
    }
}

fn short(v: &Value) -> String {
    let s = v.to_string();
    if s.len() > 160 {
        format!("{}...", &s[..s.floor_char_boundary(157)])
    } else {
        s
    }
}

fn diff_maps(key: &str, ours: &Value, theirs: &Value, out: &mut Vec<String>) {
    match (ours.as_object(), theirs.as_object()) {
        (Some(a), Some(b)) => {
            for (k, va) in a {
                match b.get(k) {
                    Some(vb) if vb == va => {}
                    Some(vb) => out.push(format!("{key}.{k}: rust {va} vs inventory {vb}")),
                    None => out.push(format!("{key}.{k}: rust {va}, absent in inventory")),
                }
            }
            for (k, vb) in b {
                if !a.contains_key(k) {
                    out.push(format!("{key}.{k}: absent in rust, inventory {vb}"));
                }
            }
        }
        _ => out.push(format!(
            "{key}: rust {} vs inventory {}",
            short(ours),
            short(theirs)
        )),
    }
}

/// Compares parsed packages with inventory entries. Parse failures are not part of the
/// comparison; callers report them separately.
pub fn compare(outcomes: &[PackageOutcome], inventory: &Inventory) -> Comparison {
    let mut cmp = Comparison::default();
    let lower: HashMap<String, &String> = inventory
        .packages
        .keys()
        .map(|k| (k.to_lowercase(), k))
        .collect();
    let mut seen = std::collections::HashSet::new();
    for o in outcomes {
        let key = if inventory.packages.contains_key(&o.rel_path) {
            Some(&o.rel_path)
        } else if let Some(k) = lower.get(&o.rel_path.to_lowercase()) {
            cmp.case_only_matches.push(o.rel_path.clone());
            Some(*k)
        } else {
            None
        };
        let Some(key) = key else {
            cmp.not_in_inventory.push(o.rel_path.clone());
            continue;
        };
        seen.insert(key.clone());
        let Ok(parsed) = &o.result else { continue };
        let (inv_bytes, inv) = &inventory.packages[key];
        let mut diffs = Vec::new();
        if *inv_bytes != o.bytes {
            diffs.push(format!("bytes: rust {} vs inventory {inv_bytes}", o.bytes));
        }
        for k in PROBE_KEYS {
            let ours = &parsed.probe[k];
            let theirs = inv.get(k).unwrap_or(&Value::Null);
            if ours != theirs {
                diff_maps(k, ours, theirs, &mut diffs);
            }
        }
        if diffs.is_empty() {
            cmp.matched += 1;
        } else {
            cmp.mismatches.push((o.rel_path.clone(), diffs));
        }
    }
    cmp.missing_on_disk = inventory
        .packages
        .keys()
        .filter(|k| !seen.contains(*k))
        .cloned()
        .collect();
    cmp
}
