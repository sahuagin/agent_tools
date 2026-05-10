//! `recall` orchestration: combine semantic and lexical retrieval, then
//! materialize chunks for the survivors.
//!
//! Semantic alone has known recall holes (queries describing concepts in
//! prose miss code that uses different terminology), and lexical alone
//! misses paraphrased intent (a query about "promote borrowed to owned"
//! won't find `into_owned()` by string match). Hybrid retrieval gets the
//! best of both — we pull each list's top-K independently, then fuse via
//! Reciprocal Rank Fusion (RRF), a well-studied merging algorithm:
//!
//!     score(d) = sum over each source m: 1 / (k_const + rank_m(d))
//!
//! Where `rank_m(d)` is the document's 1-indexed rank in source `m`'s
//! returned list, or infinity if unranked. `k_const = 60` is the
//! commonly-cited constant from the original Cormack et al. paper —
//! large enough to dampen rank-1 dominance but small enough that
//! actual ranking still matters.

use anyhow::{Context, Result};

use crate::embed::Embedder;
use crate::{Chunk, ChunkId, ChunkKind, Store};

/// Reciprocal Rank Fusion smoothing constant. 60 is the value from
/// Cormack/Clarke/Buettcher 2009 and the de-facto default everywhere.
const RRF_K: f32 = 60.0;

/// Default multiplier applied to test-chunk scores after RRF fusion.
/// 0.5 keeps tests visible but ranks them behind equivalent-strength
/// real-source matches.
///
/// Why this defaults to <1: empirically (flywheel-3p4 dogfood, see bead
/// at-ix1) tests over-rank for natural-language queries because their
/// function names tend to be descriptive prose
/// (`shared_dispatch_unknown_tool_returns_invalid_request`,
/// `rpc_errors_on_unknown_command_and_missing_params`) which match
/// query phrasing strongly, while real source code uses terse
/// identifiers that match weakly. Penalizing tests is a coarse-but-
/// effective way to surface the source they exercise.
pub const DEFAULT_TEST_PENALTY: f32 = 0.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecallMode {
    /// Semantic embedding similarity only (the original `recall_top_k`
    /// path). Best for "concept" queries where exact words don't appear.
    Semantic,
    /// Lexical FTS5 BM25 only. Best for "I know the symbol/keyword"
    /// queries; sharp exact-match recall.
    Lexical,
    /// Both, fused via Reciprocal Rank Fusion. Default.
    Hybrid,
}

impl RecallMode {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "semantic" => Some(Self::Semantic),
            "lexical" => Some(Self::Lexical),
            "hybrid" => Some(Self::Hybrid),
            _ => None,
        }
    }
}

/// One result row. `score` is mode-dependent (cosine for Semantic,
/// BM25-flipped for Lexical, RRF-fused for Hybrid). Higher is better
/// in all three cases. `chunk` is `Some` only when the caller asked
/// for materialized results.
#[derive(Debug, Clone)]
pub struct Hit {
    pub id: ChunkId,
    pub score: f32,
    pub chunk: Option<Chunk>,
}

/// Tuning knobs that don't fit the basic mode/k/materialize triple.
/// Defaults match the recommended behavior; pass `..Default::default()`
/// to override individual fields.
#[derive(Debug, Clone, Copy)]
pub struct RecallTuning {
    /// Multiplier applied to test-chunk scores after RRF. 1.0 disables
    /// the penalty. Defaults to `DEFAULT_TEST_PENALTY` (0.5).
    pub test_penalty: f32,
    /// Drop test chunks from results entirely (vs. just down-weighting).
    /// When set, takes precedence over `test_penalty`.
    pub exclude_tests: bool,
}

impl Default for RecallTuning {
    fn default() -> Self {
        Self {
            test_penalty: DEFAULT_TEST_PENALTY,
            exclude_tests: false,
        }
    }
}

/// Heuristic — does this chunk look like test code? We tag tests via
/// three signals (any one is enough):
///
/// 1. `ChunkKind::Test` — explicit. Rare today (most upstream tags.scm
///    files don't emit `@definition.test`), but reserved for when they
///    do.
/// 2. File path contains a tests directory: `/tests/`, `/__tests__/`,
///    `/test/` (singular too), `/spec/`, ending in `_test.<ext>` or
///    `.test.<ext>` or `.spec.<ext>`.
/// 3. Function/method name starts with `test_` or `should_`.
///
/// The combination misclassifies some non-test chunks (e.g. a function
/// named `test_connection` that's part of real connection logic), but
/// the conservative choice of penalty (0.5 not 0) means even false
/// positives stay reachable.
pub(crate) fn looks_like_test(chunk: &Chunk) -> bool {
    if chunk.kind == ChunkKind::Test {
        return true;
    }
    let name_lower = chunk.name.to_ascii_lowercase();
    if name_lower.starts_with("test_") || name_lower.starts_with("should_") {
        return true;
    }
    let path_lower = chunk.file.to_string_lossy().to_ascii_lowercase();

    // Path components named for testing — checking any component (not
    // a substring of the full path) means `tests/integration.rs` matches
    // even without a leading slash, and arbitrary nested paths like
    // `src/lib/tests/foo.rs` match too.
    let test_dirs = ["tests", "test", "__tests__", "spec", "specs"];
    for component in path_lower.split('/') {
        if test_dirs.contains(&component) {
            return true;
        }
    }

    // Filename-shape suffixes: `_test.rs`, `.test.ts`, `.spec.ts`, `_test.py`.
    let ext_markers = ["_test.", ".test.", ".spec.", "_spec."];
    if ext_markers.iter().any(|m| path_lower.contains(m)) {
        return true;
    }
    false
}

/// Default recall path — Hybrid mode. Equivalent to
/// `recall_with_mode(... RecallMode::Hybrid ...)`. Kept for backwards
/// compatibility with callers that don't care about mode selection.
pub fn recall(
    store: &dyn Store,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
    materialize: bool,
) -> Result<Vec<Hit>> {
    recall_with_mode(store, embedder, query, k, materialize, RecallMode::Hybrid)
}

pub fn recall_with_mode(
    store: &dyn Store,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
    materialize: bool,
    mode: RecallMode,
) -> Result<Vec<Hit>> {
    recall_tuned(
        store,
        embedder,
        query,
        k,
        materialize,
        mode,
        RecallTuning::default(),
    )
}

pub fn recall_tuned(
    store: &dyn Store,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
    materialize: bool,
    mode: RecallMode,
    tuning: RecallTuning,
) -> Result<Vec<Hit>> {
    // For hybrid we deliberately pull a wider window from each side
    // than the caller's k so RRF has more candidates to fuse over.
    // 2x the requested k is a common heuristic; keeps cost bounded
    // while giving the merge meaningful overlap to work with.
    //
    // We also OVER-PULL when test filtering is active — if we'll drop
    // some fraction of candidates, we want enough left to fill the
    // requested k. 4x is conservative; tests are typically <30% of a
    // codebase's chunks so 4x ensures we still hit k after filtering.
    let scale = if tuning.exclude_tests || tuning.test_penalty < 1.0 {
        4
    } else {
        2
    };
    let pool_k = if matches!(mode, RecallMode::Hybrid) {
        k.saturating_mul(scale).max(k)
    } else {
        k.saturating_mul(scale).max(k)
    };

    let semantic = match mode {
        RecallMode::Lexical => Vec::new(),
        _ => recall_semantic_inner(store, embedder, query, pool_k)?,
    };
    let lexical = match mode {
        RecallMode::Semantic => Vec::new(),
        _ => store
            .recall_lexical(query, pool_k)
            .with_context(|| "recall_lexical")?,
    };

    let mut scored: Vec<(ChunkId, f32)> = match mode {
        RecallMode::Semantic => semantic,
        RecallMode::Lexical => lexical,
        RecallMode::Hybrid => fuse_rrf(&semantic, &lexical, pool_k),
    };

    // Apply test-aware filtering / weighting. We need chunk metadata
    // (file path, name, kind) for each candidate, so this happens as a
    // post-pass. Cost: O(pool_k) chunk lookups; ~0.5ms each — negligible.
    if tuning.exclude_tests || tuning.test_penalty < 1.0 {
        scored = apply_test_tuning(store, scored, &tuning)?;
    }

    let truncated = scored.into_iter().take(k);
    let mut hits = Vec::with_capacity(k);
    for (id, score) in truncated {
        let chunk = if materialize {
            store
                .get_chunk(id)
                .with_context(|| format!("materializing chunk {id:?}"))?
        } else {
            None
        };
        hits.push(Hit { id, score, chunk });
    }
    Ok(hits)
}

fn apply_test_tuning(
    store: &dyn Store,
    scored: Vec<(ChunkId, f32)>,
    tuning: &RecallTuning,
) -> Result<Vec<(ChunkId, f32)>> {
    let mut out: Vec<(ChunkId, f32)> = Vec::with_capacity(scored.len());
    for (id, score) in scored {
        let chunk = match store.get_chunk(id)? {
            Some(c) => c,
            None => {
                // Chunk vanished between recall and post-pass (race);
                // pass through unchanged. Materialization step will
                // catch this if it matters to the caller.
                out.push((id, score));
                continue;
            }
        };
        let is_test = looks_like_test(&chunk);
        if is_test && tuning.exclude_tests {
            continue;
        }
        let weight = if is_test { tuning.test_penalty } else { 1.0 };
        out.push((id, score * weight));
    }
    // Re-sort after weighting; truncation happens in the caller.
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(out)
}

fn recall_semantic_inner(
    store: &dyn Store,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
) -> Result<Vec<(ChunkId, f32)>> {
    let qvecs = embedder
        .embed(&[query.to_string()])
        .with_context(|| "embedding query")?;
    let qvec = qvecs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors"))?;
    store
        .recall_top_k(embedder.model_id(), &qvec, k)
        .with_context(|| "recall_top_k")
}

/// Reciprocal Rank Fusion of two ranked lists. Each list provides
/// (id, score) pairs in descending-score order. Output is the union
/// of ids, sorted by RRF score descending, truncated to `k`.
///
/// We DELIBERATELY ignore the absolute scores from each source — RRF's
/// design assumes the ranking signals from different methods are not
/// directly comparable on a numeric scale (cosine similarity vs. BM25
/// have completely different distributions). Rank ordering is the only
/// universal currency.
fn fuse_rrf(
    semantic: &[(ChunkId, f32)],
    lexical: &[(ChunkId, f32)],
    k: usize,
) -> Vec<(ChunkId, f32)> {
    use std::collections::HashMap;
    let mut combined: HashMap<ChunkId, f32> = HashMap::new();
    for (rank, (id, _score)) in semantic.iter().enumerate() {
        let r = (rank + 1) as f32; // 1-indexed
        *combined.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + r);
    }
    for (rank, (id, _score)) in lexical.iter().enumerate() {
        let r = (rank + 1) as f32;
        *combined.entry(*id).or_insert(0.0) += 1.0 / (RRF_K + r);
    }
    let mut sorted: Vec<(ChunkId, f32)> = combined.into_iter().collect();
    sorted.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    sorted.truncate(k);
    sorted
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{embed_pending, MockEmbedder};
    use crate::store::SqliteStore;
    use crate::{Chunk, ChunkKind};

    fn dummy(name: &str, text: &str) -> Chunk {
        Chunk {
            id: ChunkId(0),
            file: "f.rs".into(),
            lines: 1..2,
            kind: ChunkKind::Function,
            name: name.into(),
            signature_hash: 0,
            text: text.into(),
        }
    }

    fn seeded_store_with_embeddings() -> SqliteStore {
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.upsert_chunk(&dummy("alpha", "the alpha body")).unwrap();
        s.upsert_chunk(&dummy("beta", "the beta body")).unwrap();
        s.upsert_chunk(&dummy("read_parquet_from_s3", "load data"))
            .unwrap();
        s.upsert_chunk(&dummy("write_csv", "dump to file")).unwrap();
        let m = MockEmbedder::default();
        embed_pending(&mut s, &m, 8).unwrap();
        s
    }

    #[test]
    fn semantic_mode_returns_top_by_cosine() {
        let s = seeded_store_with_embeddings();
        let m = MockEmbedder::default();
        let hits = recall_with_mode(&s, &m, "alpha", 4, false, RecallMode::Semantic)
            .unwrap();
        assert_eq!(hits.len(), 4, "semantic returns up to k regardless of overlap");
        assert!(
            hits[0].score >= hits[1].score,
            "scores must descend for semantic mode"
        );
    }

    #[test]
    fn lexical_mode_finds_exact_token_match() {
        let s = seeded_store_with_embeddings();
        let m = MockEmbedder::default();
        // The chunk "read_parquet_from_s3" should rank first for the
        // query "parquet" because BM25 will score it highest among
        // chunks where the term appears.
        let hits = recall_with_mode(&s, &m, "parquet", 5, true, RecallMode::Lexical)
            .unwrap();
        assert!(!hits.is_empty(), "lexical should match the parquet chunk");
        let top_name = hits[0].chunk.as_ref().unwrap().name.as_str();
        assert_eq!(top_name, "read_parquet_from_s3");
    }

    #[test]
    fn lexical_mode_returns_empty_for_no_matches() {
        let s = seeded_store_with_embeddings();
        let m = MockEmbedder::default();
        let hits =
            recall_with_mode(&s, &m, "completely_unrelated", 5, false, RecallMode::Lexical)
                .unwrap();
        assert!(hits.is_empty());
    }

    #[test]
    fn lexical_mode_handles_punctuation_in_query() {
        // FTS5 syntax has reserved chars (".:*"); we sanitize. Tokens
        // are split on whitespace and stripped to alphanumeric+_.
        let s = seeded_store_with_embeddings();
        let m = MockEmbedder::default();
        // Query has dots and parens that would otherwise trip raw FTS5.
        let hits = recall_with_mode(
            &s,
            &m,
            "pl.read_parquet(s3_path)",
            5,
            true,
            RecallMode::Lexical,
        )
        .unwrap();
        assert!(!hits.is_empty(), "punctuation-laden query must not error");
        // The "parquet" or "s3" tokens should still match read_parquet_from_s3.
        let names: Vec<_> = hits
            .iter()
            .filter_map(|h| h.chunk.as_ref().map(|c| c.name.as_str()))
            .collect();
        assert!(
            names.iter().any(|n| n.contains("parquet")),
            "expected a parquet match, got: {names:?}"
        );
    }

    #[test]
    fn hybrid_mode_combines_both_sources() {
        let s = seeded_store_with_embeddings();
        let m = MockEmbedder::default();
        let hits = recall_with_mode(&s, &m, "parquet", 4, false, RecallMode::Hybrid)
            .unwrap();
        // We have 4 chunks total; hybrid pool is 2k=8; should return all 4.
        assert!(!hits.is_empty());
        // All scores must be in the RRF range — small positive numbers.
        for h in &hits {
            assert!(h.score >= 0.0);
            // RRF max = 2 * (1/(60+1)) ≈ 0.0327
            assert!(h.score < 0.05, "score in RRF range, got {}", h.score);
        }
    }

    #[test]
    fn hybrid_mode_promotes_chunks_ranked_in_both_sources() {
        // A chunk ranked in BOTH semantic and lexical should beat one
        // ranked in only one. With our deterministic mock embedder and
        // FTS5 BM25, if a chunk's name and body both contain "parquet"
        // it'll rank in both lists.
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.upsert_chunk(&dummy("parquet_loader", "loads parquet files from disk"))
            .unwrap();
        s.upsert_chunk(&dummy("file_writer", "writes csv files")).unwrap();
        s.upsert_chunk(&dummy("misc_helper", "general utility")).unwrap();
        let m = MockEmbedder::default();
        embed_pending(&mut s, &m, 8).unwrap();

        let hits = recall_with_mode(&s, &m, "parquet", 3, true, RecallMode::Hybrid)
            .unwrap();
        let top = hits[0].chunk.as_ref().unwrap();
        assert_eq!(top.name, "parquet_loader", "double-source hit ranks first");
    }

    #[test]
    fn rrf_smoothing_handles_disjoint_lists() {
        // Edge case: semantic returns A, B; lexical returns C, D. No overlap.
        // RRF should still produce a sensible merged ranking.
        let semantic = vec![(ChunkId(1), 0.9), (ChunkId(2), 0.8)];
        let lexical = vec![(ChunkId(3), 1.5), (ChunkId(4), 1.2)];
        let merged = fuse_rrf(&semantic, &lexical, 10);
        assert_eq!(merged.len(), 4);
        // ChunkId(1) and ChunkId(3) both at rank 1 → equal RRF score.
        assert_eq!(merged[0].1, merged[1].1);
        // ChunkId(2) and ChunkId(4) both at rank 2 → equal, lower than rank-1.
        assert!(merged[0].1 > merged[2].1);
    }

    #[test]
    fn recall_with_no_indexed_chunks_returns_empty() {
        let s = SqliteStore::open_in_memory().unwrap();
        let m = MockEmbedder::default();
        let hits = recall(&s, &m, "anything", 10, true).unwrap();
        assert!(hits.is_empty());
    }

    // ── test-tuning suite ──────────────────────────────────────────

    fn test_chunk(name: &str, file: &str, kind: ChunkKind) -> Chunk {
        Chunk {
            id: ChunkId(0),
            file: file.into(),
            lines: 1..2,
            kind,
            name: name.into(),
            signature_hash: 0,
            text: format!("body of {name}"),
        }
    }

    #[test]
    fn looks_like_test_classifies_via_path() {
        let c = test_chunk("foo", "src/lib/tests/it.rs", ChunkKind::Function);
        assert!(looks_like_test(&c));
        let c = test_chunk("foo", "tests/integration.rs", ChunkKind::Function);
        assert!(looks_like_test(&c));
        let c = test_chunk("foo", "src/foo_test.rs", ChunkKind::Function);
        assert!(looks_like_test(&c));
        let c = test_chunk("foo", "src/foo.test.ts", ChunkKind::Function);
        assert!(looks_like_test(&c));
    }

    #[test]
    fn looks_like_test_classifies_via_name_prefix() {
        let c = test_chunk("test_connection", "src/auth.rs", ChunkKind::Function);
        assert!(looks_like_test(&c));
        let c = test_chunk("should_work", "src/auth.rs", ChunkKind::Function);
        assert!(looks_like_test(&c));
    }

    #[test]
    fn looks_like_test_returns_false_for_real_source() {
        let c = test_chunk("FixedBuffer", "src/buffer.rs", ChunkKind::Class);
        assert!(!looks_like_test(&c));
        let c = test_chunk("compute_metrics", "src/metrics.rs", ChunkKind::Function);
        assert!(!looks_like_test(&c));
    }

    #[test]
    fn test_penalty_demotes_tests_below_equivalent_real_source() {
        // Construct: one test chunk and one real-source chunk that, with
        // mock embedding, end up at similar RRF scores. With penalty=0.5
        // the test should rank lower.
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.upsert_chunk(&test_chunk(
            "compute_value",
            "src/lib.rs",
            ChunkKind::Function,
        ))
        .unwrap();
        s.upsert_chunk(&test_chunk(
            "test_compute_value",
            "tests/lib_test.rs",
            ChunkKind::Function,
        ))
        .unwrap();
        let m = MockEmbedder::default();
        embed_pending(&mut s, &m, 8).unwrap();

        let with_penalty = recall_tuned(
            &s,
            &m,
            "compute_value",
            5,
            true,
            RecallMode::Hybrid,
            RecallTuning::default(),
        )
        .unwrap();
        assert_eq!(with_penalty.len(), 2);
        assert_eq!(
            with_penalty[0].chunk.as_ref().unwrap().name,
            "compute_value",
            "real source ranks above test under default penalty"
        );

        let no_penalty = recall_tuned(
            &s,
            &m,
            "compute_value",
            5,
            true,
            RecallMode::Hybrid,
            RecallTuning {
                test_penalty: 1.0,
                exclude_tests: false,
            },
        )
        .unwrap();
        // With penalty disabled, both still appear; ordering depends on
        // mock cosine + BM25 scores (unstable across our mock). We only
        // assert both are present.
        assert_eq!(no_penalty.len(), 2);
    }

    #[test]
    fn exclude_tests_drops_them_entirely() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.upsert_chunk(&test_chunk(
            "compute_value",
            "src/lib.rs",
            ChunkKind::Function,
        ))
        .unwrap();
        s.upsert_chunk(&test_chunk(
            "test_compute_value",
            "tests/lib_test.rs",
            ChunkKind::Function,
        ))
        .unwrap();
        let m = MockEmbedder::default();
        embed_pending(&mut s, &m, 8).unwrap();

        let hits = recall_tuned(
            &s,
            &m,
            "compute_value",
            5,
            true,
            RecallMode::Hybrid,
            RecallTuning {
                test_penalty: DEFAULT_TEST_PENALTY,
                exclude_tests: true,
            },
        )
        .unwrap();
        assert_eq!(hits.len(), 1, "test chunk should be filtered out");
        assert_eq!(hits[0].chunk.as_ref().unwrap().name, "compute_value");
    }
}
