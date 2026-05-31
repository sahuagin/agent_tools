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
    fn embed_with_key(&self, texts: &[String], key: &str) -> Result<Vec<Vec<f32>>> {
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
                // Read the body as a string first so a parse failure can
                // surface what we actually got back. OpenRouter (and some
                // upstreams it proxies) occasionally returns an error
                // envelope `{"error": {...}}` with HTTP 200, which used to
                // hit our serde deserialize as `missing field "data"` with
                // no hint at the actual cause. Now we try the happy-path
                // shape, fall back to error-envelope parsing, and as a last
                // resort include a truncated body sample in the error.
                let body_text = r
                    .into_string()
                    .map_err(|e| anyhow!("reading embedding response body: {e}"))?;
                if let Ok(parsed) = serde_json::from_str::<EmbeddingResponse>(&body_text) {
                    return Ok(parsed.data.into_iter().map(|d| d.embedding).collect());
                }
                if let Ok(envelope) = serde_json::from_str::<ErrorEnvelope>(&body_text) {
                    return Err(anyhow!(
                        "OpenRouter returned error envelope: {} (code: {})",
                        envelope.error.message,
                        envelope.error.code.unwrap_or(0),
                    ));
                }
                let preview: String = body_text.chars().take(800).collect();
                Err(anyhow!(
                    "OpenRouter response did not match expected shape \
                     (no `data` field, no error envelope). Body preview \
                     ({} bytes): {preview}",
                    body_text.len(),
                ))
            }
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                Err(anyhow!("OpenRouter HTTP {code}: {body}"))
            }
            Err(e) => Err(anyhow!("OpenRouter request error: {e}")),
        }
    }
}

#[derive(Deserialize)]
struct ErrorEnvelope {
    error: ErrorBody,
}

#[derive(Deserialize)]
struct ErrorBody {
    message: String,
    #[serde(default)]
    code: Option<i64>,
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
// Embed-input shaping
// ──────────────────────────────────────────────────────────────────────────

/// Conservative cap on the text we hand to the embedder per chunk. Most
/// embedding providers cap input around 8k tokens; 24k chars is a safe
/// upper bound at typical code/English token-density of ~3-4 chars per
/// token. Matches `agent::embed::EMBED_CHAR_LIMIT`. The full chunk body
/// is still stored — only the *embed input* is truncated.
pub const EMBED_CHAR_LIMIT: usize = 24_000;

/// Build the text that goes to the embedder for a chunk. Prefixes the
/// name (strong identifier signal that bare bodies miss when the body
/// is short) and truncates the body to `EMBED_CHAR_LIMIT` on a char
/// boundary so we never hand a partial multibyte sequence to JSON
/// serialization.
pub fn embed_input_for(chunk: &Chunk) -> String {
    let body = if chunk.text.len() > EMBED_CHAR_LIMIT {
        let mut end = EMBED_CHAR_LIMIT;
        while end > 0 && !chunk.text.is_char_boundary(end) {
            end -= 1;
        }
        &chunk.text[..end]
    } else {
        &chunk.text[..]
    };
    format!("{}\n{}", chunk.name, body)
}

// ──────────────────────────────────────────────────────────────────────────
// Two-pass embedding entry point
// ──────────────────────────────────────────────────────────────────────────

/// Find every chunk in `store` that lacks an embedding for the
/// `embedder`'s model, batch them, embed, persist. Returns the count
/// embedded.
///
/// Sequential. For I/O-bound workloads at scale, use
/// [`embed_pending_concurrent`] — it fans batches across a thread pool
/// so HTTP latency overlaps instead of stacking.
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
    let started = std::time::Instant::now();
    let mut done = 0usize;
    let mut last_logged = 0usize;
    for batch in pending.chunks(batch_size) {
        let texts: Vec<String> = batch.iter().map(embed_input_for).collect();
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
        done += batch.len();
        // Log progress at most every 100 chunks (or on the final batch),
        // so big runs are inspectable without spamming small ones.
        if done == total || done - last_logged >= 100 {
            let elapsed = started.elapsed().as_secs_f64().max(0.01);
            let rate = done as f64 / elapsed;
            let remaining = total.saturating_sub(done);
            let eta_sec = (remaining as f64 / rate.max(0.01)) as u64;
            eprintln!(
                "embed: {done}/{total} chunks ({rate:.1}/s, eta ~{}m{:02}s)",
                eta_sec / 60,
                eta_sec % 60,
            );
            last_logged = done;
        }
    }
    Ok(total)
}

/// Like `embed_pending`, but fans `concurrency` HTTP batches in flight
/// across `std::thread::scope` workers. Embedding work is I/O-bound
/// (each call blocks on socket I/O); concurrency lets latency overlap
/// instead of stacking sequentially. Persistence to the store stays
/// single-writer on the main thread — sqlite doesn't love concurrent
/// writers and the writes are local-and-fast anyway.
///
/// Batches are distributed round-robin across per-worker channels, so
/// no Mutex on a shared receiver. Workers exit when their channel
/// closes (the feeder dropping its tx). Errors from any batch abort
/// the run; in-flight batches will still complete (the scope blocks
/// until all spawned threads return) but their results are discarded.
pub fn embed_pending_concurrent(
    store: &mut dyn Store,
    embedder: &(dyn Embedder + Sync),
    batch_size: usize,
    concurrency: usize,
) -> Result<usize> {
    use std::sync::mpsc;
    use std::thread;
    use std::time::Instant;

    let batch_size = batch_size.max(1);
    let concurrency = concurrency.max(1);
    let model = embedder.model_id().to_string();
    let pending = store.chunks_missing_embedding(&model)?;
    let total = pending.len();
    if total == 0 {
        return Ok(0);
    }
    if concurrency == 1 {
        // Single-worker: no point setting up the pool; fall through.
        return embed_pending(store, embedder, batch_size);
    }

    let batches: Vec<Vec<Chunk>> = pending.chunks(batch_size).map(|c| c.to_vec()).collect();
    let batch_count = batches.len();

    type WorkerOk = (Vec<ChunkId>, Vec<Vec<f32>>);
    let (result_tx, result_rx) = mpsc::sync_channel::<Result<WorkerOk>>(concurrency * 2);

    let started = Instant::now();

    thread::scope(|s| -> Result<usize> {
        // Per-worker work channels — round-robin distribution avoids a
        // shared-receiver Mutex. Per-channel capacity of 2 keeps each
        // worker primed with one in-flight + one queued, no more.
        let mut worker_txs: Vec<mpsc::SyncSender<Vec<Chunk>>> = Vec::with_capacity(concurrency);
        for _ in 0..concurrency {
            let (tx, rx) = mpsc::sync_channel::<Vec<Chunk>>(2);
            worker_txs.push(tx);
            let result_tx = result_tx.clone();
            s.spawn(move || {
                while let Ok(batch) = rx.recv() {
                    let texts: Vec<String> = batch.iter().map(embed_input_for).collect();
                    let ids: Vec<ChunkId> = batch.iter().map(|c| c.id).collect();
                    let res = embedder.embed(&texts).map(|vecs| (ids, vecs));
                    if result_tx.send(res).is_err() {
                        return;
                    }
                }
            });
        }
        // Drop the original result_tx so the channel closes once all
        // worker clones drop.
        drop(result_tx);

        // Feeder thread: round-robin batches across workers. Closes
        // each worker's channel by dropping all worker_txs at scope exit.
        s.spawn(move || {
            let n_workers = concurrency;
            for (i, batch) in batches.into_iter().enumerate() {
                // Bounded send — blocks if a worker's queue is full,
                // which provides natural backpressure.
                if worker_txs[i % n_workers].send(batch).is_err() {
                    return;
                }
            }
            // worker_txs drops here; workers' rx loops exit.
        });

        // Main thread: drain results, persist sequentially, log progress.
        let mut done = 0usize;
        let mut last_logged = 0usize;
        for _ in 0..batch_count {
            let r = match result_rx.recv() {
                Ok(r) => r,
                Err(e) => return Err(anyhow!("result channel closed early: {e}")),
            };
            match r {
                Ok((ids, vecs)) => {
                    if vecs.len() != ids.len() {
                        return Err(anyhow!(
                            "embedder returned {} vectors for {} ids",
                            vecs.len(),
                            ids.len()
                        ));
                    }
                    for (id, vec) in ids.iter().zip(vecs.iter()) {
                        store.upsert_embedding(*id, &model, vec)?;
                    }
                    done += ids.len();
                    if done == total || done - last_logged >= 100 {
                        let elapsed = started.elapsed().as_secs_f64().max(0.01);
                        let rate = done as f64 / elapsed;
                        let remaining = total.saturating_sub(done);
                        let eta_sec = (remaining as f64 / rate.max(0.01)) as u64;
                        eprintln!(
                            "embed: {done}/{total} chunks ({rate:.1}/s, eta ~{}m{:02}s)",
                            eta_sec / 60,
                            eta_sec % 60,
                        );
                        last_logged = done;
                    }
                }
                Err(e) => return Err(e.context("embedder failed for batch")),
            }
        }
        Ok(done)
    })
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
        let text = embed_input_for(chunk);
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
        let id = s.upsert_chunk(&dummy_chunk("only", "body")).unwrap();
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

    #[test]
    fn embed_pending_concurrent_persists_all_pending_chunks() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        for i in 0..50 {
            s.upsert_chunk(&dummy_chunk(&format!("c{i}"), "body"))
                .unwrap();
        }
        let m = MockEmbedder::default();
        let count = embed_pending_concurrent(&mut s, &m, 8, 4).unwrap();
        assert_eq!(count, 50);

        // All chunks now have an embedding for the mock model.
        let still_pending = s.chunks_missing_embedding(m.model_id()).unwrap();
        assert!(still_pending.is_empty());
    }

    #[test]
    fn embed_pending_concurrent_is_resumable() {
        // Embed half via the sequential path; resume via the concurrent
        // one. Result: all 30 chunks embedded exactly once.
        let mut s = SqliteStore::open_in_memory().unwrap();
        let mut ids = Vec::new();
        for i in 0..30 {
            ids.push(
                s.upsert_chunk(&dummy_chunk(&format!("c{i}"), "body"))
                    .unwrap(),
            );
        }
        let m = MockEmbedder::default();

        // First pass — embed only the first 15 manually so we have a
        // partial state to resume from.
        for id in &ids[..15] {
            let v = m.embed(&["partial".into()]).unwrap()[0].clone();
            s.upsert_embedding(*id, m.model_id(), &v).unwrap();
        }
        assert_eq!(
            s.chunks_missing_embedding(m.model_id()).unwrap().len(),
            15,
            "15 should still be pending"
        );

        let count = embed_pending_concurrent(&mut s, &m, 4, 3).unwrap();
        assert_eq!(count, 15, "should only embed the previously-missing 15");
        assert!(s.chunks_missing_embedding(m.model_id()).unwrap().is_empty());
    }

    #[test]
    fn embed_pending_concurrent_with_one_worker_works() {
        // The concurrency=1 short-circuit returns through embed_pending;
        // verify the result is the same shape as the multi-worker path.
        let mut s = SqliteStore::open_in_memory().unwrap();
        for i in 0..10 {
            s.upsert_chunk(&dummy_chunk(&format!("c{i}"), "body"))
                .unwrap();
        }
        let m = MockEmbedder::default();
        let count = embed_pending_concurrent(&mut s, &m, 4, 1).unwrap();
        assert_eq!(count, 10);
    }

    #[test]
    fn embed_pending_concurrent_returns_zero_when_nothing_pending() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        s.upsert_chunk(&dummy_chunk("only", "body")).unwrap();
        let m = MockEmbedder::default();
        // First pass embeds it.
        embed_pending_concurrent(&mut s, &m, 4, 4).unwrap();
        // Second pass has nothing to do.
        let count = embed_pending_concurrent(&mut s, &m, 4, 4).unwrap();
        assert_eq!(count, 0);
    }

    /// Stress: many workers + many small batches. Confirms we don't
    /// deadlock or drop chunks under high distribution.
    #[test]
    fn embed_pending_concurrent_does_not_drop_chunks_under_high_concurrency() {
        let mut s = SqliteStore::open_in_memory().unwrap();
        for i in 0..200 {
            s.upsert_chunk(&dummy_chunk(&format!("c{i}"), "body"))
                .unwrap();
        }
        let m = MockEmbedder::default();
        let count = embed_pending_concurrent(&mut s, &m, 4, 16).unwrap();
        assert_eq!(count, 200);
        assert!(s.chunks_missing_embedding(m.model_id()).unwrap().is_empty());
    }
}
