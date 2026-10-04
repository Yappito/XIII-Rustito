//! Opt-in integration tests against the user's owned installations (read-only).
//!
//! Skipped (with a printed "SKIPPED" line) unless the environment variables are set:
//!
//! ```text
//! XIII_GOG_DIR=P:/AI/XIII/XIII_Game cargo test -p xiii-tool --test local_corpus -- --nocapture
//! XIII_STEAM_DIR="P:/SteamLibrary/steamapps/common/XIII - Classic" cargo test -p xiii-tool --test local_corpus -- --nocapture
//! ```
//!
//! Each run compares every tagged package with the saved Python probe inventory in
//! `docs/evidence/` and checks the measured summary layout (header ends at the first table,
//! newest generation equals the export/name counts, no unaccounted or overlapping bytes).

use std::path::{Path, PathBuf};

use xiii_package::Limits;
use xiii_tool::corpus::{compare, load_inventory, scan};

fn evidence(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/evidence")
        .join(name)
}

fn run(var: &str, inventory: &str, expected_packages: usize) {
    let Some(root) = std::env::var_os(var) else {
        println!("SKIPPED: set {var} to an installation root to run this corpus test");
        return;
    };
    let root = PathBuf::from(root);
    let outcomes = scan(&root, &Limits::default()).expect("installation scan");
    let errors: Vec<_> = outcomes
        .iter()
        .filter_map(|o| {
            o.result
                .as_ref()
                .err()
                .map(|e| format!("{}: {e}", o.rel_path))
        })
        .collect();
    assert!(errors.is_empty(), "parse errors: {errors:#?}");
    assert_eq!(outcomes.len(), expected_packages, "tagged package count");
    for o in &outcomes {
        let p = o.result.as_ref().unwrap();
        assert_eq!(p.dialect.0, 100, "{}", o.rel_path);
        assert!(
            matches!(p.dialect.1, 50 | 56 | 57 | 58),
            "{} licensee {}",
            o.rel_path,
            p.dialect.1
        );
        assert_eq!(p.header_gap, 0, "{}", o.rel_path);
        assert_eq!(p.latest_generation_matches, Some(true), "{}", o.rel_path);
        assert_eq!(p.unaccounted_bytes, 0, "{}", o.rel_path);
        assert_eq!(p.overlaps, 0, "{}", o.rel_path);
    }
    let inv = load_inventory(&evidence(inventory)).expect("inventory");
    let cmp = compare(&outcomes, &inv);
    assert!(cmp.is_clean(), "comparison with {inventory}: {cmp:#?}");
    assert_eq!(cmp.matched, expected_packages);
    println!("{var}: {} packages matched {inventory}", cmp.matched);
}

#[test]
fn gog_corpus_matches_inventory() {
    run("XIII_GOG_DIR", "gog-inventory.json", 196);
}

#[test]
fn steam_corpus_matches_inventory() {
    run("XIII_STEAM_DIR", "steam-inventory.json", 213);
}
