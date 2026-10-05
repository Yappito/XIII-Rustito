//! `xiii-tool locale`: coverage of the installation's `.int`-family files and
//! `Localize`-style lookups.
//!
//! Read-only. The installation is opened through `xiii-install`, and every
//! localisation file it lists is parsed by `xiii-locale`.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Instant;

use xiii_install::{Installation, OpenOptions};
use xiii_locale::{Localizer, ParseWarningKind};

const USAGE: &str = "\
xiii-tool locale: UE2 .int localisation files (read-only)

USAGE:
  xiii-tool locale coverage <install-root>
      Open the installation, parse every .int/.frt/.det/.est/.itt/... file and
      print file/language/entry counts, encodings, hard errors and tolerated
      warnings (file:line). Exits 1 on any hard error or unreadable file.

  xiii-tool locale get <Package> <Section> <Key> [--game-dir <install-root>]
      Resolve one Localize(Section, Key, Package) string in the install's
      active language, falling back to int. The root comes from --game-dir or
      the XIII_GOG_DIR environment variable. Exits 1 when the key is absent.

The active language is [Engine.Engine] Language= from Default.ini (or XIII.ini).";

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

fn open_install(root: &PathBuf) -> Result<Installation, ExitCode> {
    if !root.is_dir() {
        eprintln!("error: {} is not a directory", root.display());
        return Err(ExitCode::from(2));
    }
    match Installation::open(root, &OpenOptions::default()) {
        Ok(i) => Ok(i),
        Err(e) => {
            eprintln!("error: cannot open {}: {e}", root.display());
            Err(ExitCode::from(2))
        }
    }
}

/// `xiii-tool locale` entry point.
pub fn run(args: &[String]) -> ExitCode {
    match args.first().map(String::as_str) {
        Some("coverage") => coverage(&args[1..]),
        Some("get") => get(&args[1..]),
        Some("-h" | "--help" | "help") | None => {
            println!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => usage_error(&format!("unknown locale subcommand '{other}'")),
    }
}

fn coverage(args: &[String]) -> ExitCode {
    let mut root: Option<PathBuf> = None;
    for a in args {
        match a.as_str() {
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s if root.is_none() => root = Some(PathBuf::from(s)),
            s => return usage_error(&format!("unexpected argument '{s}'")),
        }
    }
    let Some(root) = root else {
        return usage_error("locale coverage needs an installation root");
    };
    let started = Instant::now();
    let inst = match open_install(&root) {
        Ok(i) => i,
        Err(code) => return code,
    };
    let localizer = Localizer::from_installation(&inst);

    let mut out = String::new();
    let _ = writeln!(out, "locale coverage: {}", inst.root().display());
    let _ = writeln!(
        out,
        "profile: {}; active language: {}",
        inst.profile(),
        localizer.language()
    );

    let mut by_language: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_encoding: BTreeMap<String, usize> = BTreeMap::new();
    let mut sections = 0usize;
    let mut entries = 0usize;
    let mut duplicates = 0usize;
    let mut files = 0usize;
    let mut warnings: BTreeMap<ParseWarningKind, usize> = BTreeMap::new();
    let mut hard_errors = 0usize;
    let mut decode_errors = 0usize;
    let mut warning_lines: Vec<String> = Vec::new();
    let mut error_lines: Vec<String> = Vec::new();

    for f in localizer.files() {
        files += 1;
        *by_language.entry(f.language.as_str()).or_default() += 1;
        *by_encoding.entry(f.encoding.to_string()).or_default() += 1;
        sections += f.section_count();
        entries += f.entry_count();
        duplicates += f.duplicate_keys;
        if let Some(e) = &f.decode_error {
            decode_errors += 1;
            error_lines.push(format!("  decode {}: {e}", f.relative));
        }
        for e in &f.errors {
            hard_errors += 1;
            error_lines.push(format!(
                "  error {}:{}: {} ({})",
                f.relative, e.line, e.text, e.kind
            ));
        }
        for w in &f.warnings {
            *warnings.entry(w.kind).or_default() += 1;
            warning_lines.push(format!("  warning {}:{}: {}", f.relative, w.line, w.kind));
        }
    }
    for e in localizer.io_errors() {
        hard_errors += 1;
        error_lines.push(format!("  unreadable {e}"));
    }

    let languages = by_language
        .iter()
        .map(|(l, n)| format!("{l}: {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let encodings = by_encoding
        .iter()
        .map(|(e, n)| format!("{e}: {n}"))
        .collect::<Vec<_>>()
        .join(", ");
    let warn_total: usize = warnings.values().sum();
    let warn_kinds = warnings
        .iter()
        .map(|(k, n)| format!("{k}: {n}"))
        .collect::<Vec<_>>()
        .join("; ");
    let _ = writeln!(out, "files: {files} ({languages})");
    let _ = writeln!(out, "encodings: {encodings}");
    let _ = writeln!(
        out,
        "sections: {sections}; distinct entries: {entries}; overwritten duplicate keys: {duplicates}"
    );
    let _ = writeln!(
        out,
        "hard errors: {hard_errors} (decode {decode_errors}); warnings: {warn_total} \
         ({warn_kinds})"
    );
    for line in &warning_lines {
        let _ = writeln!(out, "{line}");
    }
    for line in &error_lines {
        let _ = writeln!(out, "{line}");
    }
    let failed = hard_errors > 0;
    let _ = writeln!(out, "result: {}", if failed { "FAIL" } else { "OK" });
    let _ = writeln!(out, "elapsed: {:.3} s", started.elapsed().as_secs_f64());
    emit(&out);
    if failed {
        ExitCode::from(1)
    } else {
        ExitCode::SUCCESS
    }
}

fn get(args: &[String]) -> ExitCode {
    let mut positional: Vec<String> = Vec::new();
    let mut game_dir: Option<PathBuf> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--game-dir" => match it.next() {
                Some(p) => game_dir = Some(PathBuf::from(p)),
                None => return usage_error("--game-dir needs a directory"),
            },
            s if s.starts_with("--") => return usage_error(&format!("unknown option '{s}'")),
            s => positional.push(s.to_owned()),
        }
    }
    let [package, section, key] = positional.as_slice() else {
        return usage_error("locale get needs <Package> <Section> <Key>");
    };
    let root = match game_dir.or_else(|| {
        std::env::var_os("XIII_GOG_DIR")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    }) {
        Some(r) => r,
        None => {
            eprintln!("error: no install root; pass --game-dir or set XIII_GOG_DIR");
            return ExitCode::from(2);
        }
    };
    let inst = match open_install(&root) {
        Ok(i) => i,
        Err(code) => return code,
    };
    let localizer = Localizer::from_installation(&inst);
    match localizer.get(package, section, key) {
        Some(value) => {
            println!("{package}.{section}.{key} = {value}");
            ExitCode::SUCCESS
        }
        None => {
            eprintln!(
                "error: no value for {package}.{section}.{key} in {} (language {})",
                inst.root().display(),
                localizer.language()
            );
            ExitCode::from(1)
        }
    }
}
