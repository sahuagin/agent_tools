//! `recall` orchestration: embed the query, ask the store for top-K
//! nearest chunks, optionally materialize them.

use anyhow::{Context, Result};

use crate::embed::Embedder;
use crate::{Chunk, ChunkId, Store};

/// One result row. `score` is cosine similarity in [-1.0, 1.0]; closer
/// to 1.0 is more relevant. `chunk` is `Some` only if the caller asked
/// for materialized results — recall_top_k itself returns ids+scores
/// (cheap), and we look up the chunk only on request.
#[derive(Debug, Clone)]
pub struct Hit {
    pub id: ChunkId,
    pub score: f32,
    pub chunk: Option<Chunk>,
}

/// Embed `query` via `embedder`, ask `store` for top-K matches against
/// the embedder's model, and (if `materialize`) load the full chunks.
pub fn recall(
    store: &dyn Store,
    embedder: &dyn Embedder,
    query: &str,
    k: usize,
    materialize: bool,
) -> Result<Vec<Hit>> {
    let qvecs = embedder
        .embed(&[query.to_string()])
        .with_context(|| "embedding query")?;
    let qvec = qvecs
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("embedder returned no vectors"))?;
    let scored = store
        .recall_top_k(embedder.model_id(), &qvec, k)
        .with_context(|| "recall_top_k")?;

    let mut hits = Vec::with_capacity(scored.len());
    for (id, score) in scored {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embed::{BatchedEmbedSink, MockEmbedder};
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

    #[test]
    fn recall_returns_hits_for_indexed_chunks() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        let id_a = s.upsert_chunk(&dummy("alpha", "the alpha body")).unwrap();
        let id_b = s.upsert_chunk(&dummy("beta", "the beta body")).unwrap();

        let m = MockEmbedder::default();
        {
            let mut sink = BatchedEmbedSink::new(&m, &mut s);
            sink.enqueue(id_a, &dummy("alpha", "the alpha body"))
                .unwrap();
            sink.enqueue(id_b, &dummy("beta", "the beta body")).unwrap();
            sink.flush().unwrap();
        }

        // Query for "alpha"; with deterministic-but-meaningless mock
        // vectors, we expect both ids to come back, ordered by cosine.
        let hits = recall(&s, &m, "alpha", 5, false).unwrap();
        assert_eq!(hits.len(), 2);
        assert!(hits[0].score >= hits[1].score, "scores must descend");
        assert!(hits.iter().all(|h| h.chunk.is_none()), "no materialize");
    }

    #[test]
    fn materialize_loads_chunk_bodies() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        let id = s.upsert_chunk(&dummy("x", "body of x")).unwrap();
        let m = MockEmbedder::default();
        {
            let mut sink = BatchedEmbedSink::new(&m, &mut s);
            sink.enqueue(id, &dummy("x", "body of x")).unwrap();
            sink.flush().unwrap();
        }

        let hits = recall(&s, &m, "x", 1, true).unwrap();
        assert_eq!(hits.len(), 1);
        let chunk = hits[0]
            .chunk
            .as_ref()
            .expect("materialize=true should populate chunk");
        assert_eq!(chunk.name, "x");
        assert!(chunk.text.contains("body of x"));
    }

    #[test]
    fn recall_with_no_indexed_chunks_returns_empty() {
        let s = SqliteStore::open_in_memory().unwrap();
        let m = MockEmbedder::default();
        let hits = recall(&s, &m, "anything", 10, true).unwrap();
        assert!(hits.is_empty());
    }
}
