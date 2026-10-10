//! Long, explicitly gated campaign regression. Routes are repository-authored inputs;
//! the only map transition is the same native travel bridge used by interactive play.
use super::*;
use std::path::PathBuf;

fn route(map: &str) -> (script::Script, f32, bool) {
    if map == "Plage00" {
        return (script::Script::parse("t=0.0 teleport 5420 -1360.3 880\nt=0.0 yaw 0\nt=0.0 forward 1\nt=2.0 forward 0\nt=2.0 wait_travel\n").expect("existing Plage00 goal input"), 40.0, false);
    }
    let duration = match map {
        "Plage01" => 140.0,
        "Banque01" => 240.0,
        "Amos01" => 150.0,
        "Toits01" => 330.0,
        "Hual01a" => 210.0,
        _ => panic!("no route for {map}"),
    };
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
        "tests/data/{}_route.script",
        map.to_ascii_lowercase()
    ));
    (
        script::Script::load(&path).expect("tracked route"),
        duration,
        true,
    )
}

fn record(label: &str, map: &str, session: &session::Session) {
    let selected = session.player_weapon().map(|id| {
        session
            .vm()
            .set()
            .path(session.vm().objects[id as usize].class)
    });
    println!(
        "[chain-state] {label} map={map} health={:?} selected={selected:?} inventory={:?}",
        session.player_health(),
        session
            .inventory_snapshot()
            .expect("strict travel inventory report")
    );
}

#[test]
fn opt_in_campaign_chain() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR and XIII_CHAIN=1 for campaign chain");
        return;
    };
    if std::env::var("XIII_CHAIN").as_deref() != Ok("1") {
        println!("SKIPPED: set XIII_CHAIN=1 for campaign chain");
        return;
    }
    let root = PathBuf::from(root);
    let params = resolve_params(&root).expect("player parameters").params;
    let maps = [
        "Plage00", "Plage01", "Banque01", "Amos01", "Toits01", "Hual01a", "Hual01b",
    ];
    let mut carried = None;
    let mut reached = Vec::new();
    for (index, map) in maps.iter().copied().enumerate() {
        let scene = viewer::load_scene(&Options {
            game_dir: Some(root.clone()),
            map: Some(map.into()),
            ..Default::default()
        })
        .expect("chain scene");
        let runtime = match carried.take() {
            Some(session) => build_map_runtime(&root, map, &scene, &params, session),
            None => open_map_runtime(&root, map, &scene, &params),
        }
        .expect("chain map runtime");
        record("carried-in", map, &runtime.session);
        reached.push(map);
        if map == "Hual01b" {
            println!("[chain-stop] Hual01b: no tracked route exists");
            break;
        }
        let (input, duration, cinematic) = route(map);
        let fresh =
            open_map_runtime(&root, map, &scene, &params).expect("fresh comparison session");
        record("fresh-in", map, &fresh.session);
        let fresh = run_script_inner(
            &root,
            map,
            &input,
            &params,
            &scene,
            duration,
            cinematic,
            RunnerOptions {
                initial: Some(fresh),
                stop_on_travel: true,
                ..Default::default()
            },
        )
        .expect("fresh route comparison");
        record("fresh-end", map, &fresh.session);
        println!(
            "[chain-result] fresh map={map} objectives={:?} travel={:?} location={:?} failures={:?}",
            fresh.session.objective_states(),
            fresh.travel,
            fresh.session.player_location(),
            fresh.session.failures
        );
        let mut outcome = run_script_inner(
            &root,
            map,
            &input,
            &params,
            &scene,
            duration,
            cinematic,
            RunnerOptions {
                initial: Some(runtime),
                stop_on_travel: true,
                ..Default::default()
            },
        )
        .expect("carried route");
        record("carried-end", map, &outcome.session);
        println!(
            "[chain-result] carried map={map} objectives={:?} travel={:?} location={:?} failures={:?}",
            outcome.session.objective_states(),
            outcome.travel,
            outcome.session.player_location(),
            outcome.session.failures
        );
        assert_eq!(
            outcome.session.objective_states(),
            fresh.session.objective_states(),
            "carried state changed {map} route objectives"
        );
        assert_eq!(
            outcome.travel.len(),
            fresh.travel.len(),
            "carried state broke {map} travel"
        );
        let Some(hop) = outcome.travel.first() else {
            println!(
                "[chain-stop] {map}: tracked route ends at {:?} without travel, objectives={:?}; reached={reached:?}",
                outcome.session.player_location(),
                outcome.session.objective_states()
            );
            // Current Amos01 route reaches the duct, not the rooftop goal. Never substitute
            // a fresh Toits01 login for the missing authored travel.
            assert_eq!(map, "Amos01", "unexpected early chain stop");
            break;
        };
        assert!(
            hop.to.eq_ignore_ascii_case(maps[index + 1]),
            "unexpected campaign destination {hop:?}"
        );
        let plan = travel::TravelPlan::parse(&hop.url, hop.mode, hop.items).expect("travel plan");
        let start = Instant::now();
        carried = Some(
            travel::open_next_session(&mut outcome.session, &root, &plan)
                .expect("shared interactive travel import"),
        );
        println!(
            "[chain-load] {map}->{} request_vm_time={:.3} wall_seconds={:.3} items={}",
            plan.map,
            hop.vm_time,
            start.elapsed().as_secs_f64(),
            hop.items
        );
    }
    assert_eq!(&reached[..3], &["Plage00", "Plage01", "Banque01"]);
}

#[test]
fn opt_in_checkpoint_imports_health_before_accept_inventory_floor() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR for checkpoint health import regression");
        return;
    };
    for (health, expected) in [(17.0, 38.0), (41.0, 41.0), (170.0, 170.0)] {
        let mut session = session::Session::open_checkpoint(Path::new(&root), "Plage00")
            .expect("checkpoint login");
        assert_eq!(session.player_health(), Some(150.0));
        let pawn = session.player;
        let save = crate::save::SaveFile {
            map: "Plage00".into(),
            teleporter: "PlayerStart".into(),
            save_trigger_tag: "Debut".into(),
            description: "travel health regression".into(),
            health,
            speed_factor_limit: 1.0,
            checkpoint_number: 7,
            location: session.player_location().unwrap(),
            rotation: session.player_rotation().unwrap(),
            objectives: vec![],
            inventory: vec![],
            sound_to_launch: None,
            selected_weapon: None,
            music_vars: vec![],
        };
        session
            .restore_checkpoint(&save)
            .expect("native checkpoint import");
        assert_eq!(
            session.player, pawn,
            "AcceptInventory receives Login's existing pawn"
        );
        assert_eq!(session.player_pawn_actors().len(), 1);
        assert_eq!(
            session.player_health(),
            Some(expected),
            "imported HP with authored 25%+1 floor"
        );
        let gi = session.game_info.unwrap();
        assert_eq!(
            session.vm().get_property(gi, "CheckpointNumber"),
            Some(&xiii_script::Value::Int(7))
        );
        println!("[checkpoint-health] imported={health} accepted={expected} login_pawn={pawn}");
    }
}

#[test]
fn opt_in_travel_health_inventory_and_accept_order() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR for native travel regression");
        return;
    };
    let root = Path::new(&root);
    let mut old = session::Session::open(root, "Plage01").expect("source session");
    old.grant_weapon("XIII.Beretta").expect("carried weapon");
    let weapon = old.player_weapon().expect("selected Beretta");
    let ammo = match old.vm().get_property(weapon, "AmmoType") {
        Some(xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(id)))) => *id,
        other => panic!("carried weapon ammo: {other:?}"),
    };
    assert!(
        old.vm_mut()
            .set_property(ammo, "AmmoAmount", 0, xiii_script::Value::Int(7))
    );
    assert!(
        old.vm_mut()
            .set_property(weapon, "ReloadCount", 0, xiii_script::Value::Byte(4))
    );
    let player = old.player;
    assert!(
        old.vm_mut()
            .set_property(player, "Health", 0, xiii_script::Value::Int(41))
    );
    let class = old.vm().objects[player as usize].class;
    let layout = old.vm_mut().class_layout(class).expect("Pawn reflection");
    assert_eq!(
        layout.slot_by_name("Health").expect("Health").flags,
        0x10021
    );
    assert_eq!(
        layout.slot_by_name("Location").expect("Location").flags
            & xiii_script::reflect::property_flags::TRAVEL,
        0
    );
    assert_eq!(
        layout.slot_by_name("Inventory").expect("Inventory").flags
            & xiii_script::reflect::property_flags::TRAVEL,
        0
    );
    println!(
        "[travel-reflection] Pawn.Health=0x{:x}, Actor.Inventory=0x{:x}, Actor.Location=0x{:x}",
        layout.slot_by_name("Health").unwrap().flags,
        layout.slot_by_name("Inventory").unwrap().flags,
        layout.slot_by_name("Location").unwrap().flags
    );
    for name in ["Health", "Inventory", "Location"] {
        let slot = layout.slot_by_name(name).unwrap();
        let export = &old.vm().set().packages[slot.prop.package].package.exports()
            [slot.prop.export as usize];
        println!(
            "[travel-property] {} flags=0x{:x} export={} payload={}..{}",
            old.vm().set().path(slot.prop),
            slot.flags,
            slot.prop.export,
            export.serial_offset,
            export.serial_offset + export.serial_size
        );
    }
    let mut next = travel::open_next_session(
        &mut old,
        root,
        &travel::TravelPlan::parse("banque01.unr", 0, true).unwrap(),
    )
    .expect("travel import");
    assert_eq!(next.player_health(), Some(41.0));
    let selected = next.player_weapon().expect("selected weapon travels");
    assert!(
        next.vm()
            .set()
            .path(next.vm().objects[selected as usize].class)
            .eq_ignore_ascii_case("XIII.Beretta")
    );
    assert_eq!(
        next.vm().get_property(selected, "ReloadCount"),
        Some(&xiii_script::Value::Byte(4))
    );
    assert!(
        next.inventory_snapshot()
            .unwrap()
            .iter()
            .any(|item| item.class_path.eq_ignore_ascii_case("XIII.c9mmAmmo")
                && item.ammo_amount == Some(7))
    );
    assert_eq!(
        next.player_pawn_actors().len(),
        1,
        "import must reuse login's pawn"
    );
    assert!(
        next.inventory_items()
            .iter()
            .any(|(_, class)| class.eq_ignore_ascii_case("XIII.Beretta"))
    );
    let trace = &next.vm().trace;
    let accept = trace.iter().position(|e| matches!(&e.kind, xiii_script::TraceKind::Event {function, args, ..} if function.ends_with(".AcceptInventory") && args.iter().any(|a| a.contains(&next.player_name)))).expect("AcceptInventory receives login pawn");
    assert!(trace[..accept].iter().any(|e| matches!(&e.kind, xiii_script::TraceKind::Event {function, ..} if function.ends_with(".TravelPreAccept"))));
    assert!(trace[accept+1..].iter().any(|e| matches!(&e.kind, xiii_script::TraceKind::Event {function, ..} if function.ends_with(".TravelPostAccept"))));
    // bItems=false gates pawn and inventory export. A map can also destroy
    // inventory itself via NextMapKeepInventory=false even when the request says true.
    let reset = travel::open_next_session(
        &mut next,
        root,
        &travel::TravelPlan::parse("banque01.unr", 0, false).unwrap(),
    )
    .expect("no-items travel");
    assert_eq!(reset.player_health(), Some(150.0));
    assert!(
        !reset
            .inventory_items()
            .iter()
            .any(|(_, class)| class.eq_ignore_ascii_case("XIII.Beretta"))
    );
    // Authored NextMapKeepInventory=false resets HP and weapons even with bItems=true.
    let mut reset_source = session::Session::open(root, "Plage01").expect("reset source");
    let pawn = reset_source.player;
    let mi = reset_source.map_info().expect("MapInfo");
    assert_eq!(
        reset_source.vm().get_property(mi, "NextMapKeepInventory"),
        Some(&xiii_script::Value::Bool(false))
    );
    let gi = reset_source.game_info.expect("GameInfo");
    // Bind the MapInfo that FirstFrame publishes without advancing the intro.
    assert!(reset_source.vm_mut().set_property(
        gi,
        "MapInfo",
        0,
        xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(mi)))
    ));
    reset_source
        .grant_weapon("XIII.Beretta")
        .expect("reset weapon");
    assert!(
        reset_source
            .vm_mut()
            .set_property(pawn, "Health", 0, xiii_script::Value::Int(41))
    );
    reset_source
        .vm_mut()
        .send_event(
            gi,
            "ProcessServerTravel",
            vec![
                xiii_script::Value::Str("banque01.unr".into()),
                xiii_script::Value::Bool(true),
            ],
        )
        .expect("authored inventory reset");
    let reset = travel::open_next_session(
        &mut reset_source,
        root,
        &travel::TravelPlan::parse("banque01.unr", 0, true).unwrap(),
    )
    .expect("travel after script reset");
    assert_eq!(reset.player_health(), Some(150.0));
    assert!(
        !reset
            .inventory_items()
            .iter()
            .any(|(_, class)| class.eq_ignore_ascii_case("XIII.Beretta"))
    );
    // A corrupt chain must fail rather than hang or silently truncate the export.
    assert!(old.vm_mut().set_property(
        player,
        "Inventory",
        0,
        xiii_script::Value::Object(Some(xiii_script::ObjRef::Instance(player)))
    ));
    assert!(
        travel::open_next_session(
            &mut old,
            root,
            &travel::TravelPlan::parse("banque01.unr", 0, true).unwrap()
        )
        .is_err()
    );
    assert!(old.vm_mut().set_property(
        player,
        "Inventory",
        0,
        xiii_script::Value::Str("corrupt link".into())
    ));
    assert!(
        travel::open_next_session(
            &mut old,
            root,
            &travel::TravelPlan::parse("banque01.unr", 0, true).unwrap()
        )
        .is_err()
    );
}
