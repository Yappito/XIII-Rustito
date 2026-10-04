//! `deps` analysis and property coverage on a generated directory (no proprietary data).

use std::fs;
use std::path::PathBuf;

use xiii_package::{Limits, Package, RF_HAS_STACK};
use xiii_tool::{coverage, deps};

fn compact(value: i32) -> Vec<u8> {
    let mut magnitude = value.unsigned_abs();
    let mut first = (magnitude & 0x3f) as u8;
    if value < 0 {
        first |= 0x80;
    }
    magnitude >>= 6;
    if magnitude != 0 {
        first |= 0x40;
    }
    let mut out = vec![first];
    while magnitude != 0 {
        let mut byte = (magnitude & 0x7f) as u8;
        magnitude >>= 7;
        if magnitude != 0 {
            byte |= 0x80;
        }
        out.push(byte);
    }
    out
}

struct Export {
    class: i32,
    outer: i32,
    name: i32,
    flags: u32,
    payload: Vec<u8>,
}

/// Builds a version-100 package: names, imports `(class package, class, outer, name)` and
/// exports.
fn package(names: &[&str], imports: &[(i32, i32, i32, i32)], exports: &[Export]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend(0x9e2a_83c1u32.to_le_bytes());
    b.extend(100u16.to_le_bytes());
    b.extend(58u16.to_le_bytes());
    b.extend(1u32.to_le_bytes());
    b.extend([0u8; 24]);
    b.extend([3u8; 16]);
    b.extend(1i32.to_le_bytes());
    b.extend((exports.len() as i32).to_le_bytes());
    b.extend((names.len() as i32).to_le_bytes());
    let names_at = b.len();
    for n in names {
        b.extend(compact(n.len() as i32 + 1));
        b.extend(n.as_bytes());
        b.push(0);
        b.extend(0u32.to_le_bytes());
    }
    let mut offsets = Vec::new();
    for e in exports {
        offsets.push(b.len());
        b.extend(&e.payload);
    }
    let imports_at = b.len();
    for &(cp, cn, outer, name) in imports {
        b.extend(compact(cp));
        b.extend(compact(cn));
        b.extend(outer.to_le_bytes());
        b.extend(compact(name));
    }
    let exports_at = b.len();
    for (e, off) in exports.iter().zip(offsets) {
        b.extend(compact(e.class));
        b.extend(compact(0));
        b.extend(e.outer.to_le_bytes());
        b.extend(compact(e.name));
        b.extend(e.flags.to_le_bytes());
        b.extend(compact(e.payload.len() as i32));
        if !e.payload.is_empty() {
            b.extend(compact(off as i32));
        }
    }
    let header = [
        names.len(),
        names_at,
        exports.len(),
        exports_at,
        imports.len(),
        imports_at,
    ];
    for (i, v) in header.into_iter().enumerate() {
        b[12 + 4 * i..16 + 4 * i].copy_from_slice(&(v as i32).to_le_bytes());
    }
    b
}

fn exp(class: i32, outer: i32, name: i32, payload: Vec<u8>) -> Export {
    Export {
        class,
        outer,
        name,
        flags: 0,
        payload,
    }
}

fn install() -> PathBuf {
    let root = PathBuf::from(env!("CARGO_TARGET_TMPDIR")).join("synthetic-deps");
    let _ = fs::remove_dir_all(&root);
    fs::create_dir_all(root.join("Maps")).unwrap();
    fs::create_dir_all(root.join("System")).unwrap();
    fs::create_dir_all(root.join("StaticMeshes")).unwrap();

    // Engine.u: classes StaticMesh and StaticMeshActor (no Level: an intrinsic class).
    let engine = package(
        &["None", "Core", "Class", "StaticMesh", "StaticMeshActor"],
        &[],
        &[exp(0, 0, 3, vec![]), exp(0, 0, 4, vec![])],
    );
    fs::write(root.join("System/Engine.u"), engine).unwrap();

    // lib.usx (lower-case on disk): Grp (group), Rock and Grp.Pebble static meshes.
    let lib = package(
        &[
            "None",
            "Core",
            "Engine",
            "Package",
            "Class",
            "StaticMesh",
            "Rock",
            "Grp",
            "Pebble",
        ],
        &[(1, 3, 0, 1), (1, 3, 0, 2), (1, 4, -2, 5), (1, 4, -1, 3)],
        &[
            exp(-4, 0, 7, vec![]),
            exp(-3, 0, 6, vec![0]),
            exp(-3, 1, 8, vec![0]),
        ],
    );
    fs::write(root.join("StaticMeshes/lib.usx"), lib).unwrap();

    // Map: one StaticMeshActor referencing Lib.Rock, plus unresolvable imports.
    let names = [
        "None",            // 0
        "Core",            // 1
        "Engine",          // 2
        "Package",         // 3
        "Class",           // 4
        "StaticMesh",      // 5
        "StaticMeshActor", // 6
        "Lib",             // 7
        "Rock",            // 8
        "Grp",             // 9
        "Pebble",          // 10
        "Level",           // 11
        "Gone",            // 12
        "Missing",         // 13
        "Actor0",          // 14
        "Location",        // 15
        "Vector",          // 16
    ];
    let imports = [
        (1, 3, 0, 2),   // -1 Engine
        (1, 4, -1, 5),  // -2 Engine.StaticMesh
        (1, 4, -1, 6),  // -3 Engine.StaticMeshActor
        (1, 3, 0, 7),   // -4 Lib
        (2, 5, -4, 8),  // -5 Lib.Rock
        (1, 3, -4, 9),  // -6 Lib.Grp
        (2, 5, -6, 10), // -7 Lib.Grp.Pebble
        (1, 4, -1, 11), // -8 Engine.Level (absent class)
        (2, 5, -4, 12), // -9 Lib.Gone (missing object)
        (1, 3, 0, 13),  // -10 Missing (missing package)
    ];
    let mut actor = compact(-3);
    actor.extend(compact(-3));
    actor.extend(u64::MAX.to_le_bytes());
    actor.extend(0u32.to_le_bytes());
    actor.extend(compact(-1));
    actor.extend(compact(5));
    actor.push(0x05);
    actor.extend(compact(-5));
    actor.extend(compact(15));
    actor.push(0x3a);
    actor.extend(compact(16));
    for v in [1.0f32, 2.0, 3.0] {
        actor.extend(v.to_le_bytes());
    }
    actor.extend(compact(0));
    let map = package(
        &names,
        &imports,
        &[Export {
            class: -3,
            outer: 0,
            name: 14,
            flags: RF_HAS_STACK,
            payload: actor,
        }],
    );
    fs::write(root.join("Maps/Test.unr"), map).unwrap();
    root
}

#[test]
fn deps_resolves_objects_and_reports_failures() {
    let root = install();
    let map_path = root.join("Maps/Test.unr");
    let data = fs::read(&map_path).unwrap();
    let map = Package::parse(&data, &Limits::default()).unwrap();
    let index = deps::index_packages(&root).unwrap();
    let r = deps::analyze("Test.unr", &map, &data, &index);

    let names: Vec<&str> = r.packages.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["Engine", "Lib", "Missing"]);
    let engine = &r.packages[0];
    assert_eq!((engine.imports, engine.resolved), (3, 2));
    assert_eq!(engine.absent_classes, ["Engine.Level"]);
    assert_eq!(engine.files, ["System/Engine.u"]);
    let lib = &r.packages[1];
    assert_eq!(lib.files, ["StaticMeshes/lib.usx"]);
    assert_eq!((lib.imports, lib.resolved, lib.property_refs), (4, 3, 1));
    assert_eq!(lib.failures.len(), 1);
    assert!(lib.failures[0].contains("Lib.Gone"), "{:?}", lib.failures);
    assert_eq!(r.missing_packages(), ["Missing"]);
    assert!(!r.is_clean());
    assert!(r.property_failures.is_empty());
    assert_eq!(r.static_mesh_actors, [1, 1, 1, 0, 0]);
    let s = &r.samples[0];
    assert_eq!(s.static_mesh.as_deref(), Some("StaticMesh'Lib.Rock'"));
    assert_eq!(s.location, Some([1.0, 2.0, 3.0]));
    let text = deps::report_text(&r);
    assert!(text.contains("UNRESOLVED"), "{text}");
    assert!(text.contains("result: FAIL"), "{text}");

    let cov = coverage::scan(&root).unwrap();
    assert_eq!(cov.packages, 3);
    let t = coverage::totals(&cov);
    assert_eq!((t.attempted, t.ok, t.failed), (3, 3, 0));
    let sma = &cov.classes["Engine.StaticMeshActor"];
    assert_eq!((sma.has_stack, sma.tail_zero, sma.properties), (1, 1, 2));
    assert_eq!(cov.frame_lengths.get(&15), Some(&1));
    let v = coverage::to_json(&cov, "synthetic");
    assert_eq!(v["totals"]["attempted"], 3);
}
