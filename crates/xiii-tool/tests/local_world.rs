//! Opt-in world-decoder coverage against the user's GOG installation (read-only).
//!
//! Skipped (with a printed "SKIPPED" line) unless `XIII_GOG_DIR` is set:
//!
//! ```text
//! XIII_GOG_DIR=P:/AI/XIII/XIII_Game cargo test -p xiii-tool --test local_world -- --nocapture
//! ```
//!
//! Expected numbers: measured GOG results recorded in `crates/xiii-decode/README.md`.

use std::path::PathBuf;

use xiii_tool::world_cmd::{ClassSelection, scan_world};

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
fn gog_world_decoders_cover_every_export() {
    let Some(root) = gog_root() else { return };
    let cov = scan_world(&root, ClassSelection::all()).expect("scan");
    println!("{}", xiii_tool::world_cmd::coverage_text(&cov));
    assert_eq!(cov.packages, 196);
    let expect = [
        ("Engine.Texture", 3507, 0),
        ("Engine.Palette", 14, 0),
        ("Engine.StaticMesh", 6128, 0),
        ("Engine.Polys", 7194, 0),
        ("Engine.TerrainSector", 2208, 0),
        // Prefix decoders with an explicit unsupported tail on every export.
        ("Engine.Model", 7194, 7194),
        ("Engine.TerrainInfo", 18, 18),
    ];
    for (class, n, partial) in expect {
        let c = &cov.classes[class];
        assert_eq!(
            (c.attempted, c.ok, c.failed(), c.partial),
            (n, n, 0, partial),
            "{class}: {:?}",
            c.examples
        );
    }
    // Every texture mip decodes to RGBA8 (9 smallest mips are stored empty).
    assert!(
        cov.texture_formats
            .values()
            .all(|[a, b, c]| a == b && *c == 0)
    );
    assert_eq!(cov.counters.get("texture.empty_mips"), Some(&9));
    // Static-mesh winding: clockwise source front faces (>99 % of triangles).
    let [against, along, _] = cov.mesh_winding;
    assert!(against > 100 * along, "against {against} along {along}");
}
