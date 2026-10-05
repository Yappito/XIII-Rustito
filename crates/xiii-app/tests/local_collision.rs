//! Opt-in end-to-end doorway collision test against the user's GOG installation (read-only).
//!
//! Runs the real `xiii-app --collision-test` binary (no window) and asserts the **aligned
//! door case**: the closed world is blocked by a `Porte6` source and the world without
//! `Porte6` passes at least 1 m beyond the door plane. The PlayerStart case is printed but
//! not asserted: at the current (uncalibrated) 50 u/m the 3 m box is stopped by the imported
//! interior floor mesh before it reaches the door, which is an informative finding, not a
//! regression. See `local/reports/item1-collision.md`.
//!
//! Skipped (with a printed `SKIPPED` line) unless `XIII_GOG_DIR` is set:
//!
//! ```text
//! XIII_GOG_DIR=P:/AI/XIII/XIII_Game cargo test -p xiii-app --test local_collision -- --nocapture
//! ```

use std::process::Command;

#[test]
fn plage01_porte6_aligned_door_case_closed_blocks_open_passes() {
    let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
        return;
    };
    // A relative value is resolved against the workspace root, so the acceptance command
    // `XIII_GOG_DIR=XIII_Game cargo test` works from anywhere (test CWD is the crate dir).
    let path = std::path::PathBuf::from(&root);
    let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let path = if path.is_relative() {
        ws.join(path)
    } else {
        path
    };
    let exe = env!("CARGO_BIN_EXE_xiii-app");
    let out = Command::new(exe)
        .args([
            "--map",
            "Plage01",
            "--game-dir",
            &path.to_string_lossy(),
            "--collision-test",
        ])
        .output()
        .expect("run xiii-app --collision-test");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    for line in stdout.lines() {
        println!("{line}");
    }
    if !stderr.is_empty() {
        eprintln!("{stderr}");
    }

    assert!(
        out.status.success(),
        "collision-test exited with {:?}",
        out.status.code()
    );

    // The aligned door case is the acceptance case (labelled distinctly).
    let closed = stdout
        .lines()
        .find(|l| l.contains("] aligned door case closed:"))
        .expect("aligned closed case line");
    let open = stdout
        .lines()
        .find(|l| l.contains("] aligned door case open:"))
        .expect("aligned open case line");
    assert!(
        closed.contains("PASS") && closed.contains("Porte6"),
        "aligned closed case must PASS and be blocked by Porte6: {closed}"
    );
    assert!(open.contains("PASS"), "aligned open case must PASS: {open}");
    assert!(
        stdout.contains("RESULT aligned door case: closed PASS open PASS"),
        "expected the aligned door case to be reported as PASS"
    );
}
