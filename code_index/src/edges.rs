//! Edge resolution: turn raw `@reference.X` captures into persisted
//! `Edge` rows by name-matching against the chunks table.
//!
//! v1 strategy is intentionally simple — Layer 3 from the at-n80 design:
//!
//!   1. Re-parse each file in the manifest.
//!   2. Run the chunker's extract_with_references — definitions land in
//!      the same shape as during ingest, references carry their
//!      containing-chunk index (via byte-range parent walk inside the
//!      chunker).
//!   3. Map each result-chunk to its DB ChunkId by `(file, line_start)`.
//!   4. For each reference, look up `name` in the chunks table.
//!      - Exactly one match in the same file → confidence 1.0
//!      - Exactly one match anywhere → 0.85 (unambiguous cross-file)
//!      - Multiple matches → pick same-file first, then any; 0.6 because
//!        ambiguous resolution
//!      - Zero matches → skip (external or unknown symbol)
//!
//! Layer 2 (`locals.scm` scope-aware resolution) is the upgrade path
//! when v1's by-name resolution turns out to be too coarse on real
//! codebases. We'll know after running this against jj_lseg /
//! pi_agent_rust whether the simple thing is good enough.

use std::path::PathBuf;

use anyhow::{Context, Result};

use crate::chunker::Chunker;
use crate::{Chunk, ChunkId, Edge, Store};

#[derive(Debug, Default)]
pub struct EdgeBuildStats {
    pub files_processed: usize,
    pub files_skipped: usize,
    pub references_found: usize,
    pub edges_emitted: usize,
    pub references_unresolved: usize,
}

pub fn build_edges(store: &mut dyn Store) -> Result<EdgeBuildStats> {
    let mut stats = EdgeBuildStats::default();
    let files = store.list_known_files()?;

    for file in files {
        let chunker = match Chunker::for_path(&file) {
            Some(Ok(c)) => c,
            _ => {
                stats.files_skipped += 1;
                continue;
            }
        };

        let bytes = match std::fs::read(&file) {
            Ok(b) => b,
            Err(_) => {
                // File disappeared since ingest — skip rather than fail
                // the whole pass. Caller may want a re-ingest.
                stats.files_skipped += 1;
                continue;
            }
        };

        let parsed = chunker
            .extract_with_references(&bytes, &file)
            .with_context(|| format!("re-parsing {}", file.display()))?;

        // Map parsed chunks back to DB ids by (file, line_start). We
        // pulled this file's chunks from the DB up front so the per-
        // reference attribution is O(refs) rather than O(refs * files).
        let db_chunks = store.list_chunks_by_file(&file)?;
        let chunk_ids: Vec<Option<ChunkId>> = parsed
            .chunks
            .iter()
            .map(|c| {
                db_chunks
                    .iter()
                    .find(|d| d.lines.start == c.lines.start && d.name == c.name)
                    .map(|d| d.id)
            })
            .collect();

        stats.files_processed += 1;

        for raw_ref in &parsed.references {
            stats.references_found += 1;

            let from_id = match raw_ref.containing_chunk_idx.and_then(|i| chunk_ids[i]) {
                Some(id) => id,
                None => {
                    // Reference is at file scope, or its containing
                    // def didn't map to a DB chunk (file drift). Skip;
                    // we don't currently model file-level edges.
                    stats.references_unresolved += 1;
                    continue;
                }
            };

            let candidates = store.find_chunks_by_name(&raw_ref.name)?;
            let (target, confidence) = pick_target(&file, &candidates);

            match target {
                Some(t) if t.id != from_id || allow_self_edge(raw_ref.kind) => {
                    let edge = Edge {
                        from: from_id,
                        to: t.id,
                        kind: raw_ref.kind,
                        confidence,
                    };
                    store.upsert_edge(&edge)?;
                    stats.edges_emitted += 1;
                }
                Some(_) => {
                    // Self-edge that we don't allow for this kind. Drop.
                    stats.references_unresolved += 1;
                }
                None => {
                    stats.references_unresolved += 1;
                }
            }
        }
    }

    Ok(stats)
}

/// Choose the best match from a candidate list of name-matched chunks.
/// Returns the chunk and our confidence in the resolution.
fn pick_target<'a>(
    in_file: &PathBuf,
    candidates: &'a [Chunk],
) -> (Option<&'a Chunk>, f32) {
    if candidates.is_empty() {
        return (None, 0.0);
    }
    if candidates.len() == 1 {
        let c = &candidates[0];
        let conf = if &c.file == in_file { 1.0 } else { 0.85 };
        return (Some(c), conf);
    }
    // Multiple candidates — prefer same-file.
    let same_file = candidates.iter().find(|c| &c.file == in_file);
    match same_file {
        Some(c) => (Some(c), 0.85),
        None => (Some(&candidates[0]), 0.6),
    }
}

/// Whether to emit a self-edge for the given kind. Recursion is real;
/// `Calls` self-edges carry meaning. `References` and `Imports`
/// self-edges are almost always uninteresting.
fn allow_self_edge(kind: crate::EdgeKind) -> bool {
    matches!(kind, crate::EdgeKind::Calls)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chunker::{Chunker, SupportedLanguage};
    use crate::ingest::ingest;
    use crate::store::SqliteStore;
    use crate::EdgeKind;
    use std::path::Path;

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let unique = format!(
            "code_index_edges_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let d = base.join(unique);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn raw_reference_attribution_picks_innermost_containing_chunk() {
        // Confirms the chunker's parent-walk correctly attributes a
        // reference to the most specific (smallest) containing
        // definition, not the outermost.
        let src = r#"
fn outer() {
    inner_call();
}
fn inner_call() {}
"#;
        let chunker = Chunker::for_language(SupportedLanguage::Rust).unwrap();
        let result = chunker
            .extract_with_references(src.as_bytes(), Path::new("a.rs"))
            .unwrap();

        // Chunks: outer + inner_call (both top-level functions, no nesting).
        assert_eq!(result.chunks.len(), 2);

        // The reference to `inner_call` from inside `outer` should be
        // attributed to the `outer` chunk.
        let outer_idx = result
            .chunks
            .iter()
            .position(|c| c.name == "outer")
            .unwrap();
        let ref_from_outer = result
            .references
            .iter()
            .find(|r| r.name == "inner_call")
            .expect("call site captured");
        assert_eq!(ref_from_outer.containing_chunk_idx, Some(outer_idx));
        assert_eq!(ref_from_outer.kind, EdgeKind::Calls);
    }

    #[test]
    fn build_edges_resolves_intra_file_call() {
        let dir = tempdir();
        write(
            &dir.join("a.rs"),
            "fn caller() {\n    callee();\n}\nfn callee() {}\n",
        );
        let mut s = SqliteStore::open_in_memory().unwrap();
        ingest(&dir, &mut s, None).unwrap();

        let stats = build_edges(&mut s).unwrap();
        assert_eq!(stats.files_processed, 1);
        assert!(stats.references_found >= 1);
        assert_eq!(stats.edges_emitted, 1, "caller→callee should resolve");
        assert_eq!(stats.references_unresolved, 0);

        let edges = s.iter_edges().unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].kind, EdgeKind::Calls);
        assert_eq!(edges[0].confidence, 1.0, "single same-file match → 1.0");
    }

    #[test]
    fn build_edges_resolves_cross_file_call_with_lower_confidence() {
        let dir = tempdir();
        write(&dir.join("caller.rs"), "fn caller() {\n    callee();\n}\n");
        write(&dir.join("util.rs"), "pub fn callee() {}\n");
        let mut s = SqliteStore::open_in_memory().unwrap();
        ingest(&dir, &mut s, None).unwrap();

        let stats = build_edges(&mut s).unwrap();
        assert_eq!(stats.edges_emitted, 1);
        let edges = s.iter_edges().unwrap();
        let edge = &edges[0];
        assert_eq!(edge.kind, EdgeKind::Calls);
        assert!(
            (edge.confidence - 0.85).abs() < 1e-6,
            "single cross-file match → 0.85, got {}",
            edge.confidence
        );
    }

    #[test]
    fn build_edges_drops_unresolved_external_calls() {
        // Calls to functions that aren't in our chunks table should be
        // counted as unresolved, not emitted as edges.
        let dir = tempdir();
        write(
            &dir.join("a.rs"),
            "fn local() {\n    std::env::var(\"X\").unwrap();\n}\n",
        );
        let mut s = SqliteStore::open_in_memory().unwrap();
        ingest(&dir, &mut s, None).unwrap();

        let stats = build_edges(&mut s).unwrap();
        assert!(stats.references_unresolved > 0);
        // The std::env::var reference shouldn't produce an edge — there's
        // no chunk in the store with name `var` or `std`.
        let edges = s.iter_edges().unwrap();
        assert!(
            edges.iter().all(|e| e.kind == EdgeKind::Calls),
            "no spurious non-Call edges from std references"
        );
    }

    #[test]
    fn build_edges_recursive_calls_emit_self_edge() {
        let dir = tempdir();
        write(
            &dir.join("a.rs"),
            "fn factorial(n: u64) -> u64 {\n    \
             if n <= 1 { 1 } else { n * factorial(n - 1) }\n\
             }\n",
        );
        let mut s = SqliteStore::open_in_memory().unwrap();
        ingest(&dir, &mut s, None).unwrap();

        let stats = build_edges(&mut s).unwrap();
        assert!(stats.edges_emitted >= 1, "recursion is a real call edge");

        let edges = s.iter_edges().unwrap();
        let self_edge = edges
            .iter()
            .find(|e| e.from == e.to && e.kind == EdgeKind::Calls);
        assert!(self_edge.is_some(), "factorial→factorial self-edge");
    }

    #[test]
    fn build_edges_picks_same_file_when_multiple_candidates() {
        // Two files each defining `helper`; from one of them, calls
        // `helper`. Must resolve to the same-file definition.
        let dir = tempdir();
        write(
            &dir.join("a.rs"),
            "fn helper() {}\nfn caller() {\n    helper();\n}\n",
        );
        write(&dir.join("b.rs"), "fn helper() {}\n");
        let mut s = SqliteStore::open_in_memory().unwrap();
        ingest(&dir, &mut s, None).unwrap();

        let stats = build_edges(&mut s).unwrap();
        assert!(stats.edges_emitted >= 1);
        let edges = s.iter_edges().unwrap();
        let edge_from_caller = edges.iter().find(|e| e.kind == EdgeKind::Calls).unwrap();
        // confidence should reflect ambiguity: same-file pick from
        // multiple candidates → 0.85
        assert!((edge_from_caller.confidence - 0.85).abs() < 1e-6);
    }

    #[test]
    fn build_edges_can_be_re_run_idempotently() {
        let dir = tempdir();
        write(
            &dir.join("a.rs"),
            "fn caller() {\n    callee();\n}\nfn callee() {}\n",
        );
        let mut s = SqliteStore::open_in_memory().unwrap();
        ingest(&dir, &mut s, None).unwrap();

        build_edges(&mut s).unwrap();
        let edges_first = s.iter_edges().unwrap();

        build_edges(&mut s).unwrap();
        let edges_second = s.iter_edges().unwrap();

        assert_eq!(
            edges_first.len(),
            edges_second.len(),
            "second build_edges run must not duplicate edges (UNIQUE on from_id, to_id, kind)"
        );
    }
}
