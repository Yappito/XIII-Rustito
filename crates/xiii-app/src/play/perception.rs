//! item107: what the player can perceive and read off the HUD, gathered for the closed-loop script
//! commands (`engage`, `wait_until`, `goto_until`, see [`super::script`]).
//!
//! Everything here is read-only: line of sight is a zero-extent trace from the player's eye
//! through the level geometry (the same world half the game's own `LineOfSightTo` and bullet
//! traces use), and the other facts are values a player sees (a pawn's visible death, objective
//! completion, the health bar, the weapon's magazine). Nothing is written to the VM, the
//! simulation or the world.

use xiii_script::{ObjRef, ObjectId, Value, Vm};

use super::script::{ActorFact, ActorState, FactRequest, Facts, Seen, WeaponFact};
use super::session::Session;
use super::sim::{PlayerParams, PlayerSim};

/// The Controller class of the soldiers' brain; scripted/cinematic pawns use other controllers.
const AI_CONTROLLER: &str = "iacontroller";

/// Whether the level geometry leaves a clear line from `from` to `to` (Unreal units). Geometry
/// owned by the actor called `allow` (the target's own collision, e.g. a destructible mover)
/// does not count as blocking.
fn sight_clear(vm: &mut Vm, from: [f32; 3], to: [f32; 3], allow: Option<&str>) -> bool {
    match vm.world_line_blocker(from, to) {
        None => true,
        Some(owner) => match (allow, owner) {
            (Some(allow), Some(owner)) => owner.eq_ignore_ascii_case(allow),
            _ => false,
        },
    }
}

fn distance(a: [f32; 3], b: [f32; 3]) -> f32 {
    ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt()
}

fn instance(vm: &Vm, id: ObjectId, name: &str) -> Option<ObjectId> {
    match vm.get_property(id, name) {
        Some(Value::Object(Some(ObjRef::Instance(p)))) => Some(*p),
        _ => None,
    }
}

fn float(vm: &Vm, id: ObjectId, name: &str) -> Option<f32> {
    match vm.get_property(id, name) {
        Some(Value::Float(v)) => Some(*v),
        Some(Value::Int(v)) => Some(*v as f32),
        Some(Value::Byte(v)) => Some(f32::from(*v)),
        _ => None,
    }
}

/// A pawn the player sees fall: `bIsDead`, or no Health left.
fn pawn_dead(vm: &Vm, id: ObjectId) -> bool {
    matches!(vm.get_property(id, "bIsDead"), Some(Value::Bool(true)))
        || float(vm, id, "Health").is_some_and(|h| h <= 0.0)
}

/// Aim points on a target pawn, best first: the requested fraction of its CollisionHeight above
/// its Location (the same head/spine band `track` aims at), then head, chest and legs, so a
/// partly covered enemy is aimed at where it shows.
fn aim_points(location: [f32; 3], half_height: f32, aim: f32) -> Vec<[f32; 3]> {
    let mut fractions = vec![aim];
    for f in [0.9, 0.6, 0.3] {
        if !fractions.iter().any(|x| (x - f).abs() < 1e-3) {
            fractions.push(f);
        }
    }
    fractions
        .into_iter()
        .map(|f| [location[0], location[1], location[2] + half_height * f])
        .collect()
}

/// First clear aim point, if any.
fn first_visible(
    vm: &mut Vm,
    eye: [f32; 3],
    points: &[[f32; 3]],
    allow: Option<&str>,
) -> Option<[f32; 3]> {
    points
        .iter()
        .copied()
        .find(|p| sight_clear(vm, eye, *p, allow))
}

/// Facts about the actor `name`: dead pawn or destroyed (`Dead`), live (`Alive`), or never
/// spawned (`Missing`), plus its visibility and aim point when alive.
fn actor_fact(vm: &mut Vm, eye: [f32; 3], name: &str, aim: f32) -> ActorFact {
    let mut fact = ActorFact {
        name: name.to_ascii_lowercase(),
        ..ActorFact::default()
    };
    let Some(id) = vm.find_live_object(name) else {
        // A destroyed actor keeps its (deleted) object: it is gone, not unspawned.
        if vm
            .objects
            .iter()
            .any(|o| o.deleted && o.name.eq_ignore_ascii_case(name))
        {
            fact.state = ActorState::Dead;
        }
        return fact;
    };
    let is_pawn = vm.is_a(id, "Pawn");
    let dead = if is_pawn {
        pawn_dead(vm, id)
    } else {
        matches!(vm.get_property(id, "bIsDead"), Some(Value::Bool(true)))
    };
    if dead {
        fact.state = ActorState::Dead;
        // A dead pawn still has its `Location` (the corpse rests there); report it so an
        // `approach` can walk onto the body to carry it (`aim` is the raw Location, not an
        // aimed band — the caller aims at a corpse on the floor).
        if let Some(location) = vm.vector_prop(id, "Location") {
            fact.distance = distance(eye, location);
            fact.aim = Some(location);
        }
        return fact;
    }
    fact.state = ActorState::Alive;
    let Some(location) = vm.vector_prop(id, "Location") else {
        return fact;
    };
    fact.distance = distance(eye, location);
    // Pawns are aimed at a fraction of their CollisionHeight; other actors (map movers have
    // oversized collision cylinders) at the raw Location.
    let points = if is_pawn {
        aim_points(
            location,
            float(vm, id, "CollisionHeight").unwrap_or(0.0),
            aim,
        )
    } else {
        vec![location]
    };
    // A non-pawn target's own collision (a destructible mover) must not hide it.
    if let Some(p) = first_visible(vm, eye, &points, Some(name)) {
        fact.visible = true;
        fact.aim = Some(p);
    }
    fact
}

/// Visible, living, hostile pawns within `radius` of the eye. Hostile: a pawn driven by an
/// `IAController` (the soldiers' brain; scripted cinematic pawns use other controllers) whose
/// authored `InitialAlliances` toward the player's alliance is negative.
fn visible_hostiles(
    vm: &mut Vm,
    player: ObjectId,
    eye: [f32; 3],
    radius: f32,
    aim: f32,
) -> Vec<Seen> {
    let player_alliance = vm.get_property(player, "Alliance").cloned();
    // Candidates first (read-only), then the sight traces (which need the VM mutably).
    let mut candidates = Vec::new();
    for (i, o) in vm.objects.iter().enumerate() {
        if !o.is_actor || o.deleted || o.name.starts_with("Default__") {
            continue;
        }
        let id = i as ObjectId;
        let Some(location) = vm.vector_prop(id, "Location") else {
            continue;
        };
        let dist = distance(eye, location);
        if dist > radius || id == player || !vm.is_a(id, "Pawn") || pawn_dead(vm, id) {
            continue;
        }
        let Some(ctrl) = instance(vm, id, "Controller") else {
            continue;
        };
        if !vm.is_a(ctrl, AI_CONTROLLER) {
            continue;
        }
        let hostile = match (vm.get_property(id, "InitialAlliances"), &player_alliance) {
            (Some(Value::Struct(fields)), Some(Value::Name(player))) => {
                let named = fields.iter().any(|(k, v)| {
                    k == "alliancename"
                        && matches!(v, Value::Name(n) if n.eq_ignore_ascii_case(player))
                });
                let level = fields.iter().find_map(|(k, v)| match (k.as_str(), v) {
                    ("alliancelevel", Value::Float(l)) => Some(*l),
                    _ => None,
                });
                named && level.is_some_and(|l| l < 0.0)
            }
            _ => false,
        };
        if hostile {
            let points = aim_points(
                location,
                float(vm, id, "CollisionHeight").unwrap_or(0.0),
                aim,
            );
            candidates.push((o.name.clone(), dist, points, float(vm, id, "Health")));
        }
    }
    candidates
        .into_iter()
        .filter_map(|(name, dist, points, health)| {
            first_visible(vm, eye, &points, None).map(|point| Seen {
                visible: true,
                name,
                aim: point,
                distance: dist,
                health,
            })
        })
        .collect()
}

/// Whether `pawn` still carries any inventory item (weapon, ammo, key): the game hands a corpse's
/// whole chain over on contact.
fn carries_item(vm: &Vm, pawn: ObjectId) -> bool {
    instance(vm, pawn, "Inventory").is_some()
}

/// Dead pawns within `radius` that still carry something, with their visibility: what a player sees lying there
/// and walks over to pick up from (the game hands a corpse's weapon over on contact).
fn lootable_corpses(vm: &mut Vm, player: ObjectId, eye: [f32; 3], radius: f32) -> Vec<Seen> {
    let mut candidates = Vec::new();
    for (i, o) in vm.objects.iter().enumerate() {
        if !o.is_actor || o.deleted || o.name.starts_with("Default__") {
            continue;
        }
        let id = i as ObjectId;
        let Some(location) = vm.vector_prop(id, "Location") else {
            continue;
        };
        let dist = distance(eye, location);
        if dist > radius
            || id == player
            || !vm.is_a(id, "Pawn")
            || !pawn_dead(vm, id)
            || !carries_item(vm, id)
        {
            continue;
        }
        candidates.push((o.name.clone(), dist, location));
    }
    candidates
        .into_iter()
        .map(|(name, distance, aim)| Seen {
            // The body lies on the floor: look at a point just above it.
            visible: sight_clear(vm, eye, [aim[0], aim[1], aim[2] + 10.0], None),
            name,
            aim,
            distance,
            health: None,
        })
        .collect()
}

/// Rounds loaded in `weapon`: the HUD's ammo count is the weapon's `AmmoType.AmmoAmount` (measured:
/// it falls by one per shot and stops at 0); a dry shot also raises the weapon's `bEmptyShot`.
fn loaded_rounds(vm: &Vm, weapon: ObjectId) -> Option<u32> {
    if matches!(
        vm.get_property(weapon, "bEmptyShot"),
        Some(Value::Bool(true))
    ) {
        return Some(0);
    }
    instance(vm, weapon, "AmmoType")
        .and_then(|ammo| float(vm, ammo, "AmmoAmount"))
        .map(|n| n.max(0.0) as u32)
}

fn is_melee(vm: &Vm, weapon: ObjectId) -> bool {
    matches!(
        vm.get_property(weapon, "bMeleeWeapon"),
        Some(Value::Bool(true))
    )
}

/// The player's weapon as the fire logic needs it: firing mode, loaded rounds and the other
/// carried ranged weapons that still have ammo (what the weapon selector shows).
fn weapon_fact(session: &Session) -> Option<WeaponFact> {
    let weapon = session.player_weapon()?;
    let vm = session.vm();
    let melee = is_melee(vm, weapon);
    let mut alternatives = Vec::new();
    let mut cursor = instance(vm, session.player, "Inventory");
    let mut guard = 0;
    while let Some(id) = cursor {
        guard += 1;
        if guard > 256 {
            break;
        }
        if id != weapon
            && vm.is_a(id, "Weapon")
            && !is_melee(vm, id)
            && loaded_rounds(vm, id).is_some_and(|n| n > 0)
            && let Some(Value::Byte(group)) = vm.get_property(id, "InventoryGroup")
        {
            alternatives.push(*group);
        }
        cursor = instance(vm, id, "Inventory");
    }
    Some(WeaponFact {
        // Fists carry no ammo and are never "empty".
        clip: if melee {
            None
        } else {
            loaded_rounds(vm, weapon)
        },
        melee,
        alternatives,
    })
}

/// Gathers what `req` asks for. `suppressed`: an authored cinematic or the menu owns the input.
pub(super) fn gather(
    req: &FactRequest,
    session: &mut Session,
    sim: &PlayerSim,
    params: &PlayerParams,
    suppressed: bool,
) -> Facts {
    let eye = sim.eye_location(params);
    let mut facts = Facts {
        player_health: session.player_health(),
        suppressed,
        ..Facts::default()
    };
    if req.weapon {
        facts.weapon = weapon_fact(session);
    }
    if req.goals {
        facts.goals = vec![false; 32];
        for goal in session.objective_states() {
            if goal.index >= facts.goals.len() {
                facts.goals.resize(goal.index + 1, false);
            }
            facts.goals[goal.index] = goal.completed;
        }
    }
    let player = session.player;
    let vm = session.vm_mut();
    facts.actors = req
        .actors
        .iter()
        .map(|name| actor_fact(vm, eye, name, req.aim))
        .collect();
    if let Some(radius) = req.corpse_radius {
        facts.corpses = lootable_corpses(vm, player, eye, radius);
    }
    if let Some(radius) = req.hostile_radius {
        facts.hostiles = visible_hostiles(vm, player, eye, radius, req.aim);
    }
    facts
}
