//! Axis-aligned bounding box and a flat-array binary BVH broad phase.

use crate::{Triangle, Vec3};

/// Axis-aligned bounding box.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Aabb {
    /// Minimum corner.
    pub min: Vec3,
    /// Maximum corner.
    pub max: Vec3,
}

impl Aabb {
    /// An empty (inverted) box that grows to include points.
    pub fn empty() -> Self {
        Self {
            min: [f32::INFINITY; 3],
            max: [f32::NEG_INFINITY; 3],
        }
    }

    /// Box around a center and half extents.
    pub fn from_center_half(center: Vec3, half: Vec3) -> Self {
        Self {
            min: [
                center[0] - half[0],
                center[1] - half[1],
                center[2] - half[2],
            ],
            max: [
                center[0] + half[0],
                center[1] + half[1],
                center[2] + half[2],
            ],
        }
    }

    /// Grows the box to include a point.
    pub fn include(&mut self, p: Vec3) {
        for (k, v) in p.iter().enumerate() {
            self.min[k] = self.min[k].min(*v);
            self.max[k] = self.max[k].max(*v);
        }
    }

    /// Grows the box to include a triangle.
    pub fn include_triangle(&mut self, t: &Triangle) {
        for v in t {
            self.include(*v);
        }
    }

    /// Smallest box containing both.
    #[must_use]
    pub fn union(&self, other: &Aabb) -> Aabb {
        let mut out = *self;
        for k in 0..3 {
            out.min[k] = out.min[k].min(other.min[k]);
            out.max[k] = out.max[k].max(other.max[k]);
        }
        out
    }

    /// Overlap test (touching counts as overlap).
    pub fn overlaps(&self, other: &Aabb) -> bool {
        (0..3).all(|k| self.min[k] <= other.max[k] && self.max[k] >= other.min[k])
    }

    /// Center.
    pub fn center(&self) -> Vec3 {
        [
            (self.min[0] + self.max[0]) * 0.5,
            (self.min[1] + self.max[1]) * 0.5,
            (self.min[2] + self.max[2]) * 0.5,
        ]
    }

    /// Longest axis index.
    pub fn longest_axis(&self) -> usize {
        let d = [
            self.max[0] - self.min[0],
            self.max[1] - self.min[1],
            self.max[2] - self.min[2],
        ];
        if d[0] >= d[1] && d[0] >= d[2] {
            0
        } else if d[1] >= d[2] {
            1
        } else {
            2
        }
    }
}

/// Builds a triangle AABB.
fn tri_bounds(t: &Triangle) -> Aabb {
    let mut b = Aabb::empty();
    b.include_triangle(t);
    b
}

struct Node {
    bounds: Aabb,
    /// Leaf: first index into `order` and count. Internal: child node indices.
    a: u32,
    b: u32,
    count: u32,
}

/// A binary bounding-volume hierarchy over triangles, built once.
pub struct Bvh {
    nodes: Vec<Node>,
    order: Vec<u32>,
}

/// Maximum triangles per leaf.
const LEAF_SIZE: usize = 8;

impl Bvh {
    /// Builds the hierarchy from triangles. Empty input yields zero nodes.
    pub fn build(tris: &[Triangle]) -> Self {
        let mut order: Vec<u32> = (0..tris.len() as u32).collect();
        let bounds: Vec<Aabb> = tris.iter().map(tri_bounds).collect();
        let mut nodes: Vec<Node> = Vec::new();
        if !tris.is_empty() {
            let mut indices: Vec<u32> = (0..tris.len() as u32).collect();
            build_node(&bounds, &mut indices, &mut order, &mut nodes);
        }
        Self { nodes, order }
    }

    /// Number of nodes (diagnostic).
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    /// Calls `f` for every triangle whose bounds overlap `query`.
    pub fn traverse(&self, query: Aabb, mut f: impl FnMut(u32)) {
        if self.nodes.is_empty() {
            return;
        }
        let mut stack = vec![0u32];
        while let Some(n) = stack.pop() {
            let node = &self.nodes[n as usize];
            if !node.bounds.overlaps(&query) {
                continue;
            }
            if node.count > 0 {
                for i in node.a..node.a + node.count {
                    f(self.order[i as usize]);
                }
            } else {
                stack.push(node.a);
                stack.push(node.b);
            }
        }
    }
}

/// Recursively builds a node for `indices[a..b]`, appending leaves to `order`.
fn build_node(
    bounds: &[Aabb],
    indices: &mut [u32],
    order: &mut Vec<u32>,
    nodes: &mut Vec<Node>,
) -> u32 {
    let mut bb = Aabb::empty();
    let mut centroid_bb = Aabb::empty();
    for &i in indices.iter() {
        bb = bb.union(&bounds[i as usize]);
        centroid_bb.include(bounds[i as usize].center());
    }
    let node_index = nodes.len() as u32;
    nodes.push(Node {
        bounds: bb,
        a: 0,
        b: 0,
        count: 0,
    });
    if indices.len() <= LEAF_SIZE {
        let start = order.len() as u32;
        for &i in indices.iter() {
            order.push(i);
        }
        nodes[node_index as usize].a = start;
        nodes[node_index as usize].count = indices.len() as u32;
        return node_index;
    }
    let axis = centroid_bb.longest_axis();
    indices.sort_by(|&x, &y| {
        bounds[x as usize].center()[axis]
            .partial_cmp(&bounds[y as usize].center()[axis])
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mid = indices.len() / 2;
    let (left, right) = indices.split_at_mut(mid);
    let l = build_node(bounds, left, order, nodes);
    let r = build_node(bounds, right, order, nodes);
    nodes[node_index as usize].a = l;
    nodes[node_index as usize].b = r;
    node_index
}
