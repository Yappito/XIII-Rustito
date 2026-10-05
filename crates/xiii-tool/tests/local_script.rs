//! Opt-in compiled-script checks against the user's installations (read-only).
//!
//! Skipped (with a printed "SKIPPED" line) unless `XIII_GOG_DIR` (and, for the patched copy,
//! `XIII_STEAM_DIR`) is set:
//!
//! ```text
//! XIII_GOG_DIR=P:/AI/XIII/XIII_Game cargo test -p xiii-tool --test local_script -- --nocapture
//! ```
//!
//! Expected numbers are the measured GOG results in `crates/xiii-script/README.md` and
//! `docs/evidence/script-coverage-gog.json`.

use std::path::PathBuf;

use xiii_script::TraceKind;
use xiii_script::natives::{CallStats, native_catalog};
use xiii_tool::script_cmd::{coverage_report, disassemble, load_install, package_coverage};
use xiii_tool::script_run::{RunConfig, run_touch_chain};
use xiii_world::runtime::load_with_map;

fn env_root(var: &str) -> Option<PathBuf> {
    match std::env::var_os(var) {
        Some(r) => Some(PathBuf::from(r)),
        None => {
            println!("SKIPPED: set {var} to the installation root to run this test");
            None
        }
    }
}

#[test]
fn gog_every_reflected_export_and_script_decodes_exactly() {
    let Some(root) = env_root("XIII_GOG_DIR") else {
        return;
    };
    let (set, failures) = load_install(&root).expect("read install");
    assert!(failures.is_empty(), "{failures:?}");
    assert_eq!(set.packages.len(), 19);
    let report = coverage_report(&set, &failures, None, "test");
    let t = &report["totals"];
    assert_eq!(t["failures"], 0, "{}", t["first_failures"]);
    let decoded = &t["decoded"];
    for (kind, n) in [
        ("Function", 6903),
        ("State", 532),
        ("Class", 1444),
        ("Struct", 92),
        ("Property", 22632),
        ("Const", 253),
        ("Enum", 104),
    ] {
        assert_eq!(decoded[kind], n, "{kind}");
        assert_eq!(t["attempted"][kind], n, "{kind}");
    }
    assert_eq!(t["class_defaults_exact"], 1444);
    assert_eq!(t["tokens"], 445_474);
    assert_eq!(t["code_memory_bytes"], 1_324_262);
    assert_eq!(t["code_file_bytes"], 980_779);
    for p in &set.packages {
        assert!(package_coverage(p).failures.is_empty(), "{}", p.name);
        assert_eq!(p.package.summary().licensee, 58, "{}", p.name);
    }
    // Native catalog and the measured index conflicts (licensee source declares them twice).
    let cat = native_catalog(&set);
    assert_eq!(cat.len(), 894);
    assert_eq!(cat.iter().filter(|n| n.native_index != 0).count(), 418);
    let dups: Vec<u16> = set
        .native_table()
        .iter()
        .filter(|(_, v)| v.len() > 1)
        .map(|(i, _)| *i)
        .collect();
    assert_eq!(dups, [203, 472, 473, 474, 475, 476]);
    // Every native index called from bytecode has a registered function.
    let mut all = CallStats::default();
    for i in 0..set.packages.len() {
        all.add_package(&set, i);
    }
    assert!(
        all.native_indices
            .keys()
            .all(|i| !set.native_functions(*i).is_empty())
    );
    // Game-package native histograms.
    let xidmaps = set.package_index("xidmaps").unwrap();
    let mut s = CallStats::default();
    s.add_package(&set, xidmaps);
    assert_eq!(s.native_indices.values().sum::<u64>(), 1422);
    assert_eq!(s.native_indices.len(), 120);
    // The M2c behavior path disassembles with named natives (output not printed: proprietary).
    let xidpawn = set.package_index("xidpawn").unwrap();
    let d = disassemble(&set, xidpawn, "XIIIDispatcher.Dispatch", false).unwrap();
    assert!(d.contains("engine.Actor.Sleep"));
    assert!(d.contains("core.Object.GotoState"));
    assert!(d.contains("labeltable [Begin@0x0000]"));
    let engine = set.package_index("engine").unwrap();
    let d = disassemble(&set, engine, "Actor.TriggerEvent", false).unwrap();
    assert!(d.contains("engine.Actor.DynamicActors"));
    println!(
        "GOG: {} packages, {} tokens, {} natives, all reflected exports exact",
        set.packages.len(),
        t["tokens"],
        cat.len()
    );
}

#[test]
fn steam_patched_scripts_decode_exactly() {
    let Some(root) = env_root("XIII_STEAM_DIR") else {
        return;
    };
    let (set, failures) = load_install(&root).expect("read install");
    assert!(failures.is_empty(), "{failures:?}");
    let report = coverage_report(&set, &failures, None, "test");
    assert_eq!(
        report["totals"]["failures"], 0,
        "{}",
        report["totals"]["first_failures"]
    );
    assert_eq!(set.packages.len(), 33);
    assert_eq!(report["totals"]["class_defaults_exact"], 1623);
}

#[test]
fn gog_plage00_dispatcher_chain_runs_in_the_interpreter() {
    let Some(root) = env_root("XIII_GOG_DIR") else {
        return;
    };
    let (set, map) = load_with_map(&root, "Plage00").expect("load");
    let cfg = RunConfig::default();
    let r = run_touch_chain(&set, map, &cfg).expect("run");
    if let Some(e) = &r.error {
        panic!("{e}");
    }
    assert_eq!(r.actors_loaded, 371);
    assert_eq!(r.load_warnings, 0);
    assert_eq!(r.final_states["XIIIDispatcher0"].as_deref(), Some("Fin"));
    let t =
        |k: &dyn Fn(&TraceKind) -> bool| r.trace.iter().filter(|e| k(&e.kind)).collect::<Vec<_>>();
    // Touch handler ran on the trigger.
    assert_eq!(
        t(&|k| matches!(k, TraceKind::Event { target, function, .. } if target == "TouchTrigger2" && function == "TouchTrigger.Touch")).len(),
        1
    );
    // Actor lookups by tag.
    let iters: Vec<Vec<String>> = r
        .trace
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::Iterator { found, .. } => Some(found.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        iters,
        vec![
            vec!["Cine8".to_owned(), "XIIIDispatcher0".to_owned()],
            vec!["BaseSoldier15".to_owned()],
            vec!["BaseSoldier14".to_owned()],
        ]
    );
    // Out-of-scope targets are reported, not run.
    let deferred: Vec<(String, String)> = r
        .trace
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::Deferred { target, class, .. } => Some((target.clone(), class.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        deferred,
        vec![
            ("Cine8".to_owned(), "xidcine.Cine2".to_owned()),
            ("BaseSoldier15".to_owned(), "xidpawn.BaseSoldier".to_owned()),
            ("BaseSoldier14".to_owned(), "xidpawn.BaseSoldier".to_owned()),
        ]
    );
    // State transitions and latent waits with their ticks.
    let states: Vec<(u64, Option<String>, Option<String>)> = r
        .trace
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::StateChange {
                actor, from, to, ..
            } if actor == "XIIIDispatcher0" => Some((e.tick, from.clone(), to.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        states,
        vec![
            (0, None, Some("Dispatch".to_owned())),
            (3, Some("Dispatch".to_owned()), Some("Fin".to_owned())),
        ]
    );
    let sleeps: Vec<u64> =
        t(&|k| matches!(k, TraceKind::LatentStart { seconds, .. } if *seconds == 0.0))
            .iter()
            .map(|e| e.tick)
            .collect();
    assert_eq!(sleeps, [1, 2]);
    let resumes: Vec<u64> = t(&|k| matches!(k, TraceKind::LatentResume { .. }))
        .iter()
        .map(|e| e.tick)
        .collect();
    assert_eq!(resumes, [2, 3]);
    let natives: Vec<&str> = r.natives.iter().map(|n| n.path.as_str()).collect();
    assert_eq!(
        natives,
        [
            "Actor.DynamicActors",
            "Actor.Sleep",
            "Object.AddAdd_Int",
            "Object.AndAnd_BoolBool",
            "Object.Disable",
            "Object.EqualEqual_NameName",
            "Object.GotoState",
            "Object.IsA",
            "Object.Less_IntInt",
            "Object.NotEqual_NameName",
            "Object.Not_PreBool",
            "Object.OrOr_BoolBool",
        ]
    );
    assert!(r.natives.iter().all(|n| n.status != "missing"));
    // Every registered native corresponds to a native declaration in the game packages, and
    // the declared signature's index matches the decoded one.
    let decls: std::collections::BTreeMap<String, u16> = xiii_script::natives::native_catalog(&set)
        .into_iter()
        .map(|n| {
            (
                format!("{}.{}", n.owner, n.name).to_ascii_lowercase(),
                n.native_index,
            )
        })
        .collect();
    let vm = xiii_script::Vm::new(&set, xiii_script::VmLimits::default());
    for d in vm.registry().defs() {
        let index = decls
            .get(&d.path.to_ascii_lowercase())
            .unwrap_or_else(|| panic!("{} has no native declaration", d.path));
        assert!(
            d.signature.starts_with(&format!("native({index})")),
            "{}: {} vs index {index}",
            d.path,
            d.signature
        );
    }
    println!(
        "Plage00 chain: {} trace records, dispatcher ended in Fin",
        r.trace.len()
    );
}
