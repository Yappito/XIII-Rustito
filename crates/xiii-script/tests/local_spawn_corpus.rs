//! Opt-in corpus test: run the Plage00 dispatcher chain with the level-start lifecycle and the
//! soldiers active, and assert the run gets past `Actor.Spawn (#278)`.
//!
//! Skipped (with a printed "SKIPPED" line) unless `XIII_GOG_DIR` is set:
//!
//! ```text
//! XIII_GOG_DIR=P:/AI/XIII/XIII_Game cargo test -p xiii-script --test local_spawn_corpus -- --nocapture
//! ```
//!
//! Only the user's owned installation is read; nothing is written. The test prints no script
//! bytes or strings from the packages.

use std::path::{Path, PathBuf};

use xiii_package::{Limits, ObjectRef};
use xiii_script::{
    GlobalRef, ObjRef, ObjectId, ScriptLimits, ScriptPackage, ScriptSet, TraceKind, Value, Vm,
    VmLimits,
};

fn find_by_ext(root: &Path, ext: &str) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.extension().is_some_and(|x| x.eq_ignore_ascii_case(ext)) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

fn find_class(set: &ScriptSet, package: &str, path: &str) -> Option<GlobalRef> {
    let pi = set.package_index(package)?;
    let export = set.packages[pi].export_by_path(path)?;
    Some(GlobalRef {
        package: pi,
        export,
    })
}

#[test]
fn gog_plage00_begin_play_gets_past_actor_spawn() {
    let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
        return;
    };
    let root = PathBuf::from(root);
    // Cargo runs tests with the crate directory as cwd, so a relative `XIII_GOG_DIR`
    // (`XIII_Game`) is resolved against the workspace root instead.
    let root = if root.is_relative() {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(&root)
    } else {
        root
    };
    let mut set = ScriptSet::new();
    for path in find_by_ext(&root, "u") {
        let data = std::fs::read(&path).expect("read package");
        let name = path
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        let pkg = ScriptPackage::load(&name, data, &ScriptLimits::default(), &Limits::default())
            .expect("parse package");
        set.add(pkg);
    }
    let map_path = find_by_ext(&root, "unr")
        .into_iter()
        .find(|p| {
            p.file_stem()
                .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case("Plage00"))
        })
        .expect("Plage00 map");
    let map_data = std::fs::read(&map_path).expect("read map");
    let map_pkg = ScriptPackage::load(
        "Plage00",
        map_data,
        &ScriptLimits::default(),
        &Limits::default(),
    )
    .expect("parse map");
    let map = set.add(map_pkg);

    let mut vm = Vm::new(&set, VmLimits::default());
    let actors = vm.load_level(map, &Limits::default()).expect("load level");
    assert_eq!(actors.len(), 371);

    // Execute the touched trigger, the dispatcher and the two soldiers.
    let mut active: Vec<ObjectId> = Vec::new();
    for &id in &actors {
        let o = &vm.objects[id as usize];
        let class_name = set.packages[o.class.package]
            .ref_name(ObjectRef::Export(o.class.export))
            .to_owned();
        let name = o.name.clone();
        if [
            "TouchTrigger2",
            "XIIIDispatcher0",
            "BaseSoldier14",
            "BaseSoldier15",
        ]
        .iter()
        .any(|a| a.eq_ignore_ascii_case(&class_name) || a.eq_ignore_ascii_case(&name))
            && !active.contains(&id)
        {
            active.push(id);
        }
    }
    for &id in &active {
        vm.set_active(id, true);
    }
    assert!(
        active.len() >= 4,
        "expected the dispatcher and both soldiers"
    );

    let default_game = std::fs::read_to_string(root.join("system/Default.ini"))
        .ok()
        .and_then(|t| {
            t.lines().find_map(|l| {
                let l = l.trim();
                l.strip_prefix("DefaultGame=").map(str::to_owned)
            })
        })
        .expect("DefaultGame in Default.ini");
    let game_class = default_game
        .split_once('.')
        .and_then(|(pkg, class)| find_class(&set, pkg, class))
        .expect("GameInfo class");

    vm.begin_play_with_game_info(&active, game_class)
        .expect("begin play");

    let touched = vm.find_object("TouchTrigger2").expect("TouchTrigger2");
    let player = vm
        .spawn(
            find_class(&set, "xiii", "XIIIPlayerPawn").unwrap(),
            "synthetic",
        )
        .unwrap();
    let arg = Value::Object(Some(ObjRef::Instance(player)));
    vm.send_event(touched, "Touch", vec![arg]).expect("touch");
    for _ in 0..60 {
        vm.tick(1.0 / 30.0).expect("tick");
    }

    // The chain is no longer blocked at Actor.Spawn: a soldier's ShadowProjector came through
    // it, and the GameInfo was spawned.
    let spawned_classes: Vec<String> = vm
        .trace
        .iter()
        .filter_map(|e| match &e.kind {
            TraceKind::Spawned { class, .. } => Some(class.clone()),
            _ => None,
        })
        .collect();
    assert!(
        spawned_classes.iter().any(|c| c.contains("XIIIGameInfo")),
        "{spawned_classes:?}"
    );
    assert!(
        spawned_classes
            .iter()
            .any(|c| c.contains("ShadowProjector")),
        "soldier Spawn never reached: {spawned_classes:?}"
    );
    let dispatcher = vm.find_object("XIIIDispatcher0").expect("dispatcher");
    assert_eq!(vm.state_name(dispatcher).as_deref(), Some("Fin"));
    assert!(
        vm.missing_natives.is_empty(),
        "unimplemented: {:?}",
        vm.missing_natives
    );
    assert!(
        vm.trace
            .iter()
            .all(|e| !matches!(&e.kind, TraceKind::SpawnRefused { .. }))
    );
    println!(
        "Plage00 --begin-play: {} trace records, {} spawned classes, dispatcher ended in Fin",
        vm.trace.len(),
        spawned_classes.len()
    );
}
