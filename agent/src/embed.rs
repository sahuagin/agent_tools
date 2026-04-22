use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

// ── Helpers ─────────────────────────────────────────────────────────────────

pub fn content_hash(text: &str) -> String {
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    format!("{:x}", h.finalize())
}

pub fn f32_to_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * 4);
    for &x in v {
        out.extend_from_slice(&x.to_le_bytes());
    }
    out
}

pub fn blob_to_f32(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let mut dot = 0.0f32;
    let mut na = 0.0f32;
    let mut nb = 0.0f32;
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Conservative char limit — ~6K tokens at ~4 chars/token, safe for 8K-context
/// embedding models and well within Qwen3's 32K. Memories longer than this are
/// truncated for embedding only; the full content remains in the memories table.
pub const EMBED_CHAR_LIMIT: usize = 24_000;

pub fn memory_embed_text(name: &str, description: &str, content: &str) -> String {
    let header = format!("{name}\n\n{description}\n\n");
    let budget = EMBED_CHAR_LIMIT.saturating_sub(header.len());
    if content.len() <= budget {
        format!("{header}{content}")
    } else {
        // Byte-safe truncation at a char boundary
        let mut end = budget;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        format!("{header}{}", &content[..end])
    }
}

// ── Trait ────────────────────────────────────────────────────────────────────

pub trait Embedder: Send + Sync {
    fn model_id(&self) -> &str;
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>>;
}

// ── Mock embedder (deterministic, no network) ───────────────────────────────

pub struct MockEmbedder {
    dims: usize,
}

impl MockEmbedder {
    pub fn new() -> Self {
        Self { dims: 128 }
    }
}

impl Embedder for MockEmbedder {
    fn model_id(&self) -> &str {
        "mock-hash-128"
    }
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        // Expand a SHA-256 seed into dims floats by hashing (seed || counter).
        // Deterministic per input; similar inputs produce unrelated vectors
        // (no semantic meaning). Useful only for wiring and schema tests.
        let out = texts
            .iter()
            .map(|t| {
                let mut v = vec![0.0f32; self.dims];
                let mut filled = 0;
                let mut counter: u32 = 0;
                while filled < self.dims {
                    let mut h = Sha256::new();
                    h.update(t.as_bytes());
                    h.update(counter.to_le_bytes());
                    let digest = h.finalize();
                    for chunk in digest.chunks_exact(2) {
                        if filled >= self.dims {
                            break;
                        }
                        let u = u16::from_le_bytes([chunk[0], chunk[1]]);
                        v[filled] = ((u as f32) - 32767.5) / 32767.5;
                        filled += 1;
                    }
                    counter += 1;
                }
                v
            })
            .collect();
        Ok(out)
    }
}

// ── OpenRouter embedder ──────────────────────────────────────────────────────

pub struct OpenRouterEmbedder {
    api_key: String,
    model: String,
    base_url: String,
    dims: usize,
}

impl OpenRouterEmbedder {
    pub fn from_env() -> Option<Self> {
        let api_key = std::env::var("OPENROUTER_API_KEY").ok()?;
        let model = std::env::var("AGENT_EMBED_MODEL")
            .unwrap_or_else(|_| "baai/bge-large-en-v1.5".to_string());
        let dims: usize = std::env::var("AGENT_EMBED_DIMS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(1024);
        let base_url = std::env::var("AGENT_EMBED_BASE_URL")
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
        Some(Self { api_key, model, base_url, dims })
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
        let url = format!("{}/embeddings", self.base_url);
        let body = EmbeddingRequest {
            model: &self.model,
            input: texts,
        };
        let json = serde_json::to_value(&body)?;

        let resp = match ureq::post(&url)
            .set("Authorization", &format!("Bearer {}", self.api_key))
            .set("Content-Type", "application/json")
            .send_json(json)
        {
            Ok(r) => r,
            Err(ureq::Error::Status(code, r)) => {
                let body = r.into_string().unwrap_or_default();
                return Err(anyhow!("embedding API HTTP {code}: {body}"));
            }
            Err(e) => return Err(anyhow!("embedding request failed: {e}")),
        };

        // Parse flexibly — OpenRouter occasionally returns 200 with an error body
        let raw: serde_json::Value = resp.into_json().context("read embedding response")?;

        if let Some(err) = raw.get("error") {
            return Err(anyhow!("embedding API error: {err}"));
        }

        let parsed: EmbeddingResponse =
            serde_json::from_value(raw.clone()).with_context(|| {
                let preview: String = raw.to_string().chars().take(500).collect();
                format!("parse embedding response (got: {preview})")
            })?;

        if parsed.data.len() != texts.len() {
            return Err(anyhow!(
                "expected {} embeddings, got {}",
                texts.len(),
                parsed.data.len()
            ));
        }
        let vectors: Vec<Vec<f32>> = parsed.data.into_iter().map(|d| d.embedding).collect();
        if let Some(first) = vectors.first() {
            if first.len() != self.dims {
                eprintln!(
                    "warning: expected dims={} but got {}; adjust AGENT_EMBED_DIMS",
                    self.dims,
                    first.len()
                );
            }
        }
        Ok(vectors)
    }
}

// ── Selection ────────────────────────────────────────────────────────────────

pub fn select_embedder() -> Box<dyn Embedder> {
    if let Some(e) = OpenRouterEmbedder::from_env() {
        Box::new(e)
    } else {
        Box::new(MockEmbedder::new())
    }
}

// ── Storage helpers ──────────────────────────────────────────────────────────

pub fn embed_one(
    conn: &Connection,
    embedder: &dyn Embedder,
    id: &str,
    text: &str,
) -> Result<bool> {
    let hash = content_hash(text);

    let existing: Option<String> = conn
        .query_row(
            "SELECT content_hash FROM memory_embeddings
             WHERE memory_id = ?1 AND model = ?2",
            params![id, embedder.model_id()],
            |r| r.get(0),
        )
        .optional()?;

    if existing.as_deref() == Some(hash.as_str()) {
        return Ok(false); // unchanged, nothing to do
    }

    let vec = embedder
        .embed(&[text.to_string()])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow!("embedder returned no vector"))?;
    let blob = f32_to_blob(&vec);
    let now_ts = chrono::Utc::now().timestamp();
    conn.execute(
        "INSERT OR REPLACE INTO memory_embeddings
         (memory_id, model, dims, content_hash, embedded_at, vector)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            id,
            embedder.model_id(),
            vec.len() as i64,
            hash,
            now_ts,
            blob
        ],
    )?;
    Ok(true)
}

pub fn try_embed_one(conn: &Connection, id: &str, text: &str) {
    let embedder = select_embedder();
    if let Err(e) = embed_one(conn, embedder.as_ref(), id, text) {
        eprintln!("warning: embedding failed for {id}: {e}");
    }
}
