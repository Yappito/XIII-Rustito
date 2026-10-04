//! Scan/compare behaviour on a generated directory tree (no proprietary data).

use std::fs;
use std::path::PathBuf;

use serde_json::json;
use xiii_package::Limits;
use xiii_tool::corpus::{Inventory, compare, scan};

/// Latin-1 FString with a one-byte compact length (texts shorter than 63 bytes).
fn fstring(text: &str) -> Vec<u8> {
    let mut out = vec![u8::try_from(text.len() + 1).unwrap()];
    out.extend(text.as_bytes());
    out.push(0);
    out
}

/// Minimal valid package: names [Core, Package, Thing], import Core (a root package) and one
/// zero-sized export `Thing` whose class is null (i.e. Core.Class).
fn tiny_package(licensee: u16) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(0x9e2a_83c1u32.to_le_bytes());
    b.extend(100u16.to_le_bytes());
    b.extend(licensee.to_le_bytes());
    b.extend(1u32.to_le_bytes());
    b.extend([0u8; 24]);
    b.extend([7u8; 16]);
    b.extend(1i32.to_le_bytes());
    b.extend(1i32.to_le_bytes());
    b.extend(3i32.to_le_bytes());
    let names_at = b.len();
    for n in ["Core", "Package", "Thing"] {
        b.extend(fstring(n));
        b.extend(0u32.to_le_bytes());
    }
    let imports_at = b.len();
    b.extend([0, 1]); // class package Core, class Package
    b.extend(0i32.to_le_bytes());
    b.push(0); // object name Core
    let exports_at = b.len();
    b.extend([0, 0]); // class, super
    b.extend(0i32.to_le_bytes());
    b.push(2); // Thing
    b.extend(0u32.to_le_bytes());
    b.push(0); // size 0, no offset
    for (i, v) in [3, names_at, 1, exports_at, 1, imports_at]
        .into_iter()
        .enumerate()
    {
        b[12 + 4 * i..16 + 4 * i].copy_from_slice(&(v as i32).to_le_bytes());
    }
    b
}

fn fresh_dir(name: &str) -> PathBuf {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join(name);
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn scan_finds_tagged_files_skips_saves_and_compares() {
    let root = fresh_dir("synthetic-install");
    fs::create_dir_all(root.join("Maps")).unwrap();
    fs::create_dir_all(root.join("System/PC")).unwrap();
    fs::create_dir_all(root.join("Save")).unwrap();
    fs::write(root.join("Maps/Test.unr"), tiny_package(58)).unwrap();
    fs::write(root.join("System/PC/Odd.bin"), tiny_package(57)).unwrap();
    fs::write(root.join("Save/Slot.unr"), tiny_package(58)).unwrap();
    fs::write(root.join("System/readme.txt"), b"not a package").unwrap();
    fs::write(root.join("System/short.u"), [0xc1, 0x83]).unwrap();
    let mut broken = tiny_package(58);
    broken.truncate(broken.len() - 3);
    fs::write(root.join("Maps/Broken.unr"), broken).unwrap();

    let outcomes = scan(&root, &Limits::default()).unwrap();
    let paths: Vec<&str> = outcomes.iter().map(|o| o.rel_path.as_str()).collect();
    assert_eq!(
        paths,
        ["Maps/Broken.unr", "Maps/Test.unr", "System/PC/Odd.bin"]
    );
    let err = outcomes[0].result.as_ref().unwrap_err();
    assert!(err.starts_with("exports at offset"), "{err}");
    let ok = outcomes[1].result.as_ref().unwrap();
    assert_eq!(ok.dialect, (100, 58));
    assert_eq!(ok.latest_generation_matches, Some(true));
    assert_eq!(
        (ok.header_gap, ok.unaccounted_bytes, ok.overlaps),
        (0, 0, 0)
    );
    assert_eq!(ok.probe["export_classes"], json!({"Core.Class": 1}));
    assert_eq!(ok.probe["zero_size_exports"], json!({"Core.Class": 1}));
    assert_eq!(ok.probe["imported_packages"], json!(["Core"]));

    // Inventory built from our own output matches; then perturb it.
    let mut inv = Inventory::default();
    // Parse failures are reported by the caller, not compared; list the file so it is not extra.
    inv.packages
        .insert("Maps/Broken.unr".into(), (0, json!({})));
    for o in &outcomes[1..] {
        inv.packages.insert(
            o.rel_path.clone(),
            (o.bytes, o.result.as_ref().unwrap().probe.clone()),
        );
    }
    let cmp = compare(&outcomes, &inv);
    assert_eq!(cmp.matched, 2);
    assert!(cmp.is_clean(), "{cmp:?}");

    let mut bad = inv.clone();
    bad.packages.get_mut("Maps/Test.unr").unwrap().1["names"] = json!(4);
    bad.packages.get_mut("System/PC/Odd.bin").unwrap().1["export_classes"]["Core.Class"] = json!(2);
    let (bytes, pkg) = bad.packages["Maps/Test.unr"].clone();
    bad.packages.insert("Maps/Gone.unr".into(), (bytes, pkg));
    let cmp = compare(&outcomes, &bad);
    assert_eq!(cmp.matched, 0);
    assert_eq!(cmp.mismatches.len(), 2);
    assert_eq!(cmp.missing_on_disk, ["Maps/Gone.unr"]);
    assert!(cmp.mismatches[0].1[0].contains("names: rust 3 vs inventory 4"));
    assert!(cmp.mismatches[1].1[0].contains("export_classes.Core.Class"));

    // A file absent from the inventory is reported; case-only differences still match.
    let mut partial = inv.clone();
    let entry = partial.packages.remove("System/PC/Odd.bin").unwrap();
    partial.packages.insert("system/pc/ODD.bin".into(), entry);
    partial.packages.remove("Maps/Test.unr").unwrap();
    let cmp = compare(&outcomes, &partial);
    assert_eq!(cmp.not_in_inventory, ["Maps/Test.unr"]);
    assert_eq!(cmp.case_only_matches, ["System/PC/Odd.bin"]);
    assert_eq!(cmp.matched, 1);
}
