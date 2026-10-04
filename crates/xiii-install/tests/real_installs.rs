//! Opt-in checks against real, user-owned installations (read-only).
//!
//! Set `XIII_GOG_DIR` and/or `XIII_STEAM_DIR` to installation roots, then run
//! `cargo test -p xiii-install --test real_installs -- --nocapture`.
//! Each test prints `SKIPPED` and passes when its variable is unset.

use std::path::PathBuf;

use xiii_install::{ContentProfile, Installation, OpenOptions, PackageKind, Severity};

const NAMES: &[&str] = &[
    "Plage00",
    "Plage01",
    "StaticPlage2",
    "XIIIPersos",
    "XIDMaps",
    "XIII",
    "XIDCine",
    "Engine",
    "Core",
];

fn env_root(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => {
            println!("SKIPPED: {var} is not set");
            None
        }
    }
}

fn open_and_report(var: &str) -> Option<Installation> {
    let root = env_root(var)?;
    let inst =
        Installation::open(&root, &OpenOptions::default()).unwrap_or_else(|e| panic!("{var}: {e}"));
    let det = inst.detection();
    println!("== {var} = {}", inst.root().display());
    println!("profile: {}", inst.profile());
    for e in &det.evidence {
        println!("  evidence {:?}: {}", e.signal, e.detail);
    }
    for r in &det.unknown_reasons {
        println!("  unknown reason: {r}");
    }
    println!(
        "inventory: {} files, {} dirs, excluded {:?}, links skipped {}",
        inst.inventory().files.len(),
        inst.inventory().directories.len(),
        inst.inventory().excluded_directories,
        inst.inventory().skipped_links.len()
    );
    for kind in PackageKind::ALL {
        let n = inst.packages().filter(|p| p.kind == kind).count();
        if n > 0 {
            println!("  {kind}: {n}");
        }
    }
    for r in inst.search_roots() {
        println!(
            "  root {:<14} -> {:<14} {:>3} {}",
            r.logical, r.relative, r.packages, r.kind
        );
    }
    println!(
        "logical names: {}, conflicts: {}",
        inst.logical_name_count(),
        inst.conflicts().count()
    );
    for ev in inst.ini_evidence() {
        println!("  ini {}: Paths={:?}", ev.file, ev.fields.paths);
        println!(
            "    search roots not in Paths: {:?}",
            ev.search_roots_not_in_paths
        );
    }
    for c in inst.specific_packages() {
        println!("  SpecificPackage {} -> {:?}", c.name, c.found);
    }
    for d in inst.diagnostics() {
        println!("  diag {d}");
    }
    for name in NAMES {
        let r = inst
            .resolve_package(name)
            .unwrap_or_else(|e| panic!("{var}: {e}"));
        println!("  {name:<13} -> {}", r.entry.relative);
        assert!(r.path().starts_with(inst.root()), "{name} escaped root");
        assert!(r.path().is_file());
    }
    assert!(inst.resolve_map("Plage00").is_ok());
    assert_eq!(inst.conflicts().count(), 0, "unexpected duplicates");
    assert!(
        inst.diagnostics()
            .iter()
            .all(|d| d.severity != Severity::Warning
                || d.code == xiii_install::DiagnosticCode::Unreadable),
        "warnings present"
    );
    Some(inst)
}

#[test]
fn gog_installation() {
    let Some(inst) = open_and_report("XIII_GOG_DIR") else {
        return;
    };
    assert_eq!(inst.profile(), ContentProfile::Gog);
    assert_eq!(
        inst.resolve_package("XIIIPersos")
            .unwrap()
            .entry
            .search_root,
        "system/pc"
    );
    assert!(inst.resolve_package("XIIIPlus").is_err());
}

#[test]
fn steam_installation() {
    let Some(inst) = open_and_report("XIII_STEAM_DIR") else {
        return;
    };
    assert_eq!(inst.profile(), ContentProfile::SteamPatched);
    assert_eq!(
        inst.resolve_map("Plage00").unwrap().entry.search_root,
        "maps/basesp"
    );
    assert!(inst.resolve_package("XIIIPlus").is_ok());
}

#[test]
fn gog_and_steam_resolve_to_their_own_roots() {
    let (Some(g), Some(s)) = (env_root("XIII_GOG_DIR"), env_root("XIII_STEAM_DIR")) else {
        return;
    };
    let gi = Installation::open(&g, &OpenOptions::default()).unwrap();
    let si = Installation::open(&s, &OpenOptions::default()).unwrap();
    assert_ne!(gi.root(), si.root());
    for name in NAMES {
        let a = gi.resolve_package(name).unwrap();
        let b = si.resolve_package(name).unwrap();
        assert!(a.path().starts_with(gi.root()) && !a.path().starts_with(si.root()));
        assert!(b.path().starts_with(si.root()) && !b.path().starts_with(gi.root()));
        assert_eq!(a.entry.kind, b.entry.kind, "{name}");
    }
}

/// Prints best-effort discovery results; never asserts on machine layout.
#[test]
fn discovery_report() {
    if std::env::var_os("XIII_STEAM_DIR").is_none() && std::env::var_os("XIII_GOG_DIR").is_none() {
        println!("SKIPPED: neither XIII_GOG_DIR nor XIII_STEAM_DIR is set");
        return;
    }
    for c in xiii_install::discover_candidates() {
        println!(
            "candidate {} valid={} via {:?}",
            c.path.display(),
            c.looks_valid,
            c.source
        );
    }
}
