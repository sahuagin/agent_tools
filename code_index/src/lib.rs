//! Public types, storage trait, and graph traits for code-index.
//!
//! Concrete implementations live in sibling modules (added incrementally).
//! The `Store` trait is the durable boundary — swappable between sqlite,
//! redb, lance, etc. The `Graph` type lives in memory and is hydrated from
//! the store; `GraphAnalyzer` operates on `Graph` and never touches `Store`.

use std::ops::Range;
use std::path::{Path, PathBuf};

use anyhow::Result;
use serde::{Deserialize, Serialize};

pub mod chunker;
pub mod edges;
pub mod embed;
pub mod graph;
pub mod ingest;
pub mod recall;
pub mod store;
pub mod walker;

// ──────────────────────────────────────────────────────────────────────────
// Identity & enums
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ChunkId(pub i64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChunkKind {
    Function,
    Method,
    Class,
    Struct,
    Enum,
    Trait,
    Impl,
    Interface,
    Type,
    Module,
    Constant,
    Macro,
    Test,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Calls,
    References,
    Implements,
    Imports,
    DefinedIn,
    ImportedBy,
    TestOf,
}

// ──────────────────────────────────────────────────────────────────────────
// Core data
// ──────────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Chunk {
    pub id: ChunkId,
    pub file: PathBuf,
    pub lines: Range<usize>,
    pub kind: ChunkKind,
    pub name: String,
    /// Hash of the chunk body — used to detect re-extraction need.
    pub signature_hash: u64,
    pub text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub from: ChunkId,
    pub to: ChunkId,
    pub kind: EdgeKind,
    /// 1.0 for AST-derived edges; <1.0 for inferred (LLM or heuristic).
    pub confidence: f32,
}

// ──────────────────────────────────────────────────────────────────────────
// Storage trait — durable boundary
// ──────────────────────────────────────────────────────────────────────────

pub trait Store {
    // chunks
    fn upsert_chunk(&mut self, c: &Chunk) -> Result<ChunkId>;
    fn delete_chunk(&mut self, id: ChunkId) -> Result<()>;
    fn get_chunk(&self, id: ChunkId) -> Result<Option<Chunk>>;
    fn list_chunks_by_file(&self, file: &Path) -> Result<Vec<Chunk>>;

    // edges
    fn upsert_edge(&mut self, e: &Edge) -> Result<()>;
    fn delete_edges_from(&mut self, from: ChunkId) -> Result<()>;
    fn iter_edges(&self) -> Result<Vec<Edge>>;

    // embeddings + vector search.
    // recall_top_k returns lightweight (id, score) pairs; the caller decides
    // which deserve a heap walk via get_chunk. See `Store` docstring for the
    // readv-style rationale.
    fn upsert_embedding(&mut self, id: ChunkId, model: &str, vec: &[f32]) -> Result<()>;
    fn recall_top_k(
        &self,
        model: &str,
        query: &[f32],
        k: usize,
    ) -> Result<Vec<(ChunkId, f32)>>;
    /// Chunks that don't yet have an embedding for `model`. Used by the
    /// embedding pass to find work after a re-chunk.
    fn chunks_missing_embedding(&self, model: &str) -> Result<Vec<Chunk>>;

    /// Lexical (BM25-style) recall. Returns `(id, score)` pairs ranked
    /// by relevance to `query`; same readv-style scatter shape as
    /// `recall_top_k`. Score is non-negative; higher is better. Backing
    /// implementation may use a full-text index (sqlite FTS5 in the
    /// SqliteStore case) or a simpler substring scan — callers should
    /// not depend on a specific scoring scale.
    fn recall_lexical(
        &self,
        query: &str,
        k: usize,
    ) -> Result<Vec<(ChunkId, f32)>>;

    /// All file paths that have been ingested (per the file_manifest).
    /// Used by the edge-build pass to know which files to re-parse for
    /// reference extraction.
    fn list_known_files(&self) -> Result<Vec<PathBuf>>;

    /// Lookup chunks by exact `name` match. Used for name-based edge
    /// resolution: the FROM is determined by parent-walk during
    /// reference extraction, the TO is found by name-matching against
    /// every known chunk. Multiple matches are common (e.g. `new`,
    /// `default`); the caller decides how to disambiguate.
    fn find_chunks_by_name(&self, name: &str) -> Result<Vec<Chunk>>;

    // manifest / staleness
    fn file_signature(&self, file: &Path) -> Result<Option<u64>>;
    fn set_file_signature(&mut self, file: &Path, hash: u64) -> Result<()>;
}

// ──────────────────────────────────────────────────────────────────────────
// Graph & analyzer trait
// ──────────────────────────────────────────────────────────────────────────

pub use graph::{Community, Graph};

pub trait GraphAnalyzer {
    fn community_detection(&self, g: &Graph) -> Result<Vec<Community>>;
    fn centrality(&self, g: &Graph) -> Result<std::collections::HashMap<ChunkId, f64>>;
    fn shortest_path(
        &self,
        g: &Graph,
        from: ChunkId,
        to: ChunkId,
    ) -> Result<Vec<ChunkId>>;
}
