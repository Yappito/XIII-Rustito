//! Opt-in fight diagnostics; uses the same session/input loop as campaign routes.
use super::*;
use std::collections::BTreeMap;
use std::path::PathBuf;
use xiii_script::registry::{NativeStatus, Registry};
use xiii_script::{ObjRef, TraceKind, Value};

#[test]
fn opt_in_item51_combat_survey() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR and XIII_SURVEY=1 for combat survey");
        return;
    };
    if std::env::var("XIII_SURVEY").as_deref() != Ok("1") {
        println!("SKIPPED: set XIII_SURVEY=1 (campaign combat survey)");
        return;
    }
    let root = PathBuf::from(root);
    let params = resolve_params(&root).expect("player parameters").params;
    let registry = Registry::builtin();
    let mut rank = BTreeMap::<String, u64>::new();
    let mut firing_failures = Vec::new();
    let (maps, _) = survey::discover_campaign_order(&root).expect("campaign order");
    for map in maps.iter().map(String::as_str) {
        if std::env::var("XIII_COMBAT_MAP")
            .is_ok_and(|selected| !selected.eq_ignore_ascii_case(map))
        {
            continue;
        }
        let scene = viewer::load_scene(&Options {
            game_dir: Some(root.clone()),
            map: Some(map.into()),
            ..Default::default()
        })
        .expect("combat scene");
        // Inspect authored placement only. Gameplay itself is driven by the shared runner.
        let probe = session::Session::open(&root, map).expect("placement inspection");
        let vm = probe.vm();
        let soldier = if map == "Base01" {
            vm.find_live_object("BaseSoldier17")
        } else {
            vm.objects
                .iter()
                .enumerate()
                .find(|(i, o)| {
                    o.is_actor
                        && !o.deleted
                        && o.name.starts_with("BaseSoldier")
                        && vm.is_a(*i as u32, "BaseSoldier")
                })
                .map(|(i, _)| i as u32)
        };
        let mut text = String::new();
        if let Some(soldier) = soldier {
            let name = vm.objects[soldier as usize].name.clone();
            let loc = vm
                .vector_prop(soldier, "Location")
                .expect("soldier location");
            let rot = vm.rotation_prop(soldier).expect("soldier rotation");
            let yaw = rot[1] as f32 * std::f32::consts::TAU / 65536.0;
            let place = [
                loc[0] + yaw.cos() * 100.0,
                loc[1] + yaw.sin() * 100.0,
                loc[2] + 20.0,
            ];
            println!(
                "[combat] map={map} target={name} authored_location={loc:?} rotation={rot:?} diagnostic_place={place:?}"
            );
            text = format!(
                "t=6 take_control\nt=6 teleport {} {} {}\nt=6 yaw {}\nt=6.1 weapon XIII.Beretta\n",
                place[0],
                place[1],
                place[2],
                yaw.to_degrees() + 180.0
            );
            // Fire beside, rather than kill, the observed soldier: no health/invulnerability edits.
            text.push_str("t=6.2 turn 45\n");
            for i in 0..60 {
                text.push_str(&format!("t={} fire\n", 6.5 + i as f32));
            }
        } else {
            println!(
                "[combat-no-target] {map}: no authored BaseSoldier; running empty-input session (no fight available)"
            );
        }
        drop(probe);
        let input = script::Script::parse(&text).expect("combat script");
        let outcome = run_script_inner(
            &root,
            map,
            &input,
            &params,
            &scene,
            66.5,
            true,
            RunnerOptions {
                wait_for_control: soldier.is_some(),
                ..Default::default()
            },
        )
        .expect("combat run");
        let session = &outcome.session;
        survey::print_details(&survey::extract(map, &outcome));
        let vm = session.vm();
        if map.eq_ignore_ascii_case("SSH101a") {
            assert!(
                session
                    .failures
                    .iter()
                    .all(|(_, error)| !error.contains("BudgetExceeded")),
                "SSH101a must reach its authored movement latent, not exhaust the VM budget: {:?}",
                session.failures
            );
            assert!(
                vm.trace.iter().any(|event| matches!(
                    &event.kind,
                    TraceKind::LatentStart { actor, native, .. }
                        if actor == "IAController9" && native == "Controller.MoveToward"
                )),
                "SSH101a IAController9 must yield to MoveToward after a successful path search"
            );
        }
        for (i, object) in vm.objects.iter().enumerate() {
            let id = i as u32;
            if !object.is_actor
                || object.name.starts_with("Default__")
                || !vm.is_a(id, "BaseSoldier")
            {
                continue;
            }
            let controller = match vm.get_property(id, "Controller") {
                Some(Value::Object(Some(ObjRef::Instance(c)))) => Some(*c),
                _ => None,
            };
            println!(
                "[combat-soldier] {map} {} orders={:?} health={:?} state={:?} controller={:?}",
                object.name,
                vm.get_property(id, "ORDER"),
                vm.get_property(id, "Health"),
                vm.state_name(id),
                controller.map(|c| &vm.objects[c as usize].name)
            );
            for event in &vm.trace {
                if let TraceKind::StateChange {
                    actor, from, to, ..
                } = &event.kind
                    && (actor == &object.name
                        || controller.is_some_and(|c| actor == &vm.objects[c as usize].name))
                {
                    println!(
                        "[combat-state] {map} t={:.3} {actor} {from:?}->{to:?}",
                        event.time
                    );
                }
            }
        }
        let mut ai_shots = 0;
        let mut hits = 0;
        let mut damage = 0;
        let mut shots_by_soldier = BTreeMap::<String, u64>::new();
        let mut hits_by_soldier = BTreeMap::<String, u64>::new();
        let mut player_shots = 0;
        for event in &vm.trace {
            match &event.kind {
                TraceKind::Native {
                    path,
                    this,
                    args,
                    result,
                    ..
                } if matches!(
                    path.as_str(),
                    "IAController.DirectionDuTir"
                        | "IAController.LineOfFireObstacle"
                        | "Weapon.GetFireStart"
                        | "Pawn.GetViewRotation"
                        | "Actor.Trace"
                ) =>
                {
                    println!(
                        "[combat-ray-native] {map} t={:.3} {this} {path} {args:?} -> {result}",
                        event.time
                    );
                }
                TraceKind::Note(note) if note.starts_with("combat-ray") => {
                    println!("[combat-ray] {map} t={:.3} {note}", event.time);
                }
                TraceKind::Event {
                    target,
                    function,
                    args,
                } if [
                    "SeePlayer",
                    "SeePawn",
                    "HearNoise",
                    "Fire",
                    "NotifyFiring",
                    "EndGame",
                    "GameEnded",
                    "Trigger",
                    "CauseGoal",
                    "SetGoalComplete",
                    "SetGoalFailed",
                    "PoteDeclencheAlarme",
                    "ChercheAlarme",
                ]
                .iter()
                .any(|f| function.ends_with(&format!(".{f}"))) =>
                {
                    println!(
                        "[combat-chain] {map} t={:.3} {target} {function} {args:?}",
                        event.time
                    );
                }
                TraceKind::Native { path, this, .. } if path == "Weapon.PlayFiringSound" => {
                    let instigator =
                        vm.find_object(this)
                            .and_then(|w| match vm.get_property(w, "Instigator") {
                                Some(Value::Object(Some(ObjRef::Instance(p)))) => Some(*p),
                                _ => None,
                            });
                    if instigator == Some(session.player) {
                        player_shots += 1;
                    }
                    if let Some(p) = instigator
                        && p != session.player
                        && vm.is_a(p, "BaseSoldier")
                    {
                        ai_shots += 1;
                        *shots_by_soldier
                            .entry(vm.objects[p as usize].name.clone())
                            .or_default() += 1;
                    }
                }
                TraceKind::Event {
                    target,
                    function,
                    args,
                } if target == &session.player_name && function.ends_with(".TakeDamage") => {
                    hits += 1;
                    if let Some(instigator) = args.get(1) {
                        *hits_by_soldier.entry(instigator.clone()).or_default() += 1;
                    }
                    damage += args
                        .first()
                        .expect("TakeDamage has damage argument")
                        .parse::<i32>()
                        .expect("TakeDamage integer trace argument");
                    println!("[combat-hit] {map} t={:.3} {function} {args:?}", event.time);
                }
                _ => {}
            }
        }
        println!(
            "[combat-result] map={map} ai_shots={ai_shots} player_shots={player_shots} player_hits={hits} damage_requested={damage} health_final={:?} final_map={} failures={} suspended={}",
            session.player_health(),
            outcome.final_map,
            session.failures.len(),
            session.suspended.len()
        );
        println!(
            "[combat-shooters] {map} shots={shots_by_soldier:?} hits_on_player={hits_by_soldier:?}"
        );
        if soldier.is_some() && player_shots != 60 {
            println!(
                "[combat-acceptance-fail] {map}: expected 60 player shots, measured {player_shots}"
            );
            firing_failures.push(format!("{map}: {player_shots}/60 player shots"));
        }
        // Preserve orphaned/destroyed controllers and weapon reload states too: looking only
        // at each pawn's final Controller pointer loses earlier lifecycle transitions.
        for event in &vm.trace {
            if let TraceKind::StateChange {
                actor, from, to, ..
            } = &event.kind
                && from != to
                && (actor.starts_with("IAController")
                    || vm.find_object(actor).is_some_and(|id| {
                        vm.is_a(id, "BaseSoldier")
                            || vm.is_a(id, "IAController")
                            || vm.is_a(id, "Weapon")
                    }))
            {
                println!(
                    "[combat-all-state] {map} t={:.3} {actor} {from:?}->{to:?}",
                    event.time
                );
            }
        }
        for blocked in &session.blocked {
            println!("[combat-blocked] {map} {blocked}");
        }
        for (_, error) in &session.failures {
            if let Some(path) = survey::native_path(error) {
                println!("[combat-missing] {map} {path}");
            }
        }
        for (path, (_, count)) in &vm.natives_used {
            let key = format!("engine.{path}").to_ascii_lowercase();
            let def = registry
                .defs()
                .find(|d| {
                    d.path.eq_ignore_ascii_case(path)
                        || d.path
                            .split_once('.')
                            .is_some_and(|(_, p)| p.eq_ignore_ascii_case(path))
                })
                .or_else(|| registry.get(&key));
            if let Some(def) = def
                && let NativeStatus::Partial(reason) = def.status
            {
                println!(
                    "[combat-partial] {map} {path} count={count} first={:?} reason={reason}",
                    vm.natives_first_caller.get(path)
                );
                if let Some((fight_count, stack)) = vm.combat_natives.get(path) {
                    *rank.entry(path.clone()).or_default() += fight_count;
                    println!(
                        "[combat-fight-partial] {map} {path} count={fight_count} first={stack:?}"
                    );
                }
            }
        }
        for (actor, error) in &session.failures {
            println!("[combat-error] {map} {actor}: {error}");
        }
        for actor in &session.suspended {
            println!(
                "[combat-suspended] {map} {actor} first_stack={:?}",
                session
                    .failures
                    .iter()
                    .find(|(a, _)| a == actor)
                    .and_then(|(_, e)| e.lines().find(|l| l.trim().starts_with("at ")))
            );
        }
    }
    let mut rank: Vec<_> = rank.into_iter().collect();
    rank.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    for (path, count) in rank {
        println!("[combat-rank] {count} {path}");
    }
    assert!(
        firing_failures.is_empty(),
        "combat firing acceptance failures: {firing_failures:?}"
    );
}

#[test]
fn opt_in_combat_control_gate_rejects_walking_cinematic_subclasses() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR for combat control gate regression");
        return;
    };
    let mut session = session::Session::open(Path::new(&root), "Plage01").expect("gate session");
    let pc = session.controller.expect("player controller");
    session
        .vm_mut()
        .goto_state(pc, "NoControl", None)
        .expect("intro state");
    assert!(
        session.vm().is_in_state(pc, "PlayerWalking"),
        "NoControl inherits PlayerWalking in XIII"
    );
    assert!(
        !player_has_control(&session),
        "a family match must not release the survey during an intro"
    );
    session
        .vm_mut()
        .goto_state(pc, "PlayerWalking", None)
        .expect("released control");
    assert!(player_has_control(&session));
    session
        .vm_mut()
        .goto_state(pc, "CameraView", None)
        .expect("camera cine");
    assert!(!player_has_control(&session));
    let root = Path::new(&root);
    let scene = viewer::load_scene(&Options {
        game_dir: Some(root.into()),
        map: Some("Plage01".into()),
        ..Default::default()
    })
    .expect("control wait scene");
    let params = resolve_params(root)
        .expect("control wait parameters")
        .params;
    let input = script::Script::parse("t=0 weapon XIII.Beretta\nt=1 fire\n").unwrap();
    let outcome = run_script_inner(
        root,
        "Plage01",
        &input,
        &params,
        &scene,
        2.0,
        true,
        RunnerOptions {
            wait_for_control: true,
            ..Default::default()
        },
    )
    .expect("wait then fire");
    let shot = outcome
        .session
        .vm()
        .trace
        .iter()
        .find(|event| {
            matches!(&event.kind,
        TraceKind::Native { path, .. } if path == "Weapon.PlayFiringSound")
        })
        .expect("one controlled shot");
    assert!(
        shot.time > 46.5,
        "must wait for Plage01's actual intro release"
    );
    assert!(
        (outcome.session.vm_time() - shot.time - 1.0).abs() < f64::from(2.0 * DT),
        "the two-second scenario must end one second after its t=1 shot; waiting must not double-extend it"
    );
}

#[test]
fn opt_in_item51_duplicate_diagnostic_grant_preserves_carried_weapon() {
    let Ok(root) = std::env::var("XIII_GOG_DIR") else {
        println!("SKIPPED: set XIII_GOG_DIR for duplicate diagnostic grant regression");
        return;
    };
    for map in ["Base01", "Hual01a"] {
        let mut session = session::Session::open(Path::new(&root), map).expect("grant session");
        session.grant_weapon("XIII.Beretta").expect("first grant");
        let first = session.player_weapon().expect("equipped carried weapon");
        session
            .grant_weapon("XIII.Beretta")
            .expect("duplicate grant merges ammo");
        assert_eq!(session.player_weapon(), Some(first));
        assert!(!session.vm().objects[first as usize].deleted);
    }
}
