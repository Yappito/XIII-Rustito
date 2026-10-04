//! `xiii-tool`: read-only inspection CLI for XIII Classic packages and installations.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use xiii_package::{Limits, Package};
use xiii_tool::{corpus, coverage, deps, props, report};

const USAGE: &str = "\
xiii-tool: read-only inspection of XIII Classic (UE2, package version 100) data

USAGE:
  xiii-tool inspect <package-file> [--json] [--exports]
      Parse one package and print its summary, GUID/generations, table spans,
      imported packages and export class histogram. --exports lists every export
      (index, flags, serial offset/size, class, path). --json prints JSON instead.

  xiii-tool corpus <install-root> [--compare <inventory.json>] [--verbose]
      Walk the installation read-only, parse every file with the package tag and
      print a summary. With --compare, check each package against a saved probe
      inventory (docs/evidence/*-inventory.json): file size, version/licensee,
      flags, table counts and spans, class and zero-size histograms, imported
      packages. Exits 1 on any parse error, mismatch, missing or extra package.

  xiii-tool props <package-file> [--export <index|path>] [--json]
      Decode the state frame and tagged-property block of every export (or one
      export, by zero-based index or case-insensitive path) and report bytes
      consumed and the remaining native tail. Output includes package content;
      keep it local.

  xiii-tool coverage <install-root> [--json-out <file>] [--label <text>] [--all]
      Attempt the property block of every export of every package and print a
      per-class table (attempted / ok / anomalous values / failed / tail).
      --json-out writes a metadata-only report (refused inside <install-root>).
      --all lists every class instead of the 60 most frequent plus failing ones.

  xiii-tool deps <map-file> --game-dir <install-root> [--json]
      List the imported packages of a map, find them (case-insensitive file
      stem) under the game directory, resolve every import to an export with the
      same path and class in its package, and count object-property references
      per package. Exits 1 if a package is missing or an import is unresolved.

Exit codes: 0 success, 1 parse error or mismatch, 2 usage or I/O error.";

/// Appends a line to a report buffer (formatting into a String cannot fail).
macro_rules! outln {
    ($out:expr, $($arg:tt)*) => {{
        let _ = writeln!($out, $($arg)*);
    }};
}

/// Writes a finished report to stdout; a closed pipe (e.g. `| head`) is not an error.
fn emit(text: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = stdout
        .write_all(text.as_bytes())
        .and_then(|()| stdout.flush());
}

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\n{USAGE}");
    ExitCode::from(2)
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("inspect") => inspect(&args[1..]),
        Some("corpus") => corpus_cmd(&args[1..]),
        Some("props") => props_cmd(&args[1..]),
        Some("coverage") => coverage_cmd(&args[1..]),
        Some("deps") => deps_cmd(&args[1..]),
        Some("-h" | "--help" | "help") => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => usage_error(&format!("unknown command '{other}'")),
        None => usage_error("missing command"),
    }
}

fn inspect(args: &[String]) -> ExitCode {
    let (mut file, mut json, mut exports) = (None, false, false);
    for a in args {
        match a.as_str() {
            "--json" => json = true,
            "--exports" => exports = true,
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if file.is_none() => file = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(file) = file else {
        return usage_error("inspect needs a package file");
    };
    let data = match std::fs::read(&file) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("error: cannot read {}: {e}", file.display());
            return ExitCode::from(2);
        }
    };
    let package = match Package::parse(&data, &Limits::default()) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("error: {}: {e}", file.display());
            return ExitCode::from(1);
        }
    };
    if json {
        let v = report::package_json(&package, exports);
        match serde_json::to_string_pretty(&v) {
            Ok(s) => emit(
                &(s + "
"),
            ),
            Err(e) => {
                eprintln!("error: JSON serialization failed: {e}");
                return ExitCode::from(2);
            }
        }
    } else {
        emit(&report::package_text(
            &file.display().to_string(),
            &package,
            exports,
        ));
    }
    ExitCode::SUCCESS
}

fn corpus_cmd(args: &[String]) -> ExitCode {
    let (mut root, mut inventory, mut verbose) = (None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--compare" => match it.next() {
                Some(p) => inventory = Some(PathBuf::from(p)),
                None => return usage_error("--compare needs an inventory path"),
            },
            "--verbose" | "-v" => verbose = true,
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if root.is_none() => root = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(root) = root else {
        return usage_error("corpus needs an installation root");
    };
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::from(2);
    }
    let inventory = match inventory.as_deref().map(corpus::load_inventory).transpose() {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(2);
        }
    };

    let mut out = String::new();
    let started = Instant::now();
    let outcomes = match corpus::scan(&root, &Limits::default()) {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: scanning {} failed: {e}", root.display());
            return ExitCode::from(2);
        }
    };
    let elapsed = started.elapsed();
    let mut failed = false;

    let total_bytes: u64 = outcomes.iter().map(|o| o.bytes).sum();
    let errors: Vec<_> = outcomes
        .iter()
        .filter_map(|o| o.result.as_ref().err().map(|e| (&o.rel_path, e)))
        .collect();
    let parsed: Vec<_> = outcomes
        .iter()
        .filter_map(|o| o.result.as_ref().ok().map(|p| (&o.rel_path, p)))
        .collect();

    outln!(out, "corpus: {}", root.display());
    outln!(
        out,
        "packages: {} tagged, {} parsed, {} parse errors ({:.1} MiB read in {:.2} s)",
        outcomes.len(),
        parsed.len(),
        errors.len(),
        total_bytes as f64 / (1024.0 * 1024.0),
        elapsed.as_secs_f64()
    );
    for (path, e) in &errors {
        outln!(out, "  PARSE ERROR {path}: {e}");
        failed = true;
    }

    let mut dialects: BTreeMap<(u16, u16), usize> = BTreeMap::new();
    let mut generations: BTreeMap<usize, usize> = BTreeMap::new();
    let (mut gen_match, mut gen_mismatch, mut gen_none) = (0, Vec::new(), 0);
    let mut header_gaps = Vec::new();
    let (mut gap_pkgs, mut gap_bytes, mut overlap_pkgs) = (0usize, 0u64, Vec::new());
    for (path, p) in &parsed {
        *dialects.entry(p.dialect).or_default() += 1;
        *generations.entry(p.generation_count).or_default() += 1;
        match p.latest_generation_matches {
            Some(true) => gen_match += 1,
            Some(false) => gen_mismatch.push(*path),
            None => gen_none += 1,
        }
        if p.header_gap != 0 {
            header_gaps.push((*path, p.header_gap));
        }
        if p.unaccounted_bytes > 0 {
            gap_pkgs += 1;
            gap_bytes += p.unaccounted_bytes;
        }
        if p.overlaps > 0 {
            overlap_pkgs.push(*path);
        }
        if verbose {
            outln!(
                out,
                "  ok {path} v{}/{} gens {} gap {} unaccounted {}",
                p.dialect.0,
                p.dialect.1,
                p.generation_count,
                p.header_gap,
                p.unaccounted_bytes
            );
        }
    }
    let fmt_map = |m: String| if m.is_empty() { "none".to_owned() } else { m };
    outln!(
        out,
        "dialects (version/licensee): {}",
        fmt_map(
            dialects
                .iter()
                .map(|((v, l), n)| format!("{v}/{l}: {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    );
    outln!(
        out,
        "generation records: {}; latest generation == (export, name) counts: {gen_match}/{} \
         (mismatch {}, none {gen_none})",
        fmt_map(
            generations
                .iter()
                .map(|(g, n)| format!("{g} gen: {n}"))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        parsed.len(),
        gen_mismatch.len()
    );
    for path in &gen_mismatch {
        outln!(out, "  generation mismatch: {path}");
    }
    if header_gaps.is_empty() {
        outln!(out, "summary-to-first-table gap: 0 bytes in every package");
    } else {
        for (path, gap) in &header_gaps {
            outln!(out, "  header gap {gap} bytes: {path}");
        }
    }
    outln!(
        out,
        "bytes outside summary/tables/export payloads: {gap_bytes} in {gap_pkgs} packages; \
         packages with overlapping ranges: {}",
        overlap_pkgs.len()
    );

    if let Some(inv) = &inventory {
        let cmp = corpus::compare(&outcomes, inv);
        outln!(
            out,
            "compare with inventory '{}' ({} packages, {} recorded probe errors): \
             {} matched, {} mismatched, {} missing on disk, {} not in inventory",
            inv.label,
            inv.packages.len(),
            inv.errors.len(),
            cmp.matched,
            cmp.mismatches.len(),
            cmp.missing_on_disk.len(),
            cmp.not_in_inventory.len()
        );
        for (path, diffs) in &cmp.mismatches {
            outln!(out, "  MISMATCH {path}");
            for d in diffs.iter().take(10) {
                outln!(out, "    {d}");
            }
            if diffs.len() > 10 {
                outln!(out, "    ... {} more", diffs.len() - 10);
            }
        }
        for path in &cmp.missing_on_disk {
            outln!(out, "  MISSING ON DISK {path}");
        }
        for path in &cmp.not_in_inventory {
            outln!(out, "  NOT IN INVENTORY {path}");
        }
        if !cmp.case_only_matches.is_empty() {
            outln!(
                out,
                "  note: {} paths matched only case-insensitively",
                cmp.case_only_matches.len()
            );
        }
        if !inv.errors.is_empty() {
            outln!(
                out,
                "  note: inventory records {} probe errors (not compared)",
                inv.errors.len()
            );
        }
        failed |= !cmp.is_clean();
    }
    outln!(out, "result: {}", if failed { "FAIL" } else { "OK" });
    emit(&out);
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn read_package(file: &std::path::Path) -> Result<(Vec<u8>, Package), ExitCode> {
    let data = std::fs::read(file).map_err(|e| {
        eprintln!("error: cannot read {}: {e}", file.display());
        ExitCode::from(2)
    })?;
    let package = Package::parse(&data, &Limits::default()).map_err(|e| {
        eprintln!("error: {}: {e}", file.display());
        ExitCode::from(1)
    })?;
    Ok((data, package))
}

fn emit_json(v: &serde_json::Value) -> ExitCode {
    match serde_json::to_string_pretty(v) {
        Ok(s) => {
            emit(&(s + "\n"));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: JSON serialization failed: {e}");
            ExitCode::from(2)
        }
    }
}

fn props_cmd(args: &[String]) -> ExitCode {
    let (mut file, mut json, mut export) = (None, false, None);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json" => json = true,
            "--export" => match it.next() {
                Some(e) => export = Some(e.clone()),
                None => return usage_error("--export needs an index or path"),
            },
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if file.is_none() => file = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(file) = file else {
        return usage_error("props needs a package file");
    };
    let (data, package) = match read_package(&file) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let selected: Vec<usize> = match &export {
        Some(sel) => match props::find_export(&package, sel) {
            Some(i) => vec![i],
            None => {
                eprintln!("error: no export '{sel}' in {}", file.display());
                return ExitCode::from(1);
            }
        },
        None => (0..package.exports().len()).collect(),
    };
    if json {
        emit_json(&props::props_json(&package, &data, &selected))
    } else {
        emit(&props::props_text(&package, &data, &selected));
        ExitCode::SUCCESS
    }
}

/// True when `out` would be written inside `root` (compared after canonicalizing the
/// existing parts, case-insensitively).
fn inside(out: &std::path::Path, root: &std::path::Path) -> bool {
    let root = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(std::path::Path::new("."));
    let parent = std::fs::canonicalize(parent).unwrap_or_else(|_| parent.to_path_buf());
    let lower = |p: &std::path::Path| p.to_string_lossy().to_lowercase();
    lower(&parent).starts_with(&lower(&root))
}

fn coverage_cmd(args: &[String]) -> ExitCode {
    let (mut root, mut json_out, mut label, mut all) = (None, None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json-out" => match it.next() {
                Some(p) => json_out = Some(PathBuf::from(p)),
                None => return usage_error("--json-out needs a file path"),
            },
            "--label" => match it.next() {
                Some(l) => label = Some(l.clone()),
                None => return usage_error("--label needs text"),
            },
            "--all" => all = true,
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if root.is_none() => root = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(root) = root else {
        return usage_error("coverage needs an installation root");
    };
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::from(2);
    }
    if let Some(out) = &json_out
        && inside(out, &root)
    {
        eprintln!(
            "error: refusing to write {} inside the installation",
            out.display()
        );
        return ExitCode::from(2);
    }
    let started = Instant::now();
    let cov = match coverage::scan(&root) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("error: scanning {} failed: {e}", root.display());
            return ExitCode::from(2);
        }
    };
    let mut out = String::new();
    let t = coverage::totals(&cov);
    outln!(
        out,
        "coverage: {} ({:.2} s)",
        root.display(),
        started.elapsed().as_secs_f64()
    );
    outln!(
        out,
        "packages {} (table errors {}); exports {} (zero-size {}), attempted {}: ok {}, \
         ok with anomalous values {}, failed {}",
        cov.packages,
        cov.package_errors.len(),
        t.exports,
        t.zero_size,
        t.attempted,
        t.ok,
        t.ok_with_anomalies,
        t.failed
    );
    outln!(
        out,
        "payload bytes {}; consumed by state frames + properties {}; decoded exports with empty tail {}",
        t.payload_bytes,
        t.consumed_bytes,
        t.tail_zero
    );
    let mut cats: BTreeMap<&str, usize> = BTreeMap::new();
    for s in cov.classes.values() {
        *cats.entry(s.category()).or_default() += 1;
    }
    outln!(out, "class categories: {cats:?}");
    let mut rows: Vec<(&String, &coverage::ClassStats)> = cov.classes.iter().collect();
    rows.sort_by(|a, b| b.1.attempted.cmp(&a.1.attempted).then(a.0.cmp(b.0)));
    outln!(
        out,
        "{:<34} {:>7} {:>7} {:>7} {:>5} {:>7} {:>7} {:>9}  category",
        "class",
        "exports",
        "attempt",
        "ok",
        "anom",
        "failed",
        "tail=0",
        "mean-tail"
    );
    for (i, (name, s)) in rows.iter().enumerate() {
        if !all && i >= 60 && s.failed == 0 {
            continue;
        }
        outln!(
            out,
            "{:<34} {:>7} {:>7} {:>7} {:>5} {:>7} {:>7} {:>9.1}  {}",
            name,
            s.exports,
            s.attempted,
            s.ok,
            s.ok_with_anomalies,
            s.failed,
            s.tail_zero,
            s.mean_tail(),
            s.category()
        );
    }
    outln!(out, "first failure per failing class:");
    for (name, s) in &rows {
        if let Some(e) = &s.first_error {
            let kinds: Vec<String> = s
                .error_kinds
                .iter()
                .map(|(k, n)| format!("{k}:{n}"))
                .collect();
            outln!(out, "  {name} [{}]: {e}", kinds.join(", "));
        }
    }
    outln!(out, "property types: {:?}", cov.property_types);
    outln!(out, "decoded structs: {:?}", cov.decoded_structs);
    outln!(
        out,
        "unknown structs (name: size->count): {:?}",
        cov.raw_structs
    );
    outln!(
        out,
        "unknown struct values that also parse exactly as a tagged block: {} yes / {} no",
        cov.raw_structs_tagged_like[0],
        cov.raw_structs_tagged_like[1]
    );
    outln!(out, "anomalous values: {:?}", cov.anomalies);
    for ex in cov.anomaly_examples.iter().take(15) {
        outln!(out, "  {ex}");
    }
    outln!(
        out,
        "bools false/true {}/{}; arrays {}; array index nonzero {} (>127: {}, >16383: {}, max {})",
        cov.bools[0],
        cov.bools[1],
        cov.arrays,
        cov.array_index_nonzero,
        cov.array_index_two_byte,
        cov.array_index_four_byte,
        cov.array_index_max
    );
    outln!(out, "bool declared sizes: {:?}", cov.bool_sizes);
    outln!(
        out,
        "terminators: first 'None' name entry {}, later duplicate 'None' entry {}",
        cov.terminator_first_none[0],
        cov.terminator_first_none[1]
    );
    outln!(
        out,
        "Core.Class payloads whose first bytes happen to parse as a property block: {:?}",
        cov.class_payload_parsed
    );
    outln!(
        out,
        "object refs null/import/export {}/{}/{}",
        cov.refs[0],
        cov.refs[1],
        cov.refs[2]
    );
    outln!(
        out,
        "state frames: node null {}, node set {}, node==state {}, lengths {:?}, offsets {:?}, \
         probe masks {}",
        cov.frame_node_null,
        cov.frame_node_set,
        cov.frame_node_is_state,
        cov.frame_lengths,
        cov.frame_offsets,
        cov.frame_probe_masks
            .iter()
            .map(|(k, v)| format!("0x{k:016x}:{v}"))
            .collect::<Vec<_>>()
            .join(", ")
    );
    outln!(out, "size codes: {:?}", cov.size_codes);
    if let Some(path) = &json_out {
        let v = coverage::to_json(&cov, label.as_deref().unwrap_or(""));
        let text = match serde_json::to_string_pretty(&v) {
            Ok(s) => s + "\n",
            Err(e) => {
                eprintln!("error: JSON serialization failed: {e}");
                return ExitCode::from(2);
            }
        };
        if let Err(e) = std::fs::write(path, text) {
            eprintln!("error: cannot write {}: {e}", path.display());
            return ExitCode::from(2);
        }
        outln!(out, "wrote {}", path.display());
    }
    emit(&out);
    if cov.package_errors.is_empty() {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}

fn deps_cmd(args: &[String]) -> ExitCode {
    let (mut map, mut game_dir, mut json) = (None, None, false);
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--game-dir" => match it.next() {
                Some(p) => game_dir = Some(PathBuf::from(p)),
                None => return usage_error("--game-dir needs a directory"),
            },
            "--json" => json = true,
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if map.is_none() => map = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let (Some(map), Some(game_dir)) = (map, game_dir) else {
        return usage_error("deps needs a map file and --game-dir");
    };
    let (data, package) = match read_package(&map) {
        Ok(v) => v,
        Err(code) => return code,
    };
    let index = match deps::index_packages(&game_dir) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("error: scanning {} failed: {e}", game_dir.display());
            return ExitCode::from(2);
        }
    };
    let report = deps::analyze(&map.display().to_string(), &package, &data, &index);
    let clean = report.is_clean() && report.property_failures.is_empty();
    let code = if json {
        emit_json(&deps::report_json(&report))
    } else {
        emit(&deps::report_text(&report));
        ExitCode::SUCCESS
    };
    if code != ExitCode::SUCCESS {
        code
    } else if clean {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(1)
    }
}
