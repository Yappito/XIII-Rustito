//! Decoded-navigation provider for the script VM (`xiii_script::navigation::NavigationData`).
//!
//! Wraps [`crate::navigation`]'s decoded `Navigation` (map `NavigationPoint`s and their native
//! `PathList` `ReachSpec` tails) in the VM's dependency-free, Unreal-space navigation trait. The
//! VM performs the actual path search; this adapter only normalizes the graph and records how
//! many decoded edges were usable.
//!
//! This module is new for item3f; it does not modify the decoder in [`crate::navigation`].

use std::collections::BTreeMap;

use xiii_script::navigation::{NavEdgeInfo, NavPointInfo, NavigationData};

use crate::navigation::{Navigation, decode_navigation};
use crate::{ClassDefaults, PackageCache};

/// A `NavigationData` provider built from one map's decoded navigation graph.
pub struct MapNavigationProvider {
    points: Vec<NavPointInfo>,
    edges: Vec<NavEdgeInfo>,
    /// Decoded `NavigationPoint` classes and their counts.
    pub class_counts: BTreeMap<String, usize>,
    /// `reachFlags` value histogram across the **mapped** edges.
    pub flags_hist: BTreeMap<u32, usize>,
    /// Edges decoded by `navigation.rs` that could not be mapped (unresolved end/start).
    pub dropped_edges: usize,
}

impl MapNavigationProvider {
    /// Maps an already-decoded [`Navigation`] into the VM's point/edge form.
    ///
    /// `ReachEdge.start_point` may be `None` when the decoded `Start` is null; the owning node
    /// (`owner`) is then used as the start, matching `navigation.rs`'s observation that the
    /// owner is the edge's source. Edges whose `End` did not resolve to a map navigation point
    /// (imports, unresolved) are counted in [`Self::dropped_edges`] and not exposed.
    pub fn from_navigation(nav: &Navigation) -> Self {
        let points: Vec<NavPointInfo> = nav
            .points
            .iter()
            .map(|p| NavPointInfo {
                actor: p.path.clone(),
                location: p.location,
                collision_radius: p.collision_radius,
                collision_height: p.collision_height,
            })
            .collect();
        let mut edges = Vec::with_capacity(nav.edges.len());
        let mut flags_hist = BTreeMap::new();
        let mut dropped = 0usize;
        for e in &nav.edges {
            let start = e.start_point.unwrap_or(e.owner);
            let Some(end) = e.end_point else {
                dropped += 1;
                continue;
            };
            if start >= nav.points.len() || end >= nav.points.len() {
                dropped += 1;
                continue;
            }
            *flags_hist.entry(e.reach_flags).or_default() += 1;
            edges.push(NavEdgeInfo {
                start: start as u32,
                end: end as u32,
                collision_radius: e.collision_radius,
                collision_height: e.collision_height,
                reach_flags: e.reach_flags,
                distance: e.distance,
            });
        }
        Self {
            points,
            edges,
            class_counts: nav.class_counts.clone(),
            flags_hist,
            dropped_edges: dropped,
        }
    }

    /// Decodes `map` through `cache` and wraps it. A class-defaults load failure is an explicit
    /// error, never a silently empty graph.
    pub fn from_cache(cache: &mut PackageCache, map: &str) -> Result<Self, String> {
        let root = cache.root().to_path_buf();
        let mut defaults =
            ClassDefaults::open(&root).map_err(|e| format!("loading class defaults: {e}"))?;
        let nav = decode_navigation(cache, &mut defaults, map)?;
        Ok(Self::from_navigation(&nav))
    }

    /// Opens an installation and decodes the map's navigation.
    pub fn open(game_dir: &std::path::Path, map: &str) -> Result<Self, String> {
        let mut cache = PackageCache::open(game_dir)?;
        Self::from_cache(&mut cache, map)
    }

    /// Number of navigation points exposed to the VM.
    pub fn point_count(&self) -> usize {
        self.points.len()
    }

    /// Number of usable (mapped) edges exposed to the VM.
    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }
}

impl NavigationData for MapNavigationProvider {
    fn points(&self) -> &[NavPointInfo] {
        &self.points
    }

    fn edges(&self) -> &[NavEdgeInfo] {
        &self.edges
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::navigation::reach_flags;

    fn node(export: usize, path: &str, loc: [f32; 3]) -> crate::navigation::NavPoint {
        crate::navigation::NavPoint {
            export,
            class: "Engine.PathNode".into(),
            path: path.into(),
            location: loc,
            collision_radius: 40.0,
            collision_height: 80.0,
            radius_source: "class_default",
            height_source: "class_default",
        }
    }

    /// A tiny decoded graph: A -500-> B, plus an edge whose End did not resolve.
    fn synthetic() -> Navigation {
        use xiii_package::ObjectRef;
        let mut nav = Navigation::default();
        nav.points.push(node(0, "PathNode0", [0.0, 0.0, 0.0]));
        nav.points.push(node(1, "PathNode1", [500.0, 0.0, 0.0]));
        nav.by_export.insert(0, 0);
        nav.by_export.insert(1, 1);
        nav.class_counts.insert("Engine.PathNode".into(), 2);
        nav.edges.push(crate::navigation::ReachEdge {
            owner: 0,
            start: ObjectRef::Export(0),
            end: ObjectRef::Export(1),
            start_point: Some(0),
            end_point: Some(1),
            collision_radius: 120,
            collision_height: 120,
            reach_flags: reach_flags::WALK,
            distance: 500,
        });
        nav.edges.push(crate::navigation::ReachEdge {
            owner: 1,
            start: ObjectRef::Export(1),
            end: ObjectRef::Import(3),
            start_point: Some(1),
            end_point: None,
            collision_radius: 64,
            collision_height: 64,
            reach_flags: reach_flags::DOOR,
            distance: 10,
        });
        nav
    }

    #[test]
    fn maps_points_edges_and_records_dropped() {
        let nav = synthetic();
        let p = MapNavigationProvider::from_navigation(&nav);
        assert_eq!(p.point_count(), 2);
        assert_eq!(p.edge_count(), 1);
        assert_eq!(p.dropped_edges, 1);
        assert_eq!(p.flags_hist.get(&reach_flags::WALK), Some(&1));
        assert!(
            !p.flags_hist.contains_key(&reach_flags::DOOR),
            "dropped edge not counted"
        );
        assert_eq!(p.points()[1].actor, "PathNode1");
        assert_eq!(p.edges()[0].start, 0);
        assert_eq!(p.edges()[0].end, 1);
        assert_eq!(p.class_counts.get("Engine.PathNode"), Some(&2));
    }

    #[test]
    fn a_null_start_uses_the_owning_node() {
        use xiii_package::ObjectRef;
        let mut nav = synthetic();
        nav.edges[0].start = ObjectRef::Null;
        nav.edges[0].start_point = None;
        let p = MapNavigationProvider::from_navigation(&nav);
        assert_eq!(p.edges()[0].start, 0, "owner index used when Start is null");
    }

    /// Opt-in: decode Plage00's real navigation and check every exposed edge resolves to two
    /// in-range points, with at least one walk edge. Prints `SKIPPED` without `XIII_GOG_DIR`.
    #[test]
    fn gog_plage00_navigation_maps_to_the_vm_form() {
        let Some(root) = std::env::var_os("XIII_GOG_DIR") else {
            println!("SKIPPED: set XIII_GOG_DIR to the GOG installation root to run this test");
            return;
        };
        let path = std::path::PathBuf::from(&root);
        let ws = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let path = if path.is_relative() {
            ws.join(path)
        } else {
            path
        };
        let provider = MapNavigationProvider::open(&path, "Plage00").expect("decode Plage00 nav");
        assert!(provider.point_count() > 0, "Plage00 has navigation points");
        assert!(provider.edge_count() > 0, "Plage00 has reach edges");
        let n = provider.point_count() as u32;
        for e in provider.edges() {
            assert!(e.start < n && e.end < n, "edge {e:?} out of range");
        }
        assert!(
            provider
                .edges()
                .iter()
                .any(|e| e.reach_flags & reach_flags::WALK != 0),
            "at least one walk edge"
        );
        println!(
            "Plage00 navigation: {} points, {} edges ({} dropped), classes {{{{...}}}} flags {:?}",
            provider.point_count(),
            provider.edge_count(),
            provider.dropped_edges,
            provider.flags_hist
        );
    }
}
