//! Navigation-provider bridge between the interpreter and the decoded `ReachSpec` graph.
//!
//! Coordinates crossing this trait are **Unreal** units and axes (X forward, Y right, **Z up**),
//! exactly like [`crate::physics::WorldPhysics`]. The VM performs the path search and the
//! movement stepping; the host only supplies the graph decoded from a map
//! (`xiii-world::nav_provider` wraps `xiii-world::navigation`).
//!
//! This module is deliberately dependency-free and knows nothing about packages, maps or actors:
//! a navigation point carries its **script object path** (the VM resolves it to the map instance)
//! and the graph is a flat point list plus directed edge list. No provider installed: every
//! pathing native fails with [`crate::vm::VmErrorKind::NoNavProvider`] — never a silent success.

/// A navigation point (a decoded `NavigationPoint` instance) in Unreal space.
#[derive(Debug, Clone, PartialEq)]
pub struct NavPointInfo {
    /// Script object path of the map instance (resolved by the VM with `Vm::find_object`).
    pub actor: String,
    /// `Location` (Unreal units, Z up).
    pub location: [f32; 3],
    /// `CollisionRadius` (Unreal units).
    pub collision_radius: f32,
    /// `CollisionHeight`, a half height (Unreal units).
    pub collision_height: f32,
}

/// A directed `ReachSpec` edge: usable from `start` to `end` by a pawn that fits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NavEdgeInfo {
    /// Index into the point list the edge starts from.
    pub start: u32,
    /// Index into the point list the edge leads to.
    pub end: u32,
    /// Largest collision radius that can traverse this edge (Unreal units).
    pub collision_radius: u16,
    /// Largest collision (half) height that can traverse this edge (Unreal units).
    pub collision_height: u16,
    /// `reachFlags` bit field (see [`reach_flags`]).
    pub reach_flags: u32,
    /// Cached edge distance (Unreal units).
    pub distance: u16,
}

/// `EReachSpecFlags` bit meanings (upstream UE2 `UnPath.h`; hypothesis for XIII, corroborated by
/// the decoded values — see `xiii-world::navigation`). Duplicated here so this crate stays
/// dependency-free.
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
}

/// Decoded navigation graph the VM queries. A provider may cache; it never invents points.
pub trait NavigationData {
    /// All navigation points, indexed by the ids used in [`NavEdgeInfo`].
    fn points(&self) -> &[NavPointInfo];
    /// All directed `ReachSpec` edges.
    fn edges(&self) -> &[NavEdgeInfo];
}

/// Diagnostic provider with no points and no edges: every path search fails with `Ok(None)`.
/// Used by tests and diagnostics; it is not game data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmptyNavigation;

impl NavigationData for EmptyNavigation {
    fn points(&self) -> &[NavPointInfo] {
        &[]
    }
    fn edges(&self) -> &[NavEdgeInfo] {
        &[]
    }
}

/// Index of the navigation point nearest to `location` (Euclidean; ties break to the lower id).
/// `None` only for an empty graph.
pub fn nearest_point(points: &[NavPointInfo], location: [f32; 3]) -> Option<u32> {
    let mut best: Option<(f32, u32)> = None;
    for (i, p) in points.iter().enumerate() {
        let d = dist_sq(p.location, location);
        if best.is_none_or(|(bd, _)| d < bd) {
            best = Some((d, i as u32));
        }
    }
    best.map(|(_, i)| i)
}

/// Whether a pawn with the given half-extents fits through `edge` (upstream `ReachSpec`
/// `CollisionRadius`/`CollisionHeight` are the **largest** size the edge admits).
pub fn edge_fits(edge: &NavEdgeInfo, radius: f32, height: f32) -> bool {
    // Engine `ReachSpec.CollisionRadius`/`CollisionHeight` are floats; the decoded corpus stores
    // them as `u16`. A zero decoded value means "unspecified": do not reject on it (the corpus
    // has edges with 0 on otherwise usable walk specs).
    let er = f32::from(edge.collision_radius);
    let eh = f32::from(edge.collision_height);
    (er <= 0.0 || radius <= er) && (eh <= 0.0 || height <= eh)
}

/// Whether an AI controller may traverse `edge` (`R_PROSCRIBED` never; `R_PLAYERONLY` only for
/// player pawns, which this search never represents).
pub fn edge_usable(edge: &NavEdgeInfo, radius: f32, height: f32, player: bool) -> bool {
    if edge.reach_flags & reach_flags::PROSCRIBED != 0 {
        return false;
    }
    if !player && edge.reach_flags & reach_flags::PLAYERONLY != 0 {
        return false;
    }
    edge_fits(edge, radius, height)
}

/// Shortest path over the decoded directed graph from `start` to `goal`, filtered by the pawn's
/// collision size and reach flags. Returns the sequence of point ids beginning at `start` and
/// ending at `goal`, or `None` when no usable path exists.
///
/// Nodes whose own `CollisionRadius`/`CollisionHeight` are smaller than the pawn's are skipped
/// (upstream `NavigationPoint`s are validated against the pawn too). Edge weight is the cached
/// `distance` (at least 1 to keep the search monotone).
pub fn find_path(
    points: &[NavPointInfo],
    edges: &[NavEdgeInfo],
    start: u32,
    goal: u32,
    radius: f32,
    height: f32,
    player: bool,
) -> Option<Vec<u32>> {
    if start as usize >= points.len() || goal as usize >= points.len() {
        return None;
    }
    // The pawn must be able to stand on the goal (its own collision volume). Intermediate nodes
    // are only passed through, so their own size is not checked (upstream filters the edges).
    if !point_fits(&points[goal as usize], radius, height) {
        return None;
    }
    if start == goal {
        return Some(vec![start]);
    }
    // Adjacency built once per call (the graph is small: at most a few hundred nodes).
    let mut adj: Vec<Vec<(u32, f32)>> = vec![Vec::new(); points.len()];
    for e in edges {
        if e.start as usize >= points.len() || e.end as usize >= points.len() {
            continue;
        }
        if !edge_usable(e, radius, height, player) {
            continue;
        }
        adj[e.start as usize].push((e.end, f32::from(e.distance).max(1.0)));
    }
    let n = points.len();
    let mut dist = vec![f32::INFINITY; n];
    let mut prev = vec![u32::MAX; n];
    dist[start as usize] = 0.0;
    // O(n^2) scan: the graph is small and this avoids float-ordering tricks in a heap.
    let mut visited = vec![false; n];
    loop {
        let mut u = usize::MAX;
        let mut best = f32::INFINITY;
        for i in 0..n {
            if !visited[i] && dist[i] < best {
                best = dist[i];
                u = i;
            }
        }
        if u == usize::MAX {
            break;
        }
        if u == goal as usize {
            break;
        }
        visited[u] = true;
        for &(v, w) in &adj[u] {
            let nd = dist[u] + w;
            if nd < dist[v as usize] {
                dist[v as usize] = nd;
                prev[v as usize] = u as u32;
            }
        }
    }
    if !dist[goal as usize].is_finite() {
        return None;
    }
    let mut path = vec![goal];
    let mut cur = goal;
    while cur != start {
        let p = prev[cur as usize];
        if p == u32::MAX {
            return None;
        }
        path.push(p);
        cur = p;
    }
    path.reverse();
    Some(path)
}

/// Whether a pawn of the given size fits on a navigation point.
pub fn point_fits(point: &NavPointInfo, radius: f32, height: f32) -> bool {
    let pr = point.collision_radius;
    let ph = point.collision_height;
    (pr <= 0.0 || radius <= pr) && (ph <= 0.0 || height <= ph)
}

/// One step of latent `MoveTo`/`MoveToward`: move horizontally toward `destination` by at most
/// `speed * dt` Unreal units, stopping exactly on the destination. Returns `(new_location,
/// arrived)`. Arrival is when the horizontal distance is within `radius` (the pawn's collision
/// radius, upstream `MoveTo`'s `MoveTarget` test); the Z component is left to world collision.
pub fn move_step(
    location: [f32; 3],
    destination: [f32; 3],
    speed: f32,
    dt: f32,
    radius: f32,
) -> ([f32; 3], bool) {
    let dx = destination[0] - location[0];
    let dy = destination[1] - location[1];
    let horiz = (dx * dx + dy * dy).sqrt();
    if horiz <= radius {
        return ([location[0], location[1], location[2]], true);
    }
    let step = speed.max(0.0) * dt;
    if step >= horiz {
        return ([destination[0], destination[1], location[2]], true);
    }
    let inv = 1.0 / horiz;
    (
        [
            location[0] + dx * inv * step,
            location[1] + dy * inv * step,
            location[2],
        ],
        false,
    )
}

fn dist_sq(a: [f32; 3], b: [f32; 3]) -> f32 {
    let d = [a[0] - b[0], a[1] - b[1], a[2] - b[2]];
    d[0] * d[0] + d[1] * d[1] + d[2] * d[2]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(actor: &str, loc: [f32; 3], r: f32, h: f32) -> NavPointInfo {
        NavPointInfo {
            actor: actor.into(),
            location: loc,
            collision_radius: r,
            collision_height: h,
        }
    }

    fn e(start: u32, end: u32, r: u16, h: u16, flags: u32, dist: u16) -> NavEdgeInfo {
        NavEdgeInfo {
            start,
            end,
            collision_radius: r,
            collision_height: h,
            reach_flags: flags,
            distance: dist,
        }
    }

    fn line() -> (Vec<NavPointInfo>, Vec<NavEdgeInfo>) {
        (
            vec![
                p("A", [0.0, 0.0, 0.0], 120.0, 120.0),
                p("B", [500.0, 0.0, 0.0], 120.0, 120.0),
                p("C", [1000.0, 0.0, 0.0], 120.0, 120.0),
            ],
            vec![
                e(0, 1, 120, 120, reach_flags::WALK, 500),
                e(1, 2, 120, 120, reach_flags::WALK, 500),
            ],
        )
    }

    #[test]
    fn path_found_returns_the_full_sequence() {
        let (pts, edges) = line();
        let path = find_path(&pts, &edges, 0, 2, 40.0, 80.0, false).expect("path");
        assert_eq!(path, vec![0, 1, 2]);
        // A bigger weight detour is still found as long as it is the only route.
        let (pts, edges) = line();
        assert_eq!(
            find_path(&pts, &edges, 1, 2, 40.0, 80.0, false).unwrap(),
            vec![1, 2]
        );
    }

    #[test]
    fn path_not_found_when_goal_is_disconnected() {
        let (pts, mut edges) = line();
        edges.clear();
        assert!(find_path(&pts, &edges, 0, 2, 40.0, 80.0, false).is_none());
        // Reversed edges do not make a route: the decoded graph is directed.
        edges.push(e(2, 1, 120, 120, reach_flags::WALK, 500));
        edges.push(e(1, 0, 120, 120, reach_flags::WALK, 500));
        assert!(find_path(&pts, &edges, 0, 2, 40.0, 80.0, false).is_none());
    }

    #[test]
    fn pawn_too_big_for_an_edge_has_no_path() {
        let (pts, edges) = line();
        // Pawn radius 200 > edge radius 120.
        assert!(find_path(&pts, &edges, 0, 2, 200.0, 80.0, false).is_none());
        // Pawn height 200 > edge height 120.
        assert!(find_path(&pts, &edges, 0, 2, 40.0, 200.0, false).is_none());
        // Exactly the edge size fits (upstream compares inclusively).
        assert!(find_path(&pts, &edges, 0, 2, 120.0, 120.0, false).is_some());
    }

    #[test]
    fn proscribed_and_player_only_edges_are_rejected_for_ai() {
        let (pts, _) = line();
        // A single proscribed edge blocks the route.
        let edges = vec![e(
            0,
            1,
            120,
            120,
            reach_flags::WALK | reach_flags::PROSCRIBED,
            500,
        )];
        assert!(find_path(&pts, &edges, 0, 1, 40.0, 80.0, false).is_none());
        // Player-only is rejected for AI, accepted for a player.
        let edges = vec![e(
            0,
            1,
            120,
            120,
            reach_flags::WALK | reach_flags::PLAYERONLY,
            500,
        )];
        assert!(find_path(&pts, &edges, 0, 1, 40.0, 80.0, false).is_none());
        assert!(find_path(&pts, &edges, 0, 1, 40.0, 80.0, true).is_some());
    }

    #[test]
    fn zero_decoded_clearance_is_not_treated_as_too_small() {
        // The decoded corpus has usable walk specs with 0 Radius/Height; those must pass.
        let (pts, _) = line();
        let edges = vec![e(0, 1, 0, 0, reach_flags::WALK, 500)];
        assert!(find_path(&pts, &edges, 0, 1, 40.0, 80.0, false).is_some());
    }

    #[test]
    fn a_small_intermediate_node_does_not_block_a_fitting_pawn() {
        // Node B is tiny but the pawn only passes through it: only the edges must admit the pawn,
        // not every node on the path. A strict per-node filter would wrongly report no path.
        let pts = vec![
            p("A", [0.0, 0.0, 0.0], 100.0, 100.0),
            p("B", [500.0, 0.0, 0.0], 20.0, 20.0),
            p("C", [1000.0, 0.0, 0.0], 100.0, 100.0),
        ];
        let edges = vec![
            e(0, 1, 120, 120, reach_flags::WALK, 500),
            e(1, 2, 120, 120, reach_flags::WALK, 500),
        ];
        assert_eq!(
            find_path(&pts, &edges, 0, 2, 40.0, 80.0, false),
            Some(vec![0, 1, 2])
        );
        // The goal node itself, however, must admit the pawn.
        let tiny_goal = vec![p("Z", [0.0, 0.0, 0.0], 20.0, 20.0)];
        assert!(find_path(&tiny_goal, &[], 0, 0, 40.0, 80.0, false).is_none());
    }

    #[test]
    fn move_step_reaches_exactly_and_overshoots() {
        // 300 UU at 100 UU/s, dt 0.1 -> exactly 30 steps with no arrival radius.
        let mut loc = [0.0, 0.0, 0.0];
        let mut ticks = 0;
        loop {
            let (next, arrived) = move_step(loc, [300.0, 0.0, 0.0], 100.0, 0.1, 0.0);
            loc = next;
            ticks += 1;
            if arrived {
                break;
            }
            assert!(ticks <= 31, "did not arrive: {loc:?}");
        }
        assert_eq!(ticks, 30);
        assert_eq!(loc, [300.0, 0.0, 0.0]);

        // One big step overshoots: snap to the destination.
        let (loc, arrived) = move_step([0.0, 0.0, 0.0], [300.0, 0.0, 0.0], 100.0, 5.0, 0.0);
        assert!(arrived);
        assert_eq!(loc, [300.0, 0.0, 0.0]);

        // Inside the arrival radius is already arrived.
        let (loc, arrived) = move_step([295.0, 0.0, 0.0], [300.0, 0.0, 0.0], 100.0, 0.1, 10.0);
        assert!(arrived);
        assert_eq!(loc, [295.0, 0.0, 0.0]);
    }

    #[test]
    fn nearest_point_and_empty_graph() {
        let (pts, _) = line();
        assert_eq!(nearest_point(&pts, [900.0, 0.0, 0.0]), Some(2));
        assert_eq!(nearest_point(&[], [0.0; 3]), None);
    }
}
