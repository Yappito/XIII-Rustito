//! Opt-in property and dependency checks against the user's GOG installation (read-only).
//!
//! Skipped (with a printed "SKIPPED" line) unless `XIII_GOG_DIR` is set:
//!
//! ```text
//! XIII_GOG_DIR=P:/AI/XIII/XIII_Game cargo test -p xiii-tool --test local_properties -- --nocapture
//! ```
//!
//! The expected numbers are the measured GOG results recorded in
//! `crates/xiii-package/README.md` and `docs/evidence/property-coverage-gog.json`.

use std::fs;
use std::path::PathBuf;

use xiii_package::{Limits, Package};
use xiii_tool::{coverage, deps};

fn gog_root() -> Option<PathBuf> {
    match std::env::var_os("XIII_GOG_DIR") {
        Some(r) => Some(PathBuf::from(r)),
        None => {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            None
        }
    }
}

#[test]
fn gog_property_blocks_decode_outside_class_payloads() {
    let Some(root) = gog_root() else { return };
    let cov = coverage::scan(&root).expect("scan");
    assert_eq!(cov.packages, 196);
    assert!(cov.package_errors.is_empty(), "{:?}", cov.package_errors);
    let t = coverage::totals(&cov);
    assert_eq!(t.exports, 141_959);
    assert_eq!(t.attempted, 141_939);
    // Every export decodes, including Core.Class defaults (located after the native
    // UField/UStruct/UState/UClass data by xiii-script).
    for (class, s) in &cov.classes {
        assert_eq!(s.failed, 0, "{class}: {:?}", s.first_error);
    }
    let classes = &cov.classes["Core.Class"];
    assert_eq!(
        (classes.attempted, classes.ok, classes.tail_zero),
        (1444, 1444, 1444)
    );
    assert_eq!(t.ok_with_anomalies, 0, "{:?}", cov.anomaly_examples);
    assert!(cov.anomalies.is_empty());
    // Measured encoding facts.
    assert_eq!(cov.bool_sizes.keys().copied().collect::<Vec<_>>(), [0]);
    assert_eq!(
        cov.raw_structs_tagged_like[0], 0,
        "struct values are not tagged in v100"
    );
    assert_eq!(cov.frame_node_null, 0);
    assert_eq!(cov.frame_node_set, cov.frame_node_is_state);
    assert_eq!(cov.frame_offsets.keys().copied().collect::<Vec<_>>(), [-1]);
    // Two-byte array indices occur only in class defaults (static arrays up to index 254).
    assert_eq!(cov.array_index_two_byte, 130);
    assert_eq!(cov.array_index_four_byte, 0);
    assert_eq!(cov.array_index_max, 254);
    println!(
        "GOG: {} exports attempted, {} decoded, {} failures, {} empty tails",
        t.attempted,
        t.decoded(),
        t.failed,
        t.tail_zero
    );
}

fn check_map(root: &std::path::Path, map: &str, imports: u64, resolved: u64, packages: usize) {
    let index = deps::index_packages(root).expect("index");
    let path = root.join("Maps").join(map);
    let data = fs::read(&path).expect("map");
    let p = Package::parse(&data, &Limits::default()).expect("parse map");
    let r = deps::analyze(map, &p, &data, &index);
    print!("{}", deps::report_text(&r));
    assert!(r.property_failures.is_empty(), "{:?}", r.property_failures);
    assert!(r.missing_packages().is_empty());
    assert_eq!(r.packages.len(), packages);
    assert_eq!((r.imports(), r.resolved()), (imports, resolved));
    assert_eq!(r.failures(), 0);
    let engine = r.packages.iter().find(|d| d.name == "Engine").unwrap();
    let mut absent = engine.absent_classes.clone();
    absent.sort();
    assert_eq!(
        absent,
        [
            "Engine.Level",
            "Engine.Model",
            "Engine.Polys",
            "Engine.StaticMeshInstance",
            "Engine.TerrainSector"
        ]
    );
    assert!(r.is_clean());
    let [total, mesh, location, ..] = r.static_mesh_actors;
    assert!(total > 100);
    assert_eq!((mesh, location), (total, total));
    let static_plage = r
        .packages
        .iter()
        .find(|d| d.name == "StaticPlage2")
        .unwrap();
    assert_eq!(static_plage.resolved, static_plage.imports);
    assert!(static_plage.property_refs > 0);
}

#[test]
fn gog_plage_maps_resolve_imports_to_exports() {
    let Some(root) = gog_root() else { return };
    check_map(&root, "Plage00.unr", 156, 151, 17);
    check_map(&root, "Plage01.unr", 351, 346, 20);
}
