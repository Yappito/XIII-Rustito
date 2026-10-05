//! Opt-in check against the real, user-owned GOG installation (read-only).
//!
//! Set `XIII_GOG_DIR` to the installation root and run
//! `cargo test -p xiii-locale --test local_corpus -- --nocapture`.
//! Without the variable the test prints `SKIPPED` and passes.

use std::collections::BTreeSet;
use std::path::PathBuf;

use xiii_install::{Installation, OpenOptions};
use xiii_locale::Localizer;

fn env_root() -> Option<PathBuf> {
    match std::env::var_os("XIII_GOG_DIR") {
        Some(v) if !v.is_empty() => Some(PathBuf::from(v)),
        _ => {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            None
        }
    }
}

#[test]
fn gog_localisation_parses_without_errors_and_resolves_known_keys() {
    let Some(root) = env_root() else {
        return;
    };
    let inst = Installation::open(&root, &OpenOptions::default()).unwrap_or_else(|e| panic!("{e}"));
    let loc = Localizer::from_installation(&inst);

    // Measured inventory of the GOG corpus (2026-10-05).
    assert_eq!(loc.language(), "int", "install language");
    assert_eq!(loc.files().count(), 216, "localisation file count");

    let languages: BTreeSet<&str> = loc.files().map(|f| f.language.as_str()).collect();
    assert_eq!(
        languages,
        BTreeSet::from(["int", "frt", "det", "est", "itt"]),
        "shipped languages"
    );

    // Every file decodes and has zero hard line errors.
    assert!(
        loc.io_errors().is_empty(),
        "unreadable: {:?}",
        loc.io_errors()
    );
    for f in loc.files() {
        assert!(
            f.decode_error.is_none(),
            "{}: decode error {:?}",
            f.relative,
            f.decode_error
        );
        assert!(
            f.errors.is_empty(),
            "{}: hard errors {:?}",
            f.relative,
            f.errors
        );
    }

    // The only tolerated issues are the 6 missing separators and 1 unterminated
    // quote in the Italian files; they are counted with line numbers.
    let warnings: Vec<_> = loc.parse_warnings().collect();
    println!(
        "files={} sections={} entries={} duplicates={} warnings={}",
        loc.files().count(),
        loc.files().map(|f| f.section_count()).sum::<usize>(),
        loc.files().map(|f| f.entry_count()).sum::<usize>(),
        loc.files().map(|f| f.duplicate_keys).sum::<usize>(),
        warnings.len()
    );
    for (f, w) in &warnings {
        println!("  warning {}:{}: {}", f.relative, w.line, w.kind);
    }
    assert_eq!(warnings.len(), 7, "measured tolerated issues");

    // Known `Localize(Section, Key, Package)` results (English files).
    assert_eq!(
        loc.get("Plage01", "Plage01CahuteKeyPick0", "PickupMessage"),
        Some("First-aid post key.")
    );
    assert_eq!(
        loc.get("XIDMaps", "Plage01CahuteKeyPick", "PickupMessage"),
        Some("Cahute Key")
    );
    assert_eq!(
        loc.get("XIII", "XIIIMover", "LockedMessage"),
        Some("This door is locked.")
    );
    assert_eq!(
        loc.get("XIII", "XIIIDialogMessage", "sEnoughAmmo"),
        Some("I can't hold more ammo")
    );
    assert_eq!(
        loc.get("XIII", "XIIIGameInfo", "GameName"),
        Some("Solo Game")
    );
    assert_eq!(
        loc.get("XIDInterf", "XIIIMenu", "LoadGameText"),
        Some("Load game")
    );
    assert_eq!(
        loc.get("XIDInterf", "XIIIWindow", "StartText"),
        Some("Start")
    );

    // `localized` class defaults: section = class, key = property.
    assert_eq!(
        loc.class_property("XIDInterf", "XIIIWindow", "MyFailureMessage", Some(0)),
        Some("The account name already exists.")
    );
    assert_eq!(
        loc.class_property("XIDInterf", "XIIIMenu", "OptionsText", None),
        Some("Options")
    );

    // Language fallback: `OKText` is present in xidinterf.int but absent from
    // XIDInterf.frt, so the French lookup returns the English value.
    assert_eq!(
        loc.get("xidinterf", "XIIIMenuMultiGSAllConfig", "OKText"),
        Some("Ok")
    );
    assert_eq!(
        loc.get_for_language("xidinterf", "XIIIMenuMultiGSAllConfig", "OKText", "frt"),
        Some("Ok")
    );
}
