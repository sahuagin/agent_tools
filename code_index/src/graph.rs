//! In-memory graph hydrated from a `Store`.
//!
//! Backed by petgraph today; hidden behind a thin facade so the public API
//! doesn't bind callers to petgraph's version churn.

use std::collections::HashMap;

use petgraph::graph::{NodeIndex, UnGraph};
use serde::{Deserialize, Serialize};

use crate::{ChunkId, Edge, EdgeKind};

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
}

impl Default for Graph {
    fn default() -> Self {
        Self::new()
    }
}
