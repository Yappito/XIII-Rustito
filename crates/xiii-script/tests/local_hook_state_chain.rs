//! Opt-in test: the state-function chain the player's grapple depends on.
//!
//! item64's VM dispatch runs the controller state `PlayerMove` while the player is hooked or
//! climbing (`Vm::dispatch_hook_player_moves`). That only reaches `ClimbToHookNavPoint.
//! ProcessMove` if the state resolves `PlayerMove` through its super-state chain (the class-level
//! `Engine.PlayerController.PlayerMove` is an empty 2-byte stub). This test prints the resolved
//! chain from the real packages and asserts it.
//!
//! Skipped (with a printed "SKIPPED" line) unless `XIII_GOG_DIR` is set:
//!
//! ```text
//! XIII_GOG_DIR=P:/AI/XIII/XIII_Game cargo test -p xiii-script --test local_hook_state_chain -- --nocapture
//! ```
//!
//! Only the user's owned installation is read; nothing is written. The test prints object paths
//! and function names only — no script bytes.

use std::path::{Path, PathBuf};

use xiii_package::Limits;
use xiii_package::ObjectRef;
use xiii_script::{GlobalRef, ScriptLimits, ScriptPackage, ScriptSet};

fn load_u(set: &mut ScriptSet, root: &Path, name: &str) {
    let path = find_u(root, name).unwrap_or_else(|| panic!("{name}.u not found"));
    let data = std::fs::read(&path).expect("read package");
    let pkg = ScriptPackage::load(name, data, &ScriptLimits::default(), &Limits::default())
        .expect("parse package");
    set.add(pkg);
}

fn find_u(root: &Path, name: &str) -> Option<PathBuf> {
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p
                .file_stem()
                .is_some_and(|s| s.to_string_lossy().eq_ignore_ascii_case(name))
                && p.extension().is_some_and(|x| x.eq_ignore_ascii_case("u"))
            {
                return Some(p);
            }
        }
    }
    None
}

fn state_chain(set: &ScriptSet, package: &str, path: &str) -> Vec<String> {
    let pi = set.package_index(package).expect("package");
    let export = set.packages[pi]
        .export_by_path(path)
        .unwrap_or_else(|| panic!("{path} not found"));
    let mut names = Vec::new();
    let mut cur = Some(GlobalRef {
        package: pi,
        export,
    });
    while let Some(s) = cur {
        let obj = set.packages[s.package]
            .objects
            .get(&s.export)
            .expect("state object");
        names.push(
            set.packages[s.package]
                .ref_name(ObjectRef::Export(s.export))
                .to_owned(),
        );
        cur = obj
            .struct_header()
            .and_then(|h| set.resolve(s.package, h.field.super_field));
    }
    names
}

/// The first state in `chain` (walking the package objects) that defines `name`.
fn state_defines(set: &ScriptSet, package: &str, path: &str, name: &str) -> Option<String> {
    let pi = set.package_index(package).expect("package");
    let export = set.packages[pi].export_by_path(path)?;
    let mut cur = Some(GlobalRef {
        package: pi,
        export,
    });
    while let Some(s) = cur {
        let obj = set.packages[s.package].objects.get(&s.export)?;
        if let Some(header) = obj.struct_header() {
            let mut child = header.children;
            let p = &set.packages[s.package];
            let mut guard = 0;
            while let ObjectRef::Export(e) = child {
                guard += 1;
                if guard > 65_536 {
                    break;
                }
                let o = p.objects.get(&e)?;
                if matches!(o, xiii_script::ScriptObject::Function(_))
                    && p.ref_name(child).eq_ignore_ascii_case(name)
                {
                    return Some(p.ref_name(ObjectRef::Export(s.export)).to_owned());
                }
                child = o.field().next;
            }
        }
        cur = obj
            .struct_header()
            .and_then(|h| set.resolve(s.package, h.field.super_field));
    }
    None
}

#[test]
fn gog_hook_states_chain_to_player_walking() {
    let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
        return;
    };
    let root = PathBuf::from(root);
    let root = if root.is_relative() {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(&root)
    } else {
        root
    };
    let mut set = ScriptSet::new();
    load_u(&mut set, &root, "Engine");
    load_u(&mut set, &root, "xiii");

    for state in [
        "XIIIPlayerController.PlayerWalking",
        "XIIIPlayerController.ClimbToHookNavPoint",
        "XIIIPlayerController.Tyroling",
    ] {
        let chain = state_chain(&set, "xiii", state);
        println!("{state} chain: {}", chain.join(" -> "));
    }

    // The dispatch contract: ClimbToHookNavPoint must resolve PlayerMove through its state chain
    // (the class-level engine PlayerMove is an empty stub) and define its own ProcessMove.
    assert_eq!(
        state_defines(
            &set,
            "xiii",
            "XIIIPlayerController.ClimbToHookNavPoint",
            "PlayerMove"
        )
        .as_deref(),
        Some("PlayerWalking"),
        "ClimbToHookNavPoint must inherit PlayerMove from the PlayerWalking state chain"
    );
    assert_eq!(
        state_defines(
            &set,
            "xiii",
            "XIIIPlayerController.ClimbToHookNavPoint",
            "ProcessMove"
        )
        .as_deref(),
        Some("ClimbToHookNavPoint"),
        "ClimbToHookNavPoint must define its own ProcessMove (the nav climb)"
    );
    assert_eq!(
        state_defines(
            &set,
            "xiii",
            "XIIIPlayerController.PlayerWalking",
            "ProcessMove"
        )
        .as_deref(),
        Some("PlayerWalking"),
        "PlayerWalking must define ProcessMove (the rope constraint)"
    );
}
