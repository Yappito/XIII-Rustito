//! Navigation decoding: `NavigationPoint` actors and their native `PathList` of `ReachSpec`
//! edges.
//!
//! ## Where the data lives (evidence)
//!
//! `ReachSpec` is **not** a separate export in XIII maps and `PathList` is **not** a tagged
//! property: it is a native tail after the actor's property block. Each `NavigationPoint`
//! subclass export payload ends with a compact count followed by that many fixed 12-byte
//! records (plus variable-length object references):
//!
//! ```text
//! compact  count
//! repeat count:
//!     compact  Start  (object reference, may be 0 = None)
//!     compact  End    (object reference)
//!     u16      CollisionRadius
//!     u16      CollisionHeight   (half height, Unreal units)
//!     u32      reachFlags
//!     u16      Distance          (Unreal units)
//! ```
//!
//! Measured on the GOG corpus: Plage01 (144 nav points, 313 edges), Plage00 (11, 12) and the
//! interior `Banque01` (234, 656) — every tail is consumed exactly, every non-null `End`
//! resolves to a navigation export, the first `Start` equals the owning node in all but three
//! cases (where it is `None`), and the trailing `u16` matches the Euclidean distance between
//! the two node locations (median error below one Unreal unit). The `reachFlags` bit values
//! observed are 1, 64, 128 and 256; 64 appears on the single `Engine.Ladder` edge, which
//! corroborates the upstream UE2 `EReachSpecFlags` reading (see [`reach_flags`]).
//!
//! The reach-flag names and bit meanings are an **upstream hypothesis** from UE2
//! (`EReachSpecFlags` in `UnPath.h`); they are not proven against the XIII engine, only
//! consistent with the decoded values. The field order (Start, End, radius, height, flags,
//! distance) is **measured** from the byte layout (see above).

use std::collections::{BTreeMap, HashMap};

use xiii_decode::common::Props;
use xiii_package::{Cursor, Limits, ObjectRef, Span};

use crate::{ClassDefaults, PackageCache};

/// `EReachSpecFlags` bit meanings (upstream UE2 `UnPath.h`; hypothesis for XIII). See the
/// module documentation for the corroborating evidence.
pub mod reach_flags {
    /// `R_WALK`: walking required.
    pub const WALK: u32 = 1;
    /// `R_FLY`: flying required.
    pub const FLY: u32 = 2;
    /// `R_SWIM`: swimming required.
    pub const SWIM: u32 = 4;
    /// `R_JUMP`: jumping required.
    pub const JUMP: u32 = 8;
    /// `R_DOOR`: passing through a door.
    pub const DOOR: u32 = 16;
    /// `R_SPECIAL`: special movement.
    pub const SPECIAL: u32 = 32;
    /// `R_LADDER`: ladder climbing.
    pub const LADDER: u32 = 64;
    /// `R_PROSCRIBED`: proscribed (do not connect) path.
    pub const PROSCRIBED: u32 = 128;
    /// `R_FORCED`: forced connection.
    pub const FORCED: u32 = 256;
    /// `R_PLAYERONLY`: player-only path.
    pub const PLAYERONLY: u32 = 512;

    /// Human-readable names of the bits set in `flags` (upstream names).
    pub fn names(flags: u32) -> Vec<&'static str> {
        let all = [
            (WALK, "R_WALK"),
            (FLY, "R_FLY"),
            (SWIM, "R_SWIM"),
            (JUMP, "R_JUMP"),
            (DOOR, "R_DOOR"),
            (SPECIAL, "R_SPECIAL"),
            (LADDER, "R_LADDER"),
            (PROSCRIBED, "R_PROSCRIBED"),
            (FORCED, "R_FORCED"),
            (PLAYERONLY, "R_PLAYERONLY"),
        ];
        let mut out: Vec<&'static str> = all
            .iter()
            .filter(|(bit, _)| flags & bit != 0)
            .map(|(_, name)| *name)
            .collect();
        if out.is_empty() {
            out.push("none");
        }
        out
    }
}

/// One decoded `ReachSpec` edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReachEdge {
    /// Index into [`Navigation::points`] of the navigation point whose `PathList` holds it.
    pub owner: usize,
    /// Decoded `Start` reference (may be null).
    pub start: ObjectRef,
    /// Decoded `End` reference.
    pub end: ObjectRef,
    /// `Start` resolved to a navigation point, when non-null and known.
    pub start_point: Option<usize>,
    /// `End` resolved to a navigation point, when an export of this map.
    pub end_point: Option<usize>,
    /// Largest collision radius that can traverse this edge (Unreal units).
    pub collision_radius: u16,
    /// Largest collision (half) height that can traverse this edge (Unreal units).
    pub collision_height: u16,
    /// `reachFlags` bit field.
    pub reach_flags: u32,
    /// Cached edge distance (Unreal units).
    pub distance: u16,
}

impl ReachEdge {
    /// True when the edge requires walking (`R_WALK` bit set).
    pub fn is_walk(&self) -> bool {
        self.reach_flags & reach_flags::WALK != 0
    }
}

/// One navigation point actor placed in the map.
#[derive(Debug, Clone, PartialEq)]
pub struct NavPoint {
    /// Export index in the map.
    pub export: usize,
    /// Class path as written in the map (e.g. `Engine.PathNode`).
    pub class: String,
    /// Object path.
    pub path: String,
    /// Effective `Location` (Unreal units, Z up).
    pub location: [f32; 3],
    /// Effective `CollisionRadius` (Unreal units).
    pub collision_radius: f32,
    /// Effective `CollisionHeight`, a half height (Unreal units).
    pub collision_height: f32,
    /// Where `CollisionRadius` came from: `map`, `class_default` or `engine_default`.
    pub radius_source: &'static str,
    /// Where `CollisionHeight` came from.
    pub height_source: &'static str,
}

/// Decoded navigation network of one map.
#[derive(Debug, Default)]
pub struct Navigation {
    /// Navigation points, in export order.
    pub points: Vec<NavPoint>,
    /// Map export index -> index in [`Navigation::points`].
    pub by_export: HashMap<usize, usize>,
    /// All decoded edges.
    pub edges: Vec<ReachEdge>,
    /// Navigation point classes and their counts.
    pub class_counts: BTreeMap<String, usize>,
    /// `reachFlags` value histogram across all edges.
    pub flags_hist: BTreeMap<u32, usize>,
    /// Navigation exports whose class could not be resolved (class, error).
    pub unresolved_class: Vec<(usize, String)>,
    /// Navigation exports whose property block failed to decode.
    pub property_failures: Vec<(usize, String)>,
    /// Navigation exports whose native `PathList` tail could not be parsed.
    pub tail_failures: Vec<(usize, String)>,
    /// Navigation exports with an empty native tail (no `PathList` bytes).
    pub empty_path_lists: usize,
    /// Edges whose decoded `Start` is null.
    pub null_start_edges: usize,
    /// Edges whose decoded `Start` is a different navigation point than the owner.
    pub start_mismatch_edges: usize,
    /// Edges whose `End` is an import (not an instance export of this map).
    pub import_end_edges: usize,
    /// Edges whose `End` does not resolve to a known navigation point.
    pub unresolved_end_edges: usize,
    /// Total edges decoded.
    pub edge_count: usize,
}

/// Raw edge as stored in a `PathList` tail, before reference resolution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct RawEdge {
    start: ObjectRef,
    end: ObjectRef,
    collision_radius: u16,
    collision_height: u16,
    reach_flags: u32,
    distance: u16,
}

/// Parses one native `PathList` tail. The slice must be consumed exactly.
fn parse_path_list(
    bytes: &[u8],
    import_count: u32,
    export_count: u32,
) -> Result<Vec<RawEdge>, String> {
    let mut c = Cursor::new(bytes);
    let count = c
        .compact_index()
        .map_err(|e| format!("PathList count: {e}"))?;
    if count < 0 {
        return Err(format!("negative PathList count {count}"));
    }
    let count = count as usize;
    let mut out = Vec::with_capacity(count.min(4096));
    for k in 0..count {
        let start = c
            .compact_index()
            .map_err(|e| format!("edge {k} Start: {e}"))?;
        let end = c
            .compact_index()
            .map_err(|e| format!("edge {k} End: {e}"))?;
        let collision_radius = c
            .u16()
            .map_err(|e| format!("edge {k} CollisionRadius: {e}"))?;
        let collision_height = c
            .u16()
            .map_err(|e| format!("edge {k} CollisionHeight: {e}"))?;
        let reach_flags = c.u32().map_err(|e| format!("edge {k} reachFlags: {e}"))?;
        let distance = c.u16().map_err(|e| format!("edge {k} Distance: {e}"))?;
        let start = ObjectRef::from_raw(start, import_count, export_count)
            .ok_or_else(|| format!("edge {k} Start ref {start} out of range"))?;
        let end = ObjectRef::from_raw(end, import_count, export_count)
            .ok_or_else(|| format!("edge {k} End ref {end} out of range"))?;
        out.push(RawEdge {
            start,
            end,
            collision_radius,
            collision_height,
            reach_flags,
            distance,
        });
    }
    if c.pos() != bytes.len() {
        return Err(format!(
            "{} trailing bytes after {count} ReachSpecs",
            bytes.len() - c.pos()
        ));
    }
    Ok(out)
}

/// Decodes every navigation point and `ReachSpec` edge of a map.
///
/// A class is a navigation point when its inheritance chain contains `NavigationPoint`
/// (`ClassDefaults::is_navigation_point`). The map's own tagged property wins over the
/// inherited class default for `Location`/`CollisionRadius`/`CollisionHeight`.
pub fn decode_navigation(
    cache: &mut PackageCache,
    defaults: &mut ClassDefaults,
    map: &str,
) -> Result<Navigation, String> {
    let map_pkg = cache.map(map)?;
    let p = &map_pkg.package;
    let data = &map_pkg.data;
    let import_count = p.imports().len() as u32;
    let export_count = p.exports().len() as u32;

    let mut nav = Navigation::default();
    let mut tails: Vec<(usize, Span)> = Vec::new();

    for i in 0..p.exports().len() {
        if p.exports()[i].serial_size == 0 {
            continue;
        }
        let Some(class) = p.export_class_path(i).map(str::to_owned) else {
            continue;
        };
        match defaults.is_navigation_point(&class) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(e) => {
                // Native-only classes (e.g. `Engine.Polys`) are not decoded script classes and
                // cannot inherit the script `NavigationPoint`; only suspect ones are recorded.
                let short = class.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
                let nav_like = ["nav", "path", "point", "ladder", "teleport", "start"]
                    .iter()
                    .any(|k| short.contains(k));
                if nav_like {
                    nav.unresolved_class.push((i, format!("{class}: {e}")));
                }
                continue;
            }
        }
        let o = match p.read_object_properties(data, i, &Limits::default()) {
            Ok(o) => o,
            Err(e) => {
                nav.property_failures.push((i, e.to_string()));
                continue;
            }
        };
        let props = Props::new(p, &o);
        let location = props
            .vector("Location")
            .or_else(|| defaults.vector_default(&class, "Location").ok().flatten())
            .unwrap_or([0.0; 3]);
        let (collision_radius, radius_source) = match props.float("CollisionRadius") {
            Some(v) => (v, "map"),
            None => match defaults.float_default(&class, "CollisionRadius") {
                Ok(Some(v)) => (v, "class_default"),
                _ => (0.0, "engine_default"),
            },
        };
        let (collision_height, height_source) = match props.float("CollisionHeight") {
            Some(v) => (v, "map"),
            None => match defaults.float_default(&class, "CollisionHeight") {
                Ok(Some(v)) => (v, "class_default"),
                _ => (0.0, "engine_default"),
            },
        };
        let path = p
            .object_path(ObjectRef::Export(i as u32))
            .unwrap_or("?")
            .to_owned();
        let idx = nav.points.len();
        nav.by_export.insert(i, idx);
        *nav.class_counts.entry(class.clone()).or_default() += 1;
        nav.points.push(NavPoint {
            export: i,
            class,
            path,
            location,
            collision_radius,
            collision_height,
            radius_source,
            height_source,
        });
        let tail = o.tail();
        if tail.is_empty() {
            nav.empty_path_lists += 1;
        } else {
            tails.push((idx, tail));
        }
    }

    for (owner, span) in tails {
        let bytes = &data[span.start..span.end];
        match parse_path_list(bytes, import_count, export_count) {
            Ok(raw_edges) => {
                for raw in raw_edges {
                    let start_point = match raw.start {
                        ObjectRef::Export(i) => nav.by_export.get(&(i as usize)).copied(),
                        _ => None,
                    };
                    let end_point = match raw.end {
                        ObjectRef::Export(i) => nav.by_export.get(&(i as usize)).copied(),
                        _ => None,
                    };
                    if raw.start.is_null() {
                        nav.null_start_edges += 1;
                    } else if start_point != Some(owner) {
                        nav.start_mismatch_edges += 1;
                    }
                    match raw.end {
                        ObjectRef::Import(_) => nav.import_end_edges += 1,
                        ObjectRef::Export(_) if end_point.is_none() => {
                            nav.unresolved_end_edges += 1
                        }
                        _ => {}
                    }
                    *nav.flags_hist.entry(raw.reach_flags).or_default() += 1;
                    nav.edge_count += 1;
                    nav.edges.push(ReachEdge {
                        owner,
                        start: raw.start,
                        end: raw.end,
                        start_point,
                        end_point,
                        collision_radius: raw.collision_radius,
                        collision_height: raw.collision_height,
                        reach_flags: raw.reach_flags,
                        distance: raw.distance,
                    });
                }
            }
            Err(e) => nav.tail_failures.push((
                nav.points[owner].export,
                format!("{e} ({} bytes)", span.len()),
            )),
        }
    }

    Ok(nav)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds a `PathList` tail with `count` explicit compact refs/values.
    fn encode(refs: &[(i32, i32, u16, u16, u32, u16)]) -> Vec<u8> {
        fn compact(value: i32) -> Vec<u8> {
            let mut m = value.unsigned_abs();
            let mut first = (m & 0x3f) as u8;
            if value < 0 {
                first |= 0x80;
            }
            m >>= 6;
            if m != 0 {
                first |= 0x40;
            }
            let mut out = vec![first];
            while m != 0 {
                let mut b = (m & 0x7f) as u8;
                m >>= 7;
                if m != 0 {
                    b |= 0x80;
                }
                out.push(b);
            }
            out
        }
        let mut out = compact(refs.len() as i32);
        for (s, e, r, h, f, d) in refs {
            out.extend(compact(*s));
            out.extend(compact(*e));
            out.extend_from_slice(&r.to_le_bytes());
            out.extend_from_slice(&h.to_le_bytes());
            out.extend_from_slice(&f.to_le_bytes());
            out.extend_from_slice(&d.to_le_bytes());
        }
        out
    }

    #[test]
    fn parses_two_edges_and_resolves_references() {
        // Start = export 0 (raw 1), End = export 1 (raw 2), plus a null start edge.
        let bytes = encode(&[
            (1, 2, 120, 120, reach_flags::WALK, 384),
            (0, 5, 72, 100, reach_flags::LADDER, 462),
        ]);
        let edges = parse_path_list(&bytes, 4, 16).expect("parse");
        assert_eq!(edges.len(), 2);
        assert_eq!(edges[0].start, ObjectRef::Export(0));
        assert_eq!(edges[0].end, ObjectRef::Export(1));
        assert_eq!(edges[0].collision_radius, 120);
        assert_eq!(edges[0].collision_height, 120);
        assert_eq!(edges[0].reach_flags, 1);
        assert_eq!(edges[0].distance, 384);
        assert!(edges[1].start.is_null());
        assert_eq!(edges[1].end, ObjectRef::Export(4));
        assert_eq!(edges[1].reach_flags, reach_flags::LADDER);
    }

    #[test]
    fn empty_path_list_is_just_a_count() {
        let bytes = encode(&[]);
        assert_eq!(bytes, vec![0]);
        assert!(parse_path_list(&bytes, 0, 0).unwrap().is_empty());
    }

    #[test]
    fn rejects_trailing_bytes_and_out_of_range_refs() {
        let mut bytes = encode(&[(1, 2, 10, 20, 1, 30)]);
        bytes.push(0xAA);
        let err = parse_path_list(&bytes, 0, 4).unwrap_err();
        assert!(err.contains("trailing bytes"), "{err}");
        // End raw 9 needs 8 exports; only 2 exist.
        let bytes = encode(&[(1, 9, 10, 20, 1, 30)]);
        let err = parse_path_list(&bytes, 0, 2).unwrap_err();
        assert!(err.contains("out of range"), "{err}");
    }

    #[test]
    fn rejects_truncated_edge() {
        let mut bytes = encode(&[(1, 2, 10, 20, 1, 30)]);
        bytes.truncate(bytes.len() - 1);
        let err = parse_path_list(&bytes, 0, 4).unwrap_err();
        assert!(err.contains("Distance") || err.contains("EOF") || !err.is_empty());
    }

    #[test]
    fn negative_count_is_rejected() {
        // compact(-1) = 0x81.
        let bytes = vec![0x81];
        let err = parse_path_list(&bytes, 0, 0).unwrap_err();
        assert!(err.contains("negative"), "{err}");
    }

    #[test]
    fn flag_names_cover_observed_values() {
        assert_eq!(reach_flags::names(1), vec!["R_WALK"]);
        assert_eq!(reach_flags::names(64), vec!["R_LADDER"]);
        assert_eq!(reach_flags::names(128), vec!["R_PROSCRIBED"]);
        assert_eq!(reach_flags::names(1 | 16), vec!["R_WALK", "R_DOOR"]);
        assert_eq!(reach_flags::names(0), vec!["none"]);
    }
}
