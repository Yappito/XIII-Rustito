//! `--reach-test`: validate collision + walking against the map's navigation network.
//!
//! The reach-walk implementation now lives in `xiii_world::reach` (Bevy-free), so the
//! `xiii-tool` campaign sweep can run it without depending on Bevy. This module is a thin
//! caller: it runs [`xiii_world::reach::analyze`] and prints the report exactly as before.

use std::path::Path;

use bevy::app::AppExit;

use xiii_world::reach::{ReachReport, analyze, reach_flags};

/// Prints the full report.
pub fn print_report(r: &ReachReport) {
    let classes: Vec<String> = r
        .class_counts
        .iter()
        .map(|(c, n)| format!("{c}:{n}"))
        .collect();
    println!(
        "[reach-test] player {} CollisionRadius={} CollisionHeight={} (half) Unreal units",
        r.player_class, r.player_radius, r.player_height
    );
    println!("[reach-test] nav points by class: {}", classes.join(", "));
    let flags: Vec<String> = r
        .flags_hist
        .iter()
        .map(|(f, n)| format!("0x{f:x}[{}]:{n}", reach_flags::names(*f).join("|")))
        .collect();
    println!(
        "[reach-test] edge reachFlags histogram: {}",
        flags.join(", ")
    );
    for d in &r.diagnostics {
        println!("[reach-test] note: {d}");
    }
    println!(
        "[reach-test] edges {} total, {} walking; eligible {} (R_WALK and spec R>={} H>={}); passes {} failures {}; unresolved-end {} spawn-failures {}",
        r.edges,
        r.walking_edges,
        r.eligible,
        r.player_radius,
        r.player_height,
        r.passes,
        r.failures.len(),
        r.unresolved_end,
        r.spawn_failures
    );
    if !r.groups.is_empty() {
        println!("[reach-test] failure groups:");
        for (cause, n) in &r.groups {
            println!("[reach-test]   {n:>5}  {cause}");
        }
    }
    for f in &r.failures {
        let flags = reach_flags::names(f.flags).join("|");
        let normal = f
            .contact_normal
            .map(|n| format!("({:.2},{:.2},{:.2})", n[0], n[1], n[2]))
            .unwrap_or_else(|| "-".to_owned());
        let contact_h = f
            .contact_height_above_bottom_uu
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".to_owned());
        let floor = f
            .floor_below_center_uu
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".to_owned());
        let ceiling = f
            .ceiling_above_center_uu
            .map(|v| format!("{v:.1}"))
            .unwrap_or_else(|| "-".to_owned());
        let stop = format!(
            "({:.1},{:.1},{:.1})",
            f.stop_uu[0], f.stop_uu[1], f.stop_uu[2]
        );
        let note = f
            .note
            .as_deref()
            .map(|n| format!(" note={n}"))
            .unwrap_or_default();
        println!(
            "[reach-test] FAIL [{}] {} -> {} flags=0x{:x}({}) spec R/H={}/{} d={} goal={:.1}U; stop={stop} remaining={:.1}U steps={}/{} falling={} blocked={} source={} normal={normal} contact_h={contact_h}U floor={floor}U ceiling={ceiling}U{note}",
            f.cause,
            f.start,
            f.end,
            f.flags,
            flags,
            f.spec_radius,
            f.spec_height,
            f.distance_uu,
            f.goal_uu,
            f.remaining_uu,
            f.steps,
            f.budget,
            f.falling,
            f.blocked,
            f.blocking_source.as_deref().unwrap_or("-"),
        );
    }
    println!(
        "[reach-test] RESULT {}: nav={} specs={} walk={} elig={} pass={} fail={}",
        r.map,
        r.nav_points,
        r.edges,
        r.walking_edges,
        r.eligible,
        r.passes,
        r.failures.len()
    );
}

/// Entry point for `--reach-test`.
pub fn run(map: &str, game_dir: &Path) -> AppExit {
    match analyze(map, game_dir) {
        Ok(report) => {
            // `analyze` already printed the inheritance chain and import header, in the same
            // order and format as before the module moved into `xiii-world`.
            print_report(&report);
            AppExit::Success
        }
        Err(e) => {
            eprintln!("error: {e}");
            AppExit::Error(std::num::NonZeroU8::new(1).expect("nonzero"))
        }
    }
}
