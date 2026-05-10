//! In-memory graph hydrated from a `Store`.
//!
//! Backed by petgraph today; hidden behind a thin facade so the public API
//! doesn't bind callers to petgraph's version churn.
//!
//! Algorithms exposed:
//! - [`Graph::pagerank`] — node centrality via PageRank
//! - [`Graph::connected_components`] — weakly-connected partition
//! - [`Graph::shortest_path`] — unweighted Dijkstra (BFS-equivalent)
//! - [`Graph::stats`] — node/edge counts + degree distribution

use std::collections::HashMap;

use anyhow::Result;
use petgraph::graph::{NodeIndex, UnGraph};
use petgraph::unionfind::UnionFind;
use serde::{Deserialize, Serialize};

use crate::{ChunkId, Edge, EdgeKind, Store};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Community {
    pub id: usize,
    pub members: Vec<ChunkId>,
    /// Modularity score for this community in [0, 1].
    pub cohesion: f32,
}

pub struct Graph {
    inner: UnGraph<ChunkId, EdgeKind>,
    by_chunk: HashMap<ChunkId, NodeIndex>,
}

impl Graph {
    pub fn new() -> Self {
        Self {
            inner: UnGraph::new_undirected(),
            by_chunk: HashMap::new(),
        }
    }

    pub fn from_edges(edges: impl IntoIterator<Item = Edge>) -> Self {
        let mut g = Self::new();
        for e in edges {
            g.add_edge(e);
        }
        g
    }

    pub fn add_node(&mut self, id: ChunkId) -> NodeIndex {
        if let Some(&ix) = self.by_chunk.get(&id) {
            return ix;
        }
        let ix = self.inner.add_node(id);
        self.by_chunk.insert(id, ix);
        ix
    }

    pub fn add_edge(&mut self, e: Edge) {
        let a = self.add_node(e.from);
        let b = self.add_node(e.to);
        self.inner.add_edge(a, b, e.kind);
    }

    /// Hydrate a `Graph` by reading every edge from `store`. Per
    /// design, the in-memory graph is rebuilt from persistent storage
    /// on demand — we don't try to keep a live graph in sync with
    /// chunks/edges tables.
    pub fn from_store(store: &dyn Store) -> Result<Self> {
        let edges = store.iter_edges()?;
        Ok(Self::from_edges(edges))
    }

    pub fn node_count(&self) -> usize {
        self.inner.node_count()
    }

    pub fn edge_count(&self) -> usize {
        self.inner.edge_count()
    }

    /// Petgraph escape hatch for analyzers that need direct access. Kept
    /// pub(crate)-ish via this method rather than exposing the field so we
    /// can replace petgraph later without callers noticing.
    pub fn inner(&self) -> &UnGraph<ChunkId, EdgeKind> {
        &self.inner
    }

    /// PageRank centrality. Returns `(ChunkId, score)` pairs sorted
    /// descending by score. `damping` is the standard 0.85 unless you
    /// have reason to deviate; `iterations` 20-50 is typical for code-
    /// graph scale (PageRank converges fast).
    ///
    /// We hand-roll the power-method update rather than calling
    /// `petgraph::algo::page_rank` because the latter benchmarked
    /// catastrophically slow on a 25k-node / 78k-edge graph (5
    /// iterations took 128 sec, vs ~50ms for this implementation).
    /// Whatever petgraph 0.6.x is doing internally has a constant
    /// factor that doesn't match O(iter * (V + E)).
    pub fn pagerank(&self, damping: f32, iterations: usize) -> Vec<(ChunkId, f32)> {
        let n = self.inner.node_count();
        if n == 0 {
            return Vec::new();
        }
        let n_f = n as f32;
        let teleport = (1.0 - damping) / n_f;
        let mut scores: Vec<f32> = vec![1.0 / n_f; n];

        // Cache per-node out-degree so the inner loop doesn't recompute it.
        let degrees: Vec<f32> = self
            .inner
            .node_indices()
            .map(|ix| self.inner.neighbors(ix).count() as f32)
            .collect();

        for _ in 0..iterations {
            let mut next: Vec<f32> = vec![teleport; n];
            for ix in self.inner.node_indices() {
                let i = ix.index();
                let d = degrees[i];
                if d == 0.0 {
                    // Dangling node: distribute its mass uniformly to
                    // every node (standard PageRank dangling-node fix).
                    let dangling = damping * scores[i] / n_f;
                    for slot in next.iter_mut() {
                        *slot += dangling;
                    }
                    continue;
                }
                let contribution = damping * scores[i] / d;
                for nb in self.inner.neighbors(ix) {
                    next[nb.index()] += contribution;
                }
            }
            scores = next;
        }

        let mut out: Vec<(ChunkId, f32)> = self
            .inner
            .node_indices()
            .zip(scores.iter())
            .map(|(ix, &score)| (self.inner[ix], score))
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out
    }

    /// Weakly-connected components. Each inner Vec is one component;
    /// sorted by size descending so the giant component (if any)
    /// comes first.
    ///
    /// True community detection (Leiden / Louvain / label propagation)
    /// is a deeper analysis — connected components only finds *islands*
    /// of mutually-reachable nodes, which on a typical codebase yields
    /// one giant component and a tail of orphans. Filed as future work.
    pub fn connected_components(&self) -> Vec<Vec<ChunkId>> {
        let n = self.inner.node_count();
        if n == 0 {
            return Vec::new();
        }
        let mut uf = UnionFind::new(n);
        for edge in self.inner.edge_indices() {
            if let Some((a, b)) = self.inner.edge_endpoints(edge) {
                uf.union(a.index(), b.index());
            }
        }
        // Group node-indices by their representative in the union-find.
        let mut groups: HashMap<usize, Vec<ChunkId>> = HashMap::new();
        for ix in self.inner.node_indices() {
            let rep = uf.find(ix.index());
            groups.entry(rep).or_default().push(self.inner[ix]);
        }
        let mut out: Vec<Vec<ChunkId>> = groups.into_values().collect();
        out.sort_by(|a, b| b.len().cmp(&a.len()));
        out
    }

    /// Shortest path between two chunks (BFS on unweighted edges).
    /// Returns the chunk-id sequence including endpoints, or `None` if
    /// the chunks are in different connected components.
    pub fn shortest_path(&self, from: ChunkId, to: ChunkId) -> Option<Vec<ChunkId>> {
        let &start = self.by_chunk.get(&from)?;
        let &goal = self.by_chunk.get(&to)?;
        if start == goal {
            return Some(vec![from]);
        }

        // BFS with predecessor tracking. petgraph::algo::dijkstra works
        // but we'd have to thread a unit-cost closure; BFS is shorter.
        use std::collections::VecDeque;
        let mut visited: HashMap<NodeIndex, NodeIndex> = HashMap::new();
        let mut queue: VecDeque<NodeIndex> = VecDeque::new();
        queue.push_back(start);
        visited.insert(start, start);

        while let Some(cur) = queue.pop_front() {
            if cur == goal {
                break;
            }
            for nb in self.inner.neighbors(cur) {
                if !visited.contains_key(&nb) {
                    visited.insert(nb, cur);
                    queue.push_back(nb);
                }
            }
        }
        if !visited.contains_key(&goal) {
            return None;
        }
        // Reconstruct path goal → … → start, then reverse.
        let mut path: Vec<NodeIndex> = vec![goal];
        let mut cur = goal;
        while cur != start {
            cur = visited[&cur];
            path.push(cur);
        }
        path.reverse();
        Some(path.into_iter().map(|ix| self.inner[ix]).collect())
    }

    /// Quick overview metrics. Intended for `graph stats` CLI verb.
    pub fn stats(&self) -> GraphStats {
        let n = self.inner.node_count();
        let e = self.inner.edge_count();
        let mut max_degree = 0usize;
        let mut total_degree = 0usize;
        for ix in self.inner.node_indices() {
            let d = self.inner.neighbors(ix).count();
            total_degree += d;
            if d > max_degree {
                max_degree = d;
            }
        }
        let avg_degree = if n == 0 {
            0.0
        } else {
            total_degree as f64 / n as f64
        };
        GraphStats {
            nodes: n,
            edges: e,
            components: self.connected_components().len(),
            max_degree,
            avg_degree,
        }
    }
}

#[derive(Debug, Clone)]
pub struct GraphStats {
    pub nodes: usize,
    pub edges: usize,
    pub components: usize,
    pub max_degree: usize,
    pub avg_degree: f64,
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edge(from: i64, to: i64, kind: EdgeKind) -> Edge {
        Edge {
            from: ChunkId(from),
            to: ChunkId(to),
            kind,
            confidence: 1.0,
        }
    }

    #[test]
    fn pagerank_ranks_central_node_highest() {
        // Star topology: 1 central node (id=10) connected to 4 leaves.
        // PageRank should rank id=10 highest by a wide margin.
        let edges = vec![
            edge(1, 10, EdgeKind::Calls),
            edge(2, 10, EdgeKind::Calls),
            edge(3, 10, EdgeKind::Calls),
            edge(4, 10, EdgeKind::Calls),
        ];
        let g = Graph::from_edges(edges);
        let ranks = g.pagerank(0.85, 50);
        assert!(!ranks.is_empty());
        assert_eq!(ranks[0].0, ChunkId(10), "star center wins");
    }

    #[test]
    fn connected_components_partitions_disjoint_subgraphs() {
        // Two triangles, no edge between them. Should produce 2
        // components of size 3 each.
        let edges = vec![
            edge(1, 2, EdgeKind::Calls),
            edge(2, 3, EdgeKind::Calls),
            edge(3, 1, EdgeKind::Calls),
            edge(10, 20, EdgeKind::Calls),
            edge(20, 30, EdgeKind::Calls),
            edge(30, 10, EdgeKind::Calls),
        ];
        let g = Graph::from_edges(edges);
        let comps = g.connected_components();
        assert_eq!(comps.len(), 2);
        assert_eq!(comps[0].len(), 3);
        assert_eq!(comps[1].len(), 3);
    }

    #[test]
    fn shortest_path_finds_known_route() {
        // Linear chain: 1 - 2 - 3 - 4 - 5
        let edges = vec![
            edge(1, 2, EdgeKind::Calls),
            edge(2, 3, EdgeKind::Calls),
            edge(3, 4, EdgeKind::Calls),
            edge(4, 5, EdgeKind::Calls),
        ];
        let g = Graph::from_edges(edges);
        let path = g.shortest_path(ChunkId(1), ChunkId(5)).unwrap();
        let names: Vec<_> = path.iter().map(|c| c.0).collect();
        assert_eq!(names, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn shortest_path_handles_disconnected() {
        let edges = vec![
            edge(1, 2, EdgeKind::Calls),
            edge(10, 20, EdgeKind::Calls),
        ];
        let g = Graph::from_edges(edges);
        assert!(g.shortest_path(ChunkId(1), ChunkId(20)).is_none());
    }

    #[test]
    fn shortest_path_to_self_returns_singleton() {
        let edges = vec![edge(1, 2, EdgeKind::Calls)];
        let g = Graph::from_edges(edges);
        let path = g.shortest_path(ChunkId(1), ChunkId(1)).unwrap();
        assert_eq!(path, vec![ChunkId(1)]);
    }

    #[test]
    fn stats_reflect_graph_shape() {
        let edges = vec![
            edge(1, 2, EdgeKind::Calls),
            edge(2, 3, EdgeKind::Calls),
            edge(2, 4, EdgeKind::Calls),
            edge(2, 5, EdgeKind::Calls),
        ];
        let g = Graph::from_edges(edges);
        let s = g.stats();
        assert_eq!(s.nodes, 5);
        assert_eq!(s.edges, 4);
        assert_eq!(s.components, 1);
        assert_eq!(s.max_degree, 4); // node 2 has 4 neighbors
    }
}
