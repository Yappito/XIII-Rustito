//! Level-transition host bridge (item15).
//!
//! The script VM reports a level-travel request ([`xiii_script::TravelRequest`]) when the
//! campaign's own code ends a level: `XIIIGoalTrigger.CauseGoal` -> `MapInfo.SetGoalComplete` ->
//! `MapInfo.DoTravel` -> `XIIIGameInfo.EndGame("GoalComplete")` -> `XIIIPlayerController
//! .GameEndedSuccess` -> `Level.ServerTravel(MapInfo.NextMapLevelWithUnr, true)`. The VM never
//! loads a map; this module turns the requested URL into a next-map plan and the host reloads the
//! session.
//!
//! Evidence (`xiii.u`, `engine.u`):
//! - `MapInfo.NextMapLevelWithUnr` is the next map (measured: Plage00 -> `Plage01.unr`,
//!   Plage01 -> `banque01.unr`).
//! - `XIIIGameInfo.ProcessServerTravel` calls `PlayerController.ClientTravel(URL, 2, bItems)`
//!   for a network client and otherwise sets `Level.NextURL` (standalone); both reach this host.
//! - `MapInfo.NextMapKeepInventory` decides whether inventory is carried (the script itself
//!   destroys it when false). Native import reuses the login pawn and remaps travel references
//!   before AcceptInventory; both interactive and headless drivers use `open_next_session`.

use std::collections::HashMap;
use std::path::Path;
use xiii_script::TravelRequest;
use xiii_script::{ObjRef, ObjectId, Value, Vm};

/// Export the pawn and live bTravel inventory, load the destination, import flagged values,
/// and deliver the retail PreAccept / AcceptInventory / PostAccept ordering. No manual GiveTo:
/// imported Owner/Instigator references and Inventory.TravelPreAccept rebuild the chain.
pub(super) fn open_next_session(
    previous: &mut super::session::Session,
    game_dir: &Path,
    plan: &TravelPlan,
) -> Result<super::session::Session, String> {
    // UGameEngine::Tick (0x1037e1a7) gates the entire pawn+inventory export on bItems.
    let mut ids = if plan.items {
        vec![previous.player]
    } else {
        Vec::new()
    };
    if plan.items {
        let mut cursor = previous.player;
        loop {
            let next = match previous.vm().get_property(cursor, "Inventory") {
                Some(Value::Object(Some(ObjRef::Instance(next)))) => *next,
                Some(Value::Object(None)) => break,
                other => {
                    return Err(format!(
                        "travel Inventory must be a live actor reference or None: {other:?}"
                    ));
                }
            };
            if ids.contains(&next) {
                return Err("travel inventory contains a cycle".into());
            }
            if ids.len() >= 4096 {
                return Err("travel inventory exceeds 4096 actors".into());
            }
            let actor = previous
                .vm()
                .objects
                .get(next as usize)
                .ok_or("travel inventory references an absent actor")?;
            if actor.deleted {
                break;
            }
            ids.push(next);
            cursor = next;
        }
    }
    ids.retain(|id| {
        matches!(
            previous.vm().get_property(*id, "bTravel"),
            Some(Value::Bool(true))
        )
    });
    let mut next = super::session::Session::open_travel(game_dir, &plan.map)?;
    let mut remap = HashMap::from([(previous.player, next.player)]);
    next.begin_travel_accept()?;
    for &id in ids.iter().filter(|id| **id != previous.player) {
        let path = previous
            .vm()
            .set()
            .path(previous.vm().objects[id as usize].class);
        let class = xiii_world::runtime::resolve_class_path(next.vm().set(), &path)
            .ok_or_else(|| format!("travel class is not loaded: {path}"))?;
        let location = next.player_location();
        let player = next.player;
        let spawned = next
            .vm_mut()
            .spawn_actor(player, Some(class), Some(player), None, location, None)
            .map_err(|e| format!("travel spawn {path}: {e}"))?
            .ok_or_else(|| format!("travel spawn {path} returned None"))?;
        remap.insert(id, spawned);
    }
    for &id in &ids {
        let class = previous.vm().objects[id as usize].class;
        let layout = previous
            .vm_mut()
            .class_layout(class)
            .map_err(|e| e.to_string())?;
        for slot in &layout.slots {
            if slot.flags & xiii_script::reflect::property_flags::TRAVEL == 0 {
                continue;
            }
            for index in 0..slot.dim {
                let value = previous.vm().objects[id as usize]
                    .props
                    .get(slot.base + index)
                    .ok_or_else(|| format!("travel property {} is absent", slot.name))?;
                let value = remap_value(value, previous.vm(), next.vm(), &remap)?;
                if !next
                    .vm_mut()
                    .set_property(remap[&id], &slot.name, index, value)
                {
                    return Err(format!(
                        "travel property {}[{index}] is not writable",
                        slot.name
                    ));
                }
            }
        }
    }
    let gi = next.game_info.ok_or("travel destination has no GameInfo")?;
    let actors = ids.iter().map(|id| remap[id]).collect::<Vec<_>>();
    next.finish_travel_accept(gi, &actors)?;
    Ok(next)
}

fn remap_value(
    value: &Value,
    old: &Vm<'_>,
    new: &Vm<'_>,
    ids: &HashMap<ObjectId, ObjectId>,
) -> Result<Value, String> {
    Ok(match value {
        Value::Object(Some(ObjRef::Instance(id))) => {
            Value::Object(ids.get(id).copied().map(ObjRef::Instance))
        }
        Value::Object(Some(reference)) => {
            let path = old
                .obj_path(value)
                .ok_or_else(|| format!("travel reference {reference:?} has no path"))?;
            if let Some((value, _)) = new.external_asset(&path) {
                value
            } else {
                let (package, object) = path
                    .split_once('.')
                    .ok_or("travel static reference has no package")?;
                let package = new
                    .set()
                    .package_index(package)
                    .ok_or_else(|| format!("travel package absent: {path}"))?;
                let export = new.set().packages[package]
                    .export_by_path(object)
                    .ok_or_else(|| format!("travel export absent: {path}"))?;
                Value::Object(Some(ObjRef::Static(xiii_script::GlobalRef {
                    package,
                    export,
                })))
            }
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .map(|v| remap_value(v, old, new, ids))
                .collect::<Result<_, _>>()?,
        ),
        Value::Struct(fields) => Value::Struct(
            fields
                .iter()
                .map(|(name, v)| Ok((name.clone(), remap_value(v, old, new, ids)?)))
                .collect::<Result<_, String>>()?,
        ),
        Value::Delegate(Some(_)) | Value::Unsupported(_) => {
            return Err(format!("unsupported travel value: {value:?}"));
        }
        _ => value.clone(),
    })
}

/// A parsed travel request: the next map stem plus the URL options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TravelPlan {
    /// Map stem (case-insensitive), without directory or `.unr` extension.
    pub map: String,
    /// The `?Key=Value` options tail of the URL (empty when absent).
    pub options: String,
    /// UE2 `ETravelType` byte from the request.
    pub mode: u8,
    /// `bItems`: export the pawn and its inventory (per the request; the script already
    /// applied `NextMapKeepInventory`, which can reset health and destroy inventory).
    pub items: bool,
}

impl TravelPlan {
    /// Parses the URL from a travel request. The map is the first `?`-free component, with any
    /// directory prefix and `.unr` extension removed; the rest is the options tail. An empty map
    /// name is an error (never a silent reload of the same map).
    pub fn from_request(req: &TravelRequest) -> Result<Self, String> {
        Self::parse(&req.url, req.mode, req.items)
    }

    /// Parses a raw URL into a plan.
    pub fn parse(url: &str, mode: u8, items: bool) -> Result<Self, String> {
        let (path, options) = match url.split_once('?') {
            Some((p, o)) => (p, o.to_owned()),
            None => (url, String::new()),
        };
        let file = path.rsplit(['/', '\\']).next().unwrap_or(path);
        let stem = if file.len() > 4 && file[file.len() - 4..].eq_ignore_ascii_case(".unr") {
            &file[..file.len() - 4]
        } else {
            file
        };
        if stem.is_empty() {
            return Err(format!("travel URL {url:?} has no map name"));
        }
        Ok(Self {
            map: stem.to_owned(),
            options,
            mode,
            items,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xiii_script::{TravelRequest, TravelSource};

    #[test]
    fn remaps_nested_actor_references_without_reusing_source_ids() {
        let set = xiii_script::ScriptSet::new();
        let old = Vm::new(&set, xiii_script::VmLimits::default());
        let new = Vm::new(&set, xiii_script::VmLimits::default());
        let ids = HashMap::from([(3, 101)]);
        let value = Value::Struct(vec![(
            "nested".into(),
            Value::Array(vec![
                Value::Object(Some(ObjRef::Instance(3))),
                Value::Object(Some(ObjRef::Instance(7))),
                Value::Int(41),
            ]),
        )]);
        assert_eq!(
            remap_value(&value, &old, &new, &ids).unwrap(),
            Value::Struct(vec![(
                "nested".into(),
                Value::Array(vec![
                    Value::Object(Some(ObjRef::Instance(101))),
                    Value::Object(None),
                    Value::Int(41),
                ])
            )])
        );
        assert!(
            remap_value(
                &Value::Unsupported("undecoded travel property".into()),
                &old,
                &new,
                &ids
            )
            .is_err()
        );
        assert!(
            remap_value(
                &Value::Object(Some(ObjRef::External(999))),
                &old,
                &new,
                &ids
            )
            .is_err()
        );
    }

    fn req(url: &str, mode: u8, items: bool) -> TravelRequest {
        TravelRequest {
            actor: "XIIIPlayerController0".to_owned(),
            url: url.to_owned(),
            mode,
            items,
            source: TravelSource::ServerTravel,
            time: 12.5,
        }
    }

    #[test]
    fn parses_plain_unr_url() {
        let p = TravelPlan::from_request(&req("Plage01.unr", 0, true)).unwrap();
        assert_eq!(p.map, "Plage01");
        assert_eq!(p.options, "");
        assert!(p.items);
        assert_eq!(p.mode, 0);
    }

    #[test]
    fn parses_lowercase_and_query() {
        let p = TravelPlan::parse("banque01.unr?Skin=0?Name=XIII", 2, false).unwrap();
        assert_eq!(p.map, "banque01");
        assert_eq!(p.options, "Skin=0?Name=XIII");
        assert!(!p.items);
        assert_eq!(p.mode, 2);
    }

    #[test]
    fn strips_directory_and_extension_case_insensitively() {
        assert_eq!(
            TravelPlan::parse("Maps/BaseSP/Amos01.UNR", 0, true)
                .unwrap()
                .map,
            "Amos01"
        );
        // A stem without extension is kept as-is.
        assert_eq!(TravelPlan::parse("Base01", 0, true).unwrap().map, "Base01");
    }

    #[test]
    fn rejects_empty_map() {
        assert!(TravelPlan::parse("?Skin=0", 0, true).is_err());
        assert!(TravelPlan::parse("", 0, true).is_err());
    }
}
