use super::*;
use crate::test_support::{TempTree, gog_tree, steam_tree};

fn open(t: &TempTree) -> Installation {
    Installation::open(t.root(), &OpenOptions::default()).expect("open synthetic install")
}

fn has_diag(inst: &Installation, code: DiagnosticCode) -> Option<&Diagnostic> {
    inst.diagnostics().iter().find(|d| d.code == code)
}

#[test]
fn gog_layout_is_detected_and_indexed() {
    let t = gog_tree();
    let inst = open(&t);
    assert_eq!(
        inst.profile(),
        ContentProfile::Gog,
        "{:?}",
        inst.detection()
    );
    let d = inst.detection();
    assert!(d.has(Signal::PlatformCodeDir) && d.has(Signal::PlatformTextureDir));
    assert!(!d.has(Signal::PlusPackages) && !d.has(Signal::PatchCredits));

    let persos = inst.resolve_package("XIIIPersos").unwrap();
    assert_eq!(persos.entry.relative, "system/PC/xiiipersos.u");
    assert_eq!(persos.entry.kind, PackageKind::Code);
    assert_eq!(persos.entry.search_root, "system/pc");
    assert_eq!(
        inst.resolve_package("Core").unwrap().entry.relative,
        "system/core.u"
    );
    assert_eq!(
        inst.resolve_package("XIIIBar").unwrap().entry.relative,
        "TexturesPC/XIIIBar.utx"
    );
    assert_eq!(
        inst.resolve_map("Plage00").unwrap().entry.relative,
        "Maps/Plage00.unr"
    );
    assert_eq!(inst.conflicts().count(), 0);
    assert_eq!(inst.logical_name_count(), 11);

    // No Textures, Maps/BaseSP or Maps/BaseMP root exists for GOG.
    let logical: Vec<_> = inst.search_roots().iter().map(|r| r.logical).collect();
    assert_eq!(
        logical,
        [
            "system",
            "system/pc",
            "maps",
            "texturespc",
            "staticmeshes",
            "sounds"
        ]
    );
}

#[test]
fn steam_patched_layout_is_detected_and_indexed() {
    let t = steam_tree();
    let inst = open(&t);
    assert_eq!(
        inst.profile(),
        ContentProfile::SteamPatched,
        "{:?}",
        inst.detection()
    );
    let d = inst.detection();
    assert!(d.has(Signal::FlatTextureDir) && d.has(Signal::SplitMapDirs));
    assert!(d.has(Signal::PlusPackages) && d.has(Signal::PatchCredits));
    assert!(!d.has(Signal::PlatformCodeDir));

    assert_eq!(
        inst.resolve_map("Plage01").unwrap().entry.relative,
        "Maps/BaseSP/Plage01.unr"
    );
    assert_eq!(
        inst.resolve_map("ctf_base").unwrap().entry.relative,
        "Maps/BaseMP/CTF_Base.unr"
    );
    assert_eq!(
        inst.resolve_package("XIIIPersos").unwrap().entry.relative,
        "System/XIIIPersos.u"
    );
    assert_eq!(
        inst.resolve_package("XIIIBar").unwrap().entry.search_root,
        "textures"
    );
    assert!(inst.resolve_package("XIIIPlus").is_ok());
}

#[test]
fn lookup_is_case_insensitive_and_keeps_exact_spelling() {
    let t = gog_tree();
    let inst = open(&t);
    for name in ["plage00", "PLAGE00", "Plage00.unr", "pLaGe00.UNR"] {
        let r = inst.resolve_package(name).unwrap();
        assert_eq!(r.entry.name, "Plage00");
        assert_eq!(r.entry.relative, "Maps/Plage00.unr");
        assert_eq!(r.requested, name);
        assert!(r.path().is_file());
    }
    let r = inst.resolve_package("XiiiPersos.U").unwrap();
    assert_eq!(r.entry.name, "xiiipersos");
}

#[test]
fn duplicate_across_system_and_platform_dir_is_reported_and_refused() {
    let t = gog_tree();
    t.package("system/XIIIPersos.u");
    let inst = open(&t);
    let d = has_diag(&inst, DiagnosticCode::DuplicateLogicalPackage).expect("duplicate diag");
    assert_eq!(d.severity, Severity::Warning);
    assert_eq!(d.paths, ["system/PC/xiiipersos.u", "system/XIIIPersos.u"]);
    match inst.resolve_package("xiiipersos") {
        Err(ResolveError::Ambiguous { candidates, .. }) => assert_eq!(candidates.len(), 2),
        other => panic!("expected ambiguity, got {other:?}"),
    }
    // A kind filter does not pick a winner either.
    assert!(matches!(
        inst.resolve_package_of_kind("XIIIPersos", PackageKind::Code),
        Err(ResolveError::Ambiguous { .. })
    ));
    // Unrelated names keep resolving.
    assert!(inst.resolve_package("gui").is_ok());
    assert_eq!(inst.conflicts().count(), 1);
}

#[test]
fn same_stem_with_different_extensions_is_a_conflict() {
    let t = gog_tree();
    t.package("Sounds/XIII.uax");
    let inst = open(&t);
    assert!(has_diag(&inst, DiagnosticCode::DuplicateLogicalPackage).is_some());
    assert!(matches!(
        inst.resolve_package("XIII.u"),
        Err(ResolveError::Ambiguous { .. })
    ));
}

#[test]
fn missing_wrong_kind_and_invalid_names() {
    let t = gog_tree();
    let inst = open(&t);
    assert_eq!(
        inst.resolve_package("Plage99"),
        Err(ResolveError::NotFound {
            name: "Plage99".into(),
            kind: None
        })
    );
    assert!(matches!(
        inst.resolve_map("Engine"),
        Err(ResolveError::WrongKind {
            expected: PackageKind::Map,
            ..
        })
    ));
    assert!(matches!(
        inst.resolve_map("Plage00.utx"),
        Err(ResolveError::InvalidName { .. })
    ));
    for bad in [
        "",
        "  ",
        "Maps/Plage00",
        "..\\x",
        "Engine.Texture",
        "a.b.unr",
        "C:x",
    ] {
        assert!(
            matches!(
                inst.resolve_package(bad),
                Err(ResolveError::InvalidName { .. })
            ),
            "{bad:?}"
        );
    }
}

#[test]
fn rejects_non_installations() {
    let empty = TempTree::new("empty");
    assert!(matches!(
        Installation::open(empty.root(), &OpenOptions::default()),
        Err(OpenError::NotAnInstallation { .. })
    ));

    // A folder named like the game with text files instead of packages.
    let fake = TempTree::new("fake");
    fake.text("XIII/system/Core.u", "hello");
    fake.text("XIII/system/Engine.u", "hello");
    fake.package("XIII/Maps/Plage00.unr");
    match Installation::open(fake.path("XIII"), &OpenOptions::default()) {
        Err(OpenError::NotAnInstallation { reasons, .. }) => {
            assert!(
                reasons.iter().any(|r| r.contains("not an Unreal package")),
                "{reasons:?}"
            )
        }
        other => panic!("{other:?}"),
    }

    // Valid code packages but no maps anywhere.
    let no_maps = TempTree::new("nomaps");
    no_maps.package("System/Core.u");
    no_maps.package("System/Engine.u");
    assert!(matches!(
        Installation::open(no_maps.root(), &OpenOptions::default()),
        Err(OpenError::NotAnInstallation { .. })
    ));

    let file = TempTree::new("file");
    file.text("plain.txt", "x");
    assert!(matches!(
        Installation::open(file.path("plain.txt"), &OpenOptions::default()),
        Err(OpenError::NotADirectory(_))
    ));
    assert!(matches!(
        Installation::open(file.path("does-not-exist"), &OpenOptions::default()),
        Err(OpenError::Io { .. })
    ));
}

#[test]
fn bad_magic_is_excluded_and_reported() {
    let t = gog_tree();
    t.text("TexturesPC/Broken.utx", "not a package");
    let inst = open(&t);
    let d = has_diag(&inst, DiagnosticCode::BadPackageMagic).expect("bad magic diag");
    assert_eq!(d.paths, ["TexturesPC/Broken.utx"]);
    assert!(matches!(
        inst.resolve_package("Broken"),
        Err(ResolveError::NotFound { .. })
    ));
}

#[test]
fn saves_logs_and_user_content_are_not_indexed() {
    let t = gog_tree();
    t.package("MapsUser/Custom.unr");
    t.package("Save/Plage00.unr");
    let inst = open(&t);
    let inv = inst.inventory();
    assert_eq!(inv.excluded_directories, ["Save"]);
    assert!(inv.files.iter().all(|f| !f.relative.ends_with(".log")));
    assert!(inv.files.iter().all(|f| !f.relative.starts_with("Save/")));
    assert!(inst.resolve_package("Custom").is_err());
    let d = has_diag(&inst, DiagnosticCode::UnindexedPackageFile).expect("unindexed diag");
    assert_eq!(d.paths, ["MapsUser/Custom.unr"]);
    let d = has_diag(&inst, DiagnosticCode::IniPathNotIndexed).expect("ini path diag");
    assert!(d.paths.iter().any(|p| p.contains("MapsUser")));
    // The Save copy does not create a duplicate.
    assert!(inst.resolve_map("Plage00").is_ok());
}

#[test]
fn flattened_without_patch_markers_is_unknown_with_union_roots() {
    let t = steam_tree();
    std::fs::remove_file(t.path("System/XIIIPlus.u")).unwrap();
    std::fs::remove_file(t.path("Maps/Patch 1.4 Credits.txt")).unwrap();
    let inst = open(&t);
    assert_eq!(inst.profile(), ContentProfile::Unknown);
    assert!(
        inst.detection().unknown_reasons[0].contains("without unofficial-patch markers"),
        "{:?}",
        inst.detection()
    );
    assert!(inst.resolve_map("Plage00").is_ok());
}

#[test]
fn mixed_layout_is_unknown_and_collisions_surface() {
    let t = gog_tree();
    t.package("Textures/XIIIBar.utx");
    let inst = open(&t);
    assert_eq!(inst.profile(), ContentProfile::Unknown);
    assert!(inst.detection().unknown_reasons[0].starts_with("mixed layout"));
    match inst.resolve_package("xiiibar") {
        Err(ResolveError::Ambiguous { candidates, .. }) => {
            assert_eq!(
                candidates,
                ["Textures/XIIIBar.utx", "TexturesPC/XIIIBar.utx"]
            )
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn two_installations_never_mix() {
    let g = gog_tree();
    let s = steam_tree();
    let gi = open(&g);
    let si = open(&s);
    for name in ["Plage00", "XIIIPersos", "Core", "XIIIBar", "StaticPlage2"] {
        let a = gi.resolve_package(name).unwrap();
        let b = si.resolve_package(name).unwrap();
        assert!(a.path().starts_with(g.root()) && a.root == gi.root());
        assert!(b.path().starts_with(s.root()) && b.root == si.root());
    }
    // Steam-only packages are not visible through the GOG installation.
    assert!(gi.resolve_package("XIIIPlus").is_err());
}

#[test]
fn ini_paths_are_compared_with_indexed_roots() {
    let t = gog_tree();
    let inst = open(&t);
    let ev = &inst.ini_evidence()[0];
    assert_eq!(ev.file, "system/Default.ini");
    assert_eq!(ev.fields.plateform.as_deref(), Some("0"));
    // The commented-out StaticMeshes path is not reported as active.
    assert!(ev.fields.paths.iter().all(|p| !p.contains("StaticMeshes")));
    assert_eq!(
        ev.search_roots_not_in_paths,
        ["staticmeshes", "system/pc", "texturespc"]
    );
    let checks: Vec<_> = inst
        .specific_packages()
        .iter()
        .map(|c| (c.name.as_str(), c.found.clone()))
        .collect();
    assert_eq!(
        checks,
        [
            ("GUI.u", vec!["system/PC/gui.u".to_owned()]),
            ("XIIIPersos.u", vec!["system/PC/xiiipersos.u".to_owned()])
        ]
    );

    let s = steam_tree();
    let si = open(&s);
    let ev = &si.ini_evidence()[0];
    assert!(ev.search_roots_not_in_paths.is_empty(), "{ev:?}");
    assert!(
        ev.path_checks
            .iter()
            .any(|c| c.resolved_dir.as_deref() == Some("maps/basemp") && c.is_search_root)
    );
}

#[test]
fn walk_limits_are_reported() {
    let t = gog_tree();
    t.text("Maps/a/b/c/d/deep.txt", "x");
    let shallow = OpenOptions {
        max_depth: 3,
        ..OpenOptions::default()
    };
    let inst = Installation::open(t.root(), &shallow).unwrap();
    assert!(has_diag(&inst, DiagnosticCode::DepthLimited).is_some());

    let tiny = OpenOptions {
        max_entries: 3,
        ..OpenOptions::default()
    };
    // Entry truncation can hide maps; either outcome must be explicit.
    match Installation::open(t.root(), &tiny) {
        Ok(inst) => assert!(has_diag(&inst, DiagnosticCode::WalkTruncated).is_some()),
        Err(OpenError::NotAnInstallation { .. }) => {}
        Err(e) => panic!("{e}"),
    }
}

#[cfg(unix)]
#[test]
fn symlinks_are_not_followed() {
    let outside = gog_tree();
    let t = gog_tree();
    std::os::unix::fs::symlink(outside.path("Maps"), t.path("Maps/linked")).unwrap();
    let inst = open(&t);
    assert_eq!(inst.inventory().skipped_links, ["Maps/linked"]);
    assert!(has_diag(&inst, DiagnosticCode::LinkNotFollowed).is_some());
}
