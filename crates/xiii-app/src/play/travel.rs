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
//!   destroys it when false). The host re-runs the next map's login, so the carried-inventory
//!   part is **not** reproduced; see the report.

use xiii_script::TravelRequest;

/// A parsed travel request: the next map stem plus the URL options.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TravelPlan {
    /// Map stem (case-insensitive), without directory or `.unr` extension.
    pub map: String,
    /// The `?Key=Value` options tail of the URL (empty when absent).
    pub options: String,
    /// UE2 `ETravelType` byte from the request.
    pub mode: u8,
    /// `bItems`: keep the inventory (per the request; the script already applied
    /// `NextMapKeepInventory`).
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
