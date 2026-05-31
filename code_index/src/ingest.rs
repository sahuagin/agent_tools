//! `ingest` orchestration: walk a path, chunk each supported file via the
//! tag-aware chunker, and persist chunks to a `Store`.
//!
//! Embedding is a separate concern (see `embed.rs`) and is wired in via
//! an optional callback so this orchestration stays storage-and-AST-only.
//! Per-file work proceeds in this order:
//!
//!   1. Hash the file's bytes (FNV-1a, same primitive used for chunk
//!      signatures). Look up the stored `file_signature`.
//!   2. If unchanged: skip the file entirely. The chunker is the
//!      expensive part for large files; manifest comparison costs nothing.
//!   3. If changed (or new): delete prior chunks for the file (cascades
//!      to their edges and embeddings via FK ON DELETE CASCADE), chunk
//!      the new content, upsert each chunk, optionally embed via callback,
//!      and update the manifest signature.
//!
//! This makes `ingest` re-runnable cheaply — a second run with no source
//! changes is a manifest scan and nothing else.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::chunker::{fnv1a_64, Chunker, SupportedLanguage};
use crate::walker::walk_sources_with;
use crate::{Chunk, ChunkId, Store};

/// Outcome of an ingest run, surfaced for logging or programmatic use.
#[derive(Debug, Default)]
pub struct IngestStats {
    pub files_walked: usize,
    pub files_unchanged: usize,
    pub files_chunked: usize,
    pub chunks_upserted: usize,
}

/// Optional callback invoked once per upserted chunk. Intended for the
/// embedder integration: the caller hands ingest a closure that takes a
/// `(ChunkId, &Chunk)` and persists or batches an embedding for it.
///
/// Returning `Err` aborts the whole ingest with that error. For
/// best-effort embedding the closure can swallow its own errors and
/// always return `Ok(())`.
pub type EmbedCallback<'a> = dyn FnMut(ChunkId, &Chunk) -> Result<()> + 'a;

/// Walk `root`, chunk per-file, persist to `store`. Calls `on_chunk` for
/// each upserted chunk if provided. Honors `.gitignore` etc. by default.
pub fn ingest(
    root: &Path,
    store: &mut dyn Store,
    on_chunk: Option<&mut EmbedCallback<'_>>,
) -> Result<IngestStats> {
    ingest_with(root, store, on_chunk, true)
}

/// Like `ingest`, but lets the caller disable VCS-ignore honoring.
pub fn ingest_with(
    root: &Path,
    store: &mut dyn Store,
    on_chunk: Option<&mut EmbedCallback<'_>>,
    respect_gitignore: bool,
) -> Result<IngestStats> {
    let mut stats = IngestStats::default();
    let files = walk_sources_with(root, respect_gitignore)
        .with_context(|| format!("walking {}", root.display()))?;
    stats.files_walked = files.len();

    // Build one chunker per language, lazily — compiling the tags.scm
    // query is the expensive bootstrap and we only want to pay it once
    // per language across the whole walk.
    let mut chunkers: std::collections::HashMap<SupportedLanguage, Chunker> =
        std::collections::HashMap::new();

    let mut on_chunk = on_chunk;
    for path in files {
        let result = ingest_one(&path, store, &mut chunkers, on_chunk.as_deref_mut())?;
        match result {
            FileOutcome::Unchanged => stats.files_unchanged += 1,
            FileOutcome::Chunked(n) => {
                stats.files_chunked += 1;
                stats.chunks_upserted += n;
            }
        }
    }
    Ok(stats)
}

enum FileOutcome {
    Unchanged,
    Chunked(usize),
}

fn ingest_one(
    path: &PathBuf,
    store: &mut dyn Store,
    chunkers: &mut std::collections::HashMap<SupportedLanguage, Chunker>,
    mut on_chunk: Option<&mut EmbedCallback<'_>>,
) -> Result<FileOutcome> {
    let Some(language) = SupportedLanguage::from_path(path) else {
        return Ok(FileOutcome::Unchanged);
    };

    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let signature = fnv1a_64(&bytes);

    if let Some(prior) = store.file_signature(path)? {
        if prior == signature {
            return Ok(FileOutcome::Unchanged);
        }
    }

    // Re-chunk: drop the prior chunks (cascades to edges + embeddings).
    for c in store.list_chunks_by_file(path)? {
        store.delete_chunk(c.id)?;
    }

    let chunker = match chunkers.entry(language) {
        std::collections::hash_map::Entry::Occupied(o) => o.into_mut(),
        std::collections::hash_map::Entry::Vacant(v) => {
            let c = Chunker::for_language(language)
                .with_context(|| format!("constructing chunker for {language:?}"))?;
            v.insert(c)
        }
    };

    let chunks = chunker
        .extract(&bytes, path)
        .with_context(|| format!("chunking {}", path.display()))?;
    let n = chunks.len();
    for chunk in chunks {
        let id = store.upsert_chunk(&chunk)?;
        if let Some(cb) = on_chunk.as_deref_mut() {
            // Refresh the chunk with its assigned id so the callback can
            // persist embeddings keyed correctly.
            let mut with_id = chunk.clone();
            with_id.id = id;
            cb(id, &with_id)?;
        }
    }

    store.set_file_signature(path, signature)?;
    Ok(FileOutcome::Chunked(n))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    use crate::ChunkKind;

    fn write(path: &Path, content: &str) {
        if let Some(p) = path.parent() {
            std::fs::create_dir_all(p).unwrap();
        }
        std::fs::write(path, content).unwrap();
    }

    fn tempdir() -> PathBuf {
        let base = std::env::temp_dir();
        let unique = format!(
            "code_index_ingest_test_{}_{}",
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
    fn first_run_chunks_new_files() {
        let dir = tempdir();
        write(&dir.join("a.rs"), "fn one() {}\nfn two() {}\n");
        write(&dir.join("b.py"), "def foo(): pass\n");
        write(&dir.join("c.md"), "# unsupported, skipped\n");

        let mut s = SqliteStore::open_in_memory().unwrap();
        let stats = ingest(&dir, &mut s, None).unwrap();

        assert_eq!(stats.files_walked, 2, "md is filtered out by walker");
        assert_eq!(stats.files_unchanged, 0);
        assert_eq!(stats.files_chunked, 2);
        assert_eq!(stats.chunks_upserted, 3);

        let by_a = s.list_chunks_by_file(&dir.join("a.rs")).unwrap();
        assert_eq!(by_a.len(), 2);
        assert!(by_a.iter().any(|c| c.name == "one"));
    }

    #[test]
    fn second_run_with_no_changes_is_all_unchanged() {
        let dir = tempdir();
        write(&dir.join("a.rs"), "fn one() {}\n");
        let mut s = SqliteStore::open_in_memory().unwrap();

        let r1 = ingest(&dir, &mut s, None).unwrap();
        assert_eq!(r1.files_chunked, 1);

        let r2 = ingest(&dir, &mut s, None).unwrap();
        assert_eq!(r2.files_walked, 1);
        assert_eq!(r2.files_unchanged, 1);
        assert_eq!(r2.files_chunked, 0);
        assert_eq!(r2.chunks_upserted, 0);
    }

    #[test]
    fn changed_file_replaces_prior_chunks() {
        let dir = tempdir();
        let f = dir.join("a.rs");
        write(&f, "fn one() {}\nfn two() {}\n");
        let mut s = SqliteStore::open_in_memory().unwrap();

        ingest(&dir, &mut s, None).unwrap();
        let initial = s.list_chunks_by_file(&f).unwrap();
        assert_eq!(initial.len(), 2);

        // Rewrite with different content — same file, fewer chunks.
        write(&f, "fn renamed() {}\n");
        let r2 = ingest(&dir, &mut s, None).unwrap();
        assert_eq!(r2.files_unchanged, 0);
        assert_eq!(r2.files_chunked, 1);

        let after = s.list_chunks_by_file(&f).unwrap();
        assert_eq!(after.len(), 1, "old chunks should be gone");
        assert_eq!(after[0].name, "renamed");
    }

    #[test]
    fn callback_fires_with_assigned_id_per_chunk() {
        let dir = tempdir();
        write(&dir.join("a.rs"), "fn one() {}\nfn two() {}\n");
        let mut s = SqliteStore::open_in_memory().unwrap();

        let mut seen: Vec<(ChunkId, ChunkKind, String)> = Vec::new();
        {
            let mut cb = |id: ChunkId, c: &Chunk| -> anyhow::Result<()> {
                seen.push((id, c.kind, c.name.clone()));
                Ok(())
            };
            ingest(&dir, &mut s, Some(&mut cb)).unwrap();
        }

        assert_eq!(seen.len(), 2);
        for (id, _kind, _name) in &seen {
            assert!(id.0 > 0, "callback should receive a real assigned id");
        }
    }

    #[test]
    fn callback_error_aborts_ingest() {
        let dir = tempdir();
        write(&dir.join("a.rs"), "fn one() {}\n");
        let mut s = SqliteStore::open_in_memory().unwrap();

        let mut cb = |_id: ChunkId, _c: &Chunk| -> anyhow::Result<()> {
            anyhow::bail!("synthetic embedding failure")
        };
        let r = ingest(&dir, &mut s, Some(&mut cb));
        assert!(r.is_err());
    }
}
