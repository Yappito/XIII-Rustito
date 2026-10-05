//! `xiii-tool audio ...`: HXAudio bank coverage, WAV export and `.uax` linkage.
//!
//! Decoded audio and exported WAVs contain original game data: keep them outside the
//! repository and the installation. This module reads the installation read-only.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Instant;

use xiii_audio::hx::{Codec, DataLocation, HxBank, HxLimits};
use xiii_audio::{decode_entry, hx, write_wav};
use xiii_package::{Limits, ObjectRef, Package, PropertyValue};

pub const USAGE: &str = "\
  xiii-tool audio coverage <install-root> [--json-out <file>]
      Parse every .hxc bank under <install-root>, decode every wave entry (PCM in
      the bank, UBI ADPCM in the sibling .hsc), and print per-class/per-codec
      counts, byte accounting, decode success and .uax linkage. Exits 1 on any
      parse/decode failure. --json-out writes a metadata-only summary (refused
      inside <install-root>).

  xiii-tool audio export <bank.hxc> --entry <index|name> --wav <out.wav>
      Decode one bank entry and write a PCM16 WAV. <index|name> is a zero-based
      index entry or a case-insensitive WavRes name. Refuses to write inside the
      bank's directory (the installation).

  xiii-tool audio link <install-root> [--sound <leaf>] [--wav <out.wav>]
      Link Engine.Sound / SndXIIIStep objects in .uax packages to bank WavRes
      names and print the traced chains. With --sound, trace one name; with
      --wav, decode the first matching bank entry to a WAV.";

fn usage_error(msg: &str) -> ExitCode {
    eprintln!("error: {msg}\n\nUSAGE:\n{USAGE}");
    ExitCode::from(2)
}

/// Entry point for `xiii-tool audio <sub> ...`.
pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("coverage") => coverage(&args[1..]),
        Some("export") => export(&args[1..]),
        Some("link") => link(&args[1..]),
        Some(other) => usage_error(&format!("unknown audio command '{other}'")),
        None => usage_error("missing audio command"),
    }
}

// ------------------------------------------------------------------ filesystem helpers

fn walk_files(dir: &Path, ext: &str, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            walk_files(&path, ext, out);
        } else if path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case(ext))
        {
            out.push(path);
        }
    }
}

/// Extracts the file name from a stored resource path such as `.\\Plage00.hsc`.
fn resource_file(resource: &str) -> String {
    resource
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(resource)
        .to_owned()
}

fn find_case_insensitive(dir: &Path, name: &str) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(name))
        {
            return Some(path);
        }
    }
    None
}

/// True when `out` is inside `root` (case-insensitive, canonicalized where possible).
fn path_inside(out: &Path, root: &Path) -> bool {
    let canon = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let parent = out
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    canon(parent)
        .to_string_lossy()
        .to_lowercase()
        .starts_with(&canon(root).to_string_lossy().to_lowercase())
}

fn read_bank(path: &Path) -> Result<(Vec<u8>, HxBank), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let bank = hx::parse_bank(&bytes, &HxLimits::default())
        .map_err(|e| format!("{}: {e}", path.display()))?;
    Ok((bytes, bank))
}

/// Reads the external stream referenced by a wave entry, if any.
fn read_stream(bank_path: &Path, wave: &xiii_audio::WaveResource) -> Option<Vec<u8>> {
    let resource = wave.resource_name.as_ref()?;
    let dir = bank_path.parent().unwrap_or(Path::new("."));
    let stream_path = find_case_insensitive(dir, &resource_file(resource))?;
    std::fs::read(stream_path).ok()
}

// ------------------------------------------------------------------ scan

/// One named bank entry (a WavRes name resolved to its wave).
#[derive(Clone)]
struct HxRef {
    bank: PathBuf,
    entry: usize,
    codec: Codec,
    channels: u16,
    sample_rate: u32,
    external: bool,
}

#[derive(Default)]
struct Scan {
    banks: usize,
    parse_errors: Vec<String>,
    class_counts: BTreeMap<String, usize>,
    codec_counts: BTreeMap<&'static str, usize>,
    decode_ok: BTreeMap<&'static str, usize>,
    decode_fail: BTreeMap<&'static str, usize>,
    decode_failures: Vec<String>,
    unparsed_bytes: usize,
    unparsed_files: usize,
    names: BTreeMap<String, Vec<HxRef>>,
}

impl Scan {
    fn total_entries(&self) -> usize {
        self.class_counts.values().sum()
    }
}

/// Parses every bank under `root` and builds the WavRes name index. When `decode_audio` is set,
/// every wave entry is decoded (both codecs) and the success/failure counters are filled.
fn analyze(root: &Path, decode_audio: bool) -> Scan {
    let mut banks = Vec::new();
    walk_files(root, "hxc", &mut banks);
    banks.sort();

    let mut scan = Scan::default();
    for path in &banks {
        scan.banks += 1;
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) => {
                scan.parse_errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        let bank = match hx::parse_bank(&bytes, &HxLimits::default()) {
            Ok(b) => b,
            Err(e) => {
                scan.parse_errors.push(format!("{}: {e}", path.display()));
                continue;
            }
        };
        if !bank.unparsed.is_empty() {
            scan.unparsed_files += 1;
            scan.unparsed_bytes += bank.unparsed.iter().map(|s| s.len()).sum::<usize>();
        }

        for entry in &bank.entries {
            *scan
                .class_counts
                .entry(entry.class_name().to_owned())
                .or_default() += 1;
        }

        let mut stream_loaded = false;
        let mut stream: Option<Vec<u8>> = None;
        for entry in &bank.entries {
            let Some(wave) = entry.as_wave() else {
                continue;
            };
            *scan.codec_counts.entry(wave.codec.as_str()).or_default() += 1;
            if let Some(name) = &wave.name {
                scan.names
                    .entry(name.to_lowercase())
                    .or_default()
                    .push(HxRef {
                        bank: path.clone(),
                        entry: entry.index,
                        codec: wave.codec,
                        channels: wave.channels,
                        sample_rate: wave.sample_rate,
                        external: matches!(wave.data, DataLocation::External { .. }),
                    });
            }
            if !decode_audio {
                continue;
            }
            if matches!(wave.data, DataLocation::External { .. }) && !stream_loaded {
                stream = read_stream(path, wave);
                stream_loaded = true;
            }
            let label = wave.codec.as_str();
            match decode_entry(&bank, entry.index, &bytes, stream.as_deref()) {
                Ok(_) => *scan.decode_ok.entry(label).or_default() += 1,
                Err(e) => {
                    *scan.decode_fail.entry(label).or_default() += 1;
                    if scan.decode_failures.len() < 20 {
                        scan.decode_failures.push(format!(
                            "{}#{}: {e}",
                            path.display(),
                            entry.index
                        ));
                    }
                }
            }
        }
    }
    scan
}

// ------------------------------------------------------------------ coverage

fn coverage(args: &[String]) -> ExitCode {
    let mut root = None;
    let mut json_out = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--json-out" => match it.next() {
                Some(p) => json_out = Some(PathBuf::from(p)),
                None => return usage_error("--json-out needs a file path"),
            },
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if root.is_none() => root = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(root) = root else {
        return usage_error("audio coverage needs an installation root");
    };
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::from(2);
    }
    if let Some(out) = &json_out
        && path_inside(out, &root)
    {
        eprintln!(
            "error: refusing to write {} inside the installation",
            out.display()
        );
        return ExitCode::from(2);
    }

    let started = Instant::now();
    let scan = analyze(&root, true);
    let linkage = scan_linkage(&root, &scan.names);

    let mut out = String::new();
    let total_decode: usize =
        scan.decode_ok.values().sum::<usize>() + scan.decode_fail.values().sum::<usize>();
    let _ = writeln!(
        out,
        "audio coverage: {} ({:.2} s)",
        root.display(),
        started.elapsed().as_secs_f64()
    );
    let _ = writeln!(
        out,
        "banks {}; parse errors {}; index/header entries {}",
        scan.banks,
        scan.parse_errors.len(),
        scan.total_entries()
    );
    for e in &scan.parse_errors {
        let _ = writeln!(out, "  PARSE ERROR {e}");
    }
    let _ = writeln!(out, "entries by class: {:?}", scan.class_counts);
    let _ = writeln!(out, "wave codecs: {:?}", scan.codec_counts);
    let _ = writeln!(
        out,
        "decode: {} attempted, {} ok, {} failed",
        total_decode,
        scan.decode_ok.values().sum::<usize>(),
        scan.decode_fail.values().sum::<usize>()
    );
    let mut labels: Vec<&'static str> = scan
        .decode_ok
        .keys()
        .chain(scan.decode_fail.keys())
        .copied()
        .collect();
    labels.sort_unstable();
    labels.dedup();
    for label in labels {
        let _ = writeln!(
            out,
            "  {label}: ok {}, failed {}",
            scan.decode_ok.get(label).copied().unwrap_or(0),
            scan.decode_fail.get(label).copied().unwrap_or(0)
        );
    }
    for f in &scan.decode_failures {
        let _ = writeln!(out, "  DECODE FAIL {f}");
    }
    let _ = writeln!(
        out,
        "byte accounting: {} file(s) with unparsed bytes, {} bytes total",
        scan.unparsed_files, scan.unparsed_bytes
    );
    let _ = writeln!(
        out,
        "linkage: {} .uax packages, {} Sound exports, {} SndStep exports, {} distinct WavRes names, \
         {} Sound exports resolve to a bank name",
        linkage.packages,
        linkage.sounds,
        linkage.steps,
        scan.names.len(),
        linkage.matched_sounds
    );

    let failed = !scan.parse_errors.is_empty() || !scan.decode_fail.is_empty();

    if let Some(path) = &json_out {
        let v = serde_json::json!({
            "root": root.display().to_string(),
            "banks": scan.banks,
            "parse_errors": scan.parse_errors,
            "entries_by_class": scan.class_counts,
            "wave_codecs": scan.codec_counts,
            "decode_ok": scan.decode_ok,
            "decode_fail": scan.decode_fail,
            "unparsed_files": scan.unparsed_files,
            "unparsed_bytes": scan.unparsed_bytes,
            "distinct_names": scan.names.len(),
            "linkage": {
                "uax_packages": linkage.packages,
                "sounds": linkage.sounds,
                "steps": linkage.steps,
                "matched_sounds": linkage.matched_sounds,
            },
        });
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
        let _ = writeln!(out, "wrote {}", path.display());
    }

    print!("{out}");
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

// ------------------------------------------------------------------ export

fn export(args: &[String]) -> ExitCode {
    let mut bank_path = None;
    let mut selector = None;
    let mut wav = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--entry" => match it.next() {
                Some(v) => selector = Some(v.clone()),
                None => return usage_error("--entry needs an index or name"),
            },
            "--wav" => match it.next() {
                Some(v) => wav = Some(PathBuf::from(v)),
                None => return usage_error("--wav needs an output path"),
            },
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if bank_path.is_none() => bank_path = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let (Some(bank_path), Some(selector), Some(wav)) = (bank_path, selector, wav) else {
        return usage_error("audio export needs <bank>, --entry and --wav");
    };
    if let Some(dir) = bank_path.parent()
        && path_inside(&wav, dir)
    {
        eprintln!(
            "error: refusing to write {} inside the bank directory (installation)",
            wav.display()
        );
        return ExitCode::from(2);
    }

    let (bytes, bank) = match read_bank(&bank_path) {
        Ok(v) => v,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::from(1);
        }
    };
    let Some(index) = bank.find_entry(&selector) else {
        eprintln!("error: no entry '{selector}' in {}", bank_path.display());
        return ExitCode::from(1);
    };
    let Some(wave) = bank.entries[index].as_wave() else {
        eprintln!("error: entry {index} is not a wave");
        return ExitCode::from(1);
    };
    let stream = if matches!(wave.data, DataLocation::External { .. }) {
        match read_stream(&bank_path, wave) {
            Some(s) => Some(s),
            None => {
                eprintln!(
                    "error: external stream {} not found next to {}",
                    wave.resource_name.as_deref().unwrap_or("?"),
                    bank_path.display()
                );
                return ExitCode::from(1);
            }
        }
    } else {
        None
    };

    let audio = match decode_entry(&bank, index, &bytes, stream.as_deref()) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("error: decode entry {index}: {e}");
            return ExitCode::from(1);
        }
    };
    let data = write_wav(&audio);
    if let Err(e) = std::fs::write(&wav, &data) {
        eprintln!("error: cannot write {}: {e}", wav.display());
        return ExitCode::from(2);
    }
    println!(
        "entry {index} {} {} ch {} Hz {} samples -> {} ({} bytes)",
        wave.codec.as_str(),
        audio.channels,
        audio.sample_rate,
        audio.samples.len(),
        wav.display(),
        data.len()
    );
    ExitCode::SUCCESS
}

// ------------------------------------------------------------------ linkage

#[derive(Default)]
struct Linkage {
    packages: usize,
    sounds: usize,
    steps: usize,
    matched_sounds: usize,
}

/// Counts `.uax` Sound/SndStep exports and how many Sound leaves match a bank WavRes name.
fn scan_linkage(root: &Path, names: &BTreeMap<String, Vec<HxRef>>) -> Linkage {
    let mut uaxs = Vec::new();
    walk_files(root, "uax", &mut uaxs);
    uaxs.sort();

    let mut link = Linkage::default();
    for path in &uaxs {
        let Ok(data) = std::fs::read(path) else {
            continue;
        };
        let Ok(package) = Package::parse(&data, &Limits::default()) else {
            continue;
        };
        link.packages += 1;
        for i in 0..package.exports().len() {
            let class = package.export_class_path(i).unwrap_or("");
            if is_sound_class(class) {
                link.sounds += 1;
                if let Some(leaf) = export_leaf(&package, i)
                    && names.contains_key(&leaf.to_lowercase())
                {
                    link.matched_sounds += 1;
                }
            } else if is_step_class(class) {
                link.steps += 1;
            }
        }
    }
    link
}

fn is_sound_class(class: &str) -> bool {
    class.ends_with(".Sound") || class == "Sound"
}

fn is_step_class(class: &str) -> bool {
    class.ends_with("SndXIIIStep") || class.ends_with("SndPNJStep")
}

/// Leaf name of an export's object path (after the last `.`).
fn export_leaf(package: &Package, index: usize) -> Option<String> {
    let path = package.object_path(ObjectRef::Export(index as u32))?;
    Some(path.rsplit('.').next().unwrap_or(path).to_owned())
}

/// Single-object property references of a step export whose leaf name is `target`,
/// plus any Sound references regardless of target (used for tracing).
fn step_sound_refs(data: &[u8], package: &Package, index: usize) -> Vec<(String, String)> {
    let mut refs = Vec::new();
    let Ok(props) = package.read_object_properties(data, index, &Limits::default()) else {
        return refs;
    };
    for prop in &props.block.properties {
        let name = package.property_name(prop).to_owned();
        if !matches!(
            name.as_str(),
            "EndStep" | "LandSound" | "RightSteps" | "LeftSteps"
        ) {
            continue;
        }
        if let PropertyValue::Object(r) | PropertyValue::Class(r) = &prop.value
            && let Some(path) = package.object_path(*r)
            && let Some(leaf) = path.rsplit('.').next()
        {
            refs.push((name, leaf.to_owned()));
        }
    }
    refs
}

fn link(args: &[String]) -> ExitCode {
    let mut root = None;
    let mut sound = None;
    let mut wav = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--sound" => match it.next() {
                Some(v) => sound = Some(v.clone()),
                None => return usage_error("--sound needs a name"),
            },
            "--wav" => match it.next() {
                Some(v) => wav = Some(PathBuf::from(v)),
                None => return usage_error("--wav needs an output path"),
            },
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if root.is_none() => root = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(root) = root else {
        return usage_error("audio link needs an installation root");
    };
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return ExitCode::from(2);
    }
    if let Some(dir) = wav.as_ref().and_then(|w| w.parent())
        && path_inside(dir, &root)
    {
        eprintln!("error: refusing to write WAV inside the installation");
        return ExitCode::from(2);
    }

    let scan = analyze(&root, false);
    let linkage = scan_linkage(&root, &scan.names);
    let mut out = String::new();
    let _ = writeln!(
        out,
        "linkage: {} packages, {} Sound exports, {} SndStep exports, {} distinct WavRes names, \
         {} Sound exports matched a bank name",
        linkage.packages,
        linkage.sounds,
        linkage.steps,
        scan.names.len(),
        linkage.matched_sounds
    );

    if let Some(target) = &sound {
        let key = target.to_lowercase();
        let _ = writeln!(out, "trace sound '{target}':");
        let matches = scan.names.get(&key).cloned().unwrap_or_default();
        for m in &matches {
            let _ = writeln!(
                out,
                "  HX {}#{} {} {} ch {} Hz{}",
                m.bank.display(),
                m.entry,
                m.codec.as_str(),
                m.channels,
                m.sample_rate,
                if m.external { " (streamed)" } else { "" }
            );
        }
        if matches.is_empty() {
            let _ = writeln!(out, "  no bank WavRes named '{target}'");
        }
        if let Some(wav) = &wav {
            let Some(first) = matches.first() else {
                eprintln!("error: no bank entry for '{target}'");
                return ExitCode::from(1);
            };
            match export_ref(first, wav) {
                Ok(()) => {
                    let _ = writeln!(out, "  wrote {}", wav.display());
                }
                Err(e) => {
                    eprintln!("error: {e}");
                    return ExitCode::from(1);
                }
            }
        }
    } else {
        // Show the three traced chains: weapon, footstep (through a SndStep) and dialogue.
        for (package_name, export_name, label) in [
            ("XIIISound.uax", "Guns.M16Fire1", "weapon"),
            ("Footsteps.uax", "XIIIFSMar", "footstep step"),
            ("Plage00Voices.uax", "Plage00_XIIIa_00", "dialogue"),
        ] {
            let _ = writeln!(out, "trace ({label}):");
            trace_export(&root, package_name, export_name, &scan, &mut out);
        }
    }

    print!("{out}");
    ExitCode::SUCCESS
}

fn trace_export(root: &Path, package_name: &str, export_name: &str, scan: &Scan, out: &mut String) {
    let mut uaxs = Vec::new();
    walk_files(root, "uax", &mut uaxs);
    let Some(path) = uaxs.into_iter().find(|p| {
        p.file_name()
            .is_some_and(|n| n.eq_ignore_ascii_case(package_name))
    }) else {
        let _ = writeln!(out, "  {package_name}: not found");
        return;
    };
    let Ok(data) = std::fs::read(&path) else {
        let _ = writeln!(out, "  {package_name}: unreadable");
        return;
    };
    let Ok(package) = Package::parse(&data, &Limits::default()) else {
        let _ = writeln!(out, "  {package_name}: parse failed");
        return;
    };
    let Some(i) = (0..package.exports().len()).find(|&i| {
        package
            .object_path(ObjectRef::Export(i as u32))
            .is_some_and(|p| p.eq_ignore_ascii_case(export_name))
    }) else {
        let _ = writeln!(out, "  {package_name}: export '{export_name}' not found");
        return;
    };
    let class = package.export_class_path(i).unwrap_or("?");
    let _ = writeln!(out, "  {package_name} {export_name} [{class}] (export {i})");
    if is_step_class(class) {
        for (prop, leaf) in step_sound_refs(&data, &package, i) {
            let _ = writeln!(out, "    {prop} -> Sound {leaf}");
            match scan.names.get(&leaf.to_lowercase()) {
                Some(matches) if !matches.is_empty() => {
                    let m = &matches[0];
                    let _ = writeln!(
                        out,
                        "      HX {}#{} {} {} ch {} Hz{}",
                        m.bank.display(),
                        m.entry,
                        m.codec.as_str(),
                        m.channels,
                        m.sample_rate,
                        if m.external { " (streamed)" } else { "" }
                    );
                }
                _ => {
                    let _ = writeln!(out, "      no bank WavRes named '{leaf}'");
                }
            }
        }
    }
    if let Some(leaf) = export_leaf(&package, i) {
        match scan.names.get(&leaf.to_lowercase()) {
            Some(matches) if !matches.is_empty() => {
                let m = &matches[0];
                let _ = writeln!(
                    out,
                    "    Sound leaf '{leaf}' -> HX {}#{} {} {} ch {} Hz{}",
                    m.bank.display(),
                    m.entry,
                    m.codec.as_str(),
                    m.channels,
                    m.sample_rate,
                    if m.external { " (streamed)" } else { "" }
                );
            }
            _ => {
                let _ = writeln!(out, "    Sound leaf '{leaf}' -> no bank WavRes name match");
            }
        }
    }
}

fn export_ref(hx_ref: &HxRef, wav: &Path) -> Result<(), String> {
    let (bytes, bank) = read_bank(&hx_ref.bank)?;
    let wave = bank.entries[hx_ref.entry]
        .as_wave()
        .ok_or_else(|| format!("entry {} is not a wave", hx_ref.entry))?;
    let stream = if matches!(wave.data, DataLocation::External { .. }) {
        read_stream(&hx_ref.bank, wave)
    } else {
        None
    };
    let audio = decode_entry(&bank, hx_ref.entry, &bytes, stream.as_deref())
        .map_err(|e| format!("decode {}#{}: {e}", hx_ref.bank.display(), hx_ref.entry))?;
    std::fs::write(wav, write_wav(&audio))
        .map_err(|e| format!("cannot write {}: {e}", wav.display()))
}
