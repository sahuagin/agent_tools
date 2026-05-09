//! Embedder trait + OpenRouter-backed implementation.
//!
//! ⚠ Duplicated structurally from `agent_tools/agent/src/embed.rs`. The
//! shared shape (Embedder trait, MockEmbedder for tests, OpenRouter HTTP
//! client with env-key + config.toml fallback) is intentional —
//! eventually both crates should depend on a single `agent_tools/embed`
//! workspace crate. Deferred until it stops being one-of-two consumers
//! and becomes one-of-N. Until then: keep the surfaces drift-compatible.

use std::path::PathBuf;

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Chunk, ChunkId, Store};

// ──────────────────────────────────────────────────────────────────────────
// Trait + adapters
// ──────────────────────────────────────────────────────────────────────────

pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

/// Build the right embedder for the current env. Returns the deterministic
/// `MockEmbedder` if no OpenRouter key is reachable — this lets `ingest`
/// run without network access during tests, demos, and offline iteration.
pub fn select_embedder() -> Box<dyn Embedder> {
    if let Some(e) = OpenRouterEmbedder::from_env() {
        Box::new(e)
    } else {
        Box::new(MockEmbedder::default())
    }
}

// ──────────────────────────────────────────────────────────────────────────
// Mock embedder (deterministic; used in tests and as offline fallback)
// ──────────────────────────────────────────────────────────────────────────

pub struct MockEmbedder {
    dims: usize,
}

impl MockEmbedder {
    pub fn new(dims: usize) -> Self {
        Self { dims }
    }
}

impl Default for MockEmbedder {
    fn default() -> Self {
        Self::new(128)
    }
}

impl Embedder for MockEmbedder {
    fn model_id(&self) -> &str {
        "mock-sha256-128"
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // Deterministic per text: hash the text, expand into `dims` floats
        // by repeating the digest. Identical inputs give identical vectors;
        // different inputs give *unrelated* vectors. Not semantically
        // useful, but exercises every pathway.
        let mut out = Vec::with_capacity(texts.len());
        for t in texts {
            let digest = Sha256::digest(t.as_bytes());
            let mut v = Vec::with_capacity(self.dims);
            for i in 0..self.dims {
                let b = digest[i % digest.len()];
                // Map byte to f32 in roughly [-1, 1] so cosine math doesn't
                // saturate. (b as f32 - 127.5) / 127.5
                v.push((b as f32 - 127.5) / 127.5);
            }
            out.push(v);
        }
        Ok(out)
    }
}

// ──────────────────────────────────────────────────────────────────────────
// OpenRouter embedder
// ──────────────────────────────────────────────────────────────────────────

pub struct OpenRouterEmbedder {
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenRouterEmbedder {
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(read_openrouter_key_from_config_toml)?;
        let model = std::env::var("CODE_INDEX_EMBED_MODEL")
            .or_else(|_| std::env::var("AGENT_EMBED_MODEL"))
            .unwrap_or_else(|_| "qwen/qwen3-embedding-8b".to_string());
        let base_url = std::env::var("CODE_INDEX_EMBED_BASE_URL")
            .or_else(|_| std::env::var("AGENT_EMBED_BASE_URL"))
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
        Some(Self {
            api_key,
            model,
            base_url,
        })
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingDatum>,
}

#[derive(Deserialize)]
struct EmbeddingDatum {
    embedding: Vec<f32>,
}

impl Embedder for OpenRouterEmbedder {
    fn model_id(&self) -> &str {
        &self.model
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // First attempt with the captured api_key. On 401, retry once
        // with the fresh value from config.toml — long-running parent
        // processes cache stale env keys after rotation; reading the
        // toml as a fallback lets a key rotation take effect without
        // restarting every consumer.
        match self.embed_with_key(texts, &self.api_key) {
            Ok(v) => Ok(v),
            Err(e) => {
                let msg = format!("{e}");
                if msg.contains("HTTP 401") {
                    if let Some(fallback) = read_openrouter_key_from_config_toml() {
                        if fallback != self.api_key {
                            eprintln!(
                                "info: embedding got 401 with env key; \
                                 retrying with config.toml fallback key"
                            );
                            return self.embed_with_key(texts, &fallback);
                        }
                    }
                }
                Err(e)
            }
        }
    }
}

impl OpenRouterEmbedder {
    fn embed_with_key(
        &self,
        texts: &[String],
        key: &str,
    ) -> Result<Vec<Vec<f32>>> {
        let url = format!("{}/embeddings", self.base_url);
        let body = EmbeddingRequest {
            model: &self.model,
            input: texts,
        };
        let json = serde_json::to_value(&body)?;
        let resp = ureq::post(&url)
            .set("Authorization", &format!("Bearer {key}"))
            .set("Content-Type", "application/json")
            .send_json(json);
        match resp {
            Ok(r) => {
                let parsed: EmbeddingResponse = r
                    .into_json()
                    .map_err(|e| anyhow!("parsing embedding response: {e}"))?;
                Ok(parsed.data.into_iter().map(|d| d.embedding).collect())
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                Err(anyhow!("OpenRouter HTTP {code}: {body}"))
            }
            Err(e) => Err(anyhow!("OpenRouter request error: {e}")),
        }
    }
}

/// Hand-rolled `[openrouter].api_key` parser over `~/.config/agent/config.toml`.
/// Mirrors agent::embed's helper of the same name. Returns `None` on any
/// failure (file missing, key missing, permissions). No `toml` crate dep.
fn read_openrouter_key_from_config_toml() -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path: PathBuf = format!("{}/.config/agent/config.toml", home).into();
    let content = std::fs::read_to_string(&path).ok()?;

    let mut in_openrouter = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_openrouter = line == "[openrouter]";
            continue;
        }
        if !in_openrouter {
            continue;
        }
        let (key, value) = match line.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        if key.trim() != "api_key" {
            continue;
        }
        let value = value.split('#').next().unwrap_or("").trim();
        let stripped = value
            .trim_start_matches(['"', '\''])
            .trim_end_matches(['"', '\'']);
        if stripped.is_empty() {
            return None;
        }
        return Some(stripped.to_string());
    }
    None
}

// ──────────────────────────────────────────────────────────────────────────
// Two-pass embedding entry point
// ──────────────────────────────────────────────────────────────────────────

/// Find every chunk in `store` that lacks an embedding for the
/// `embedder`'s model, batch them, embed, persist. Returns the count
/// embedded.
///
/// Two-pass design (ingest first → embed second) keeps storage and
/// HTTP concerns separated and avoids a borrow-checker tangle when
/// the same store needs to be passed to both ingest and the sink.
/// Cheap to re-run: the `NOT EXISTS` query in
/// `Store::chunks_missing_embedding` returns nothing when everything
/// is already embedded.
pub fn embed_pending(
    store: &mut dyn Store,
    embedder: &dyn Embedder,
    batch_size: usize,
) -> Result<usize> {
    let batch_size = batch_size.max(1);
    let model = embedder.model_id().to_string();
    let pending = store.chunks_missing_embedding(&model)?;
    let total = pending.len();
    for batch in pending.chunks(batch_size) {
        let texts: Vec<String> = batch
            .iter()
            .map(|c| format!("{}\n{}", c.name, c.text))
            .collect();
        let vectors = embedder.embed(&texts)?;
        if vectors.len() != batch.len() {
            return Err(anyhow!(
                "embedder returned {} vectors for {} inputs",
                vectors.len(),
                batch.len()
            ));
        }
        for (chunk, vec) in batch.iter().zip(vectors.iter()) {
            store.upsert_embedding(chunk.id, &model, vec)?;
        }
    }
    Ok(total)
}

// ──────────────────────────────────────────────────────────────────────────
// Batched sink — buffers chunks, flushes via embedder + persists to store
// ──────────────────────────────────────────────────────────────────────────

const DEFAULT_BATCH_SIZE: usize = 32;

/// Buffers chunks, calls `Embedder::embed` in batches, persists each
/// returned vector to the store. Caller is responsible for invoking
/// `flush()` (or letting `Drop` handle it best-effort) after ingest.
///
/// The text used for embedding is `format!("{name}\n{text}", ...)` —
/// names carry strong identifier signal that bare bodies miss when the
/// body is short or unique-name-light (e.g. `fn run(&self) {}`).
pub struct BatchedEmbedSink<'a> {
    embedder: &'a dyn Embedder,
    store: &'a mut dyn Store,
    pending_ids: Vec<ChunkId>,
    pending_texts: Vec<String>,
    batch_size: usize,
    pub flushed: usize,
}

impl<'a> BatchedEmbedSink<'a> {
    pub fn new(embedder: &'a dyn Embedder, store: &'a mut dyn Store) -> Self {
        Self {
            embedder,
            store,
            pending_ids: Vec::with_capacity(DEFAULT_BATCH_SIZE),
            pending_texts: Vec::with_capacity(DEFAULT_BATCH_SIZE),
            batch_size: DEFAULT_BATCH_SIZE,
            flushed: 0,
        }
    }

    pub fn with_batch_size(mut self, size: usize) -> Self {
        self.batch_size = size.max(1);
        self
    }

    pub fn enqueue(&mut self, id: ChunkId, chunk: &Chunk) -> Result<()> {
        let text = format!("{}\n{}", chunk.name, chunk.text);
        self.pending_ids.push(id);
        self.pending_texts.push(text);
        if self.pending_ids.len() >= self.batch_size {
            self.flush()?;
        }
        Ok(())
    }

    pub fn flush(&mut self) -> Result<()> {
        if self.pending_ids.is_empty() {
            return Ok(());
        }
        let vectors = self.embedder.embed(&self.pending_texts)?;
        if vectors.len() != self.pending_ids.len() {
            return Err(anyhow!(
                "embedder returned {} vectors for {} inputs",
                vectors.len(),
                self.pending_ids.len()
            ));
        }
        let model = self.embedder.model_id().to_string();
        for (id, vec) in self.pending_ids.iter().zip(vectors.iter()) {
            self.store.upsert_embedding(*id, &model, vec)?;
        }
        self.flushed += self.pending_ids.len();
        self.pending_ids.clear();
        self.pending_texts.clear();
        Ok(())
    }
}

impl Drop for BatchedEmbedSink<'_> {
    fn drop(&mut self) {
        // Best-effort drain on drop — swallow errors because Drop can't
        // fail. Real callers should call `flush()` explicitly so they
        // can act on errors; this guard catches the case where the
        // ingest path errored before the explicit flush.
        if !self.pending_ids.is_empty() {
            let _ = self.flush();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::SqliteStore;
    use crate::{Chunk, ChunkKind};

    fn dummy_chunk(name: &str, text: &str) -> Chunk {
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
    fn mock_embedder_is_deterministic() {
        let m = MockEmbedder::new(64);
        let v1 = m.embed(&["hello".into()]).unwrap();
        let v2 = m.embed(&["hello".into()]).unwrap();
        assert_eq!(v1, v2);
        let v3 = m.embed(&["world".into()]).unwrap();
        assert_ne!(v1, v3);
        assert_eq!(v1[0].len(), 64);
    }

    #[test]
    fn batched_sink_flushes_at_batch_size_and_on_explicit_flush() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        // Pre-create chunks so foreign keys are satisfied.
        let mut ids = Vec::new();
        for i in 0..5 {
            let c = dummy_chunk(&format!("c{i}"), "body");
            ids.push(s.upsert_chunk(&c).unwrap());
        }

        let m = MockEmbedder::default();
        {
            let mut sink = BatchedEmbedSink::new(&m, &mut s).with_batch_size(2);
            for (i, id) in ids.iter().enumerate() {
                let c = dummy_chunk(&format!("c{i}"), "body");
                sink.enqueue(*id, &c).unwrap();
            }
            // After enqueue of 5 with batch=2: 2 batches flushed (4 chunks),
            // 1 still pending. Explicit flush drains the last.
            assert_eq!(sink.flushed, 4);
            sink.flush().unwrap();
            assert_eq!(sink.flushed, 5);
        }

        // recall returns rows for each chunk we embedded (mock is
        // deterministic-but-meaningless, so we don't care about ordering).
        let any = s
            .recall_top_k("mock-sha256-128", &vec![0.0; 128], 10)
            .unwrap();
        assert_eq!(any.len(), 5);
    }

    #[test]
    fn drop_flushes_pending_on_scope_exit() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        let id = s
            .upsert_chunk(&dummy_chunk("only", "body"))
            .unwrap();
        let m = MockEmbedder::default();
        {
            let mut sink = BatchedEmbedSink::new(&m, &mut s).with_batch_size(100);
            // batch_size=100; one chunk is well under the threshold, so it
            // sits in pending until Drop fires.
            sink.enqueue(id, &dummy_chunk("only", "body")).unwrap();
            assert_eq!(sink.flushed, 0);
        } // drop here
        let recall = s
            .recall_top_k("mock-sha256-128", &vec![0.0; 128], 5)
            .unwrap();
        assert_eq!(recall.len(), 1, "Drop should have flushed pending");
    }

    #[test]
    fn flush_on_empty_is_noop() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        let m = MockEmbedder::default();
        let mut sink = BatchedEmbedSink::new(&m, &mut s);
        sink.flush().unwrap();
        assert_eq!(sink.flushed, 0);
    }
}
