use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Default per-call embedding HTTP read timeout (ms). Bounds the hang when the
/// embedding endpoint (e.g. an occupied ollama box) accepts the connection but
/// never responds. Override per-section via `timeout_ms` /
/// `AGENT_EMBED[_FALLBACK]_TIMEOUT_MS`. (at-7mp)
const DEFAULT_EMBED_TIMEOUT_MS: u64 = 8_000;
/// Connect-phase timeout (ms) — bounds an unreachable host fast.
const CONNECT_TIMEOUT_MS: u64 = 5_000;

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
    timeout_ms: u64,
}

impl OpenRouterEmbedder {
    /// Primary embedder from the `[embed]` config section (env `AGENT_EMBED_*`
    /// overrides). On this stack it points at the local ollama box; the same
    /// OpenAI-compatible `/embeddings` shape also serves OpenRouter directly.
    ///
    /// Every setting resolves env var (per-invocation OVERRIDE) → `[embed]` in
    /// ~/.config/agent/config.toml (source of truth) → code default
    /// (at-supersession-activation-gf2.4).
    ///
    /// The api key resolves $OPENROUTER_API_KEY → `[openrouter].api_key`. Why
    /// the config fallback: long-running parents (claude-code, pi) cache the
    /// env value at spawn, so after a key rotation the running process keeps a
    /// stale key and 401s on every embed; reading config.toml lets the rotated
    /// key take effect without restarting every consumer. A non-OpenRouter
    /// endpoint (local ollama) doesn't authenticate, so a missing key there is
    /// fine ("unused").
    pub fn from_env() -> Option<Self> {
        Self::from_section(
            "embed",
            "AGENT_EMBED",
            "https://openrouter.ai/api/v1",
            "baai/bge-large-en-v1.5",
            1024,
        )
    }

    /// Optional fallback embedder from `[embed.fallback]` (env
    /// `AGENT_EMBED_FALLBACK_*`). Strictly opt-in: returns `None` unless the
    /// section is configured (a `base_url` or `model` is present), so installs
    /// without the section behave exactly as before. Defaults target
    /// OpenRouter's `openai/text-embedding-3-small` (1536-dim). (at-7mp)
    pub fn fallback_from_config() -> Option<Self> {
        let configured = resolve_setting(
            "embed.fallback",
            "base_url",
            "AGENT_EMBED_FALLBACK_BASE_URL",
        )
        .is_some()
            || resolve_setting("embed.fallback", "model", "AGENT_EMBED_FALLBACK_MODEL").is_some();
        if !configured {
            return None;
        }
        Self::from_section(
            "embed.fallback",
            "AGENT_EMBED_FALLBACK",
            "https://openrouter.ai/api/v1",
            "openai/text-embedding-3-small",
            1536,
        )
    }

    /// Build an embedder from a config section + env prefix, applying the
    /// env → config.toml → default resolution to every setting.
    fn from_section(
        section: &str,
        env_prefix: &str,
        default_base: &str,
        default_model: &str,
        default_dims: usize,
    ) -> Option<Self> {
        let model = resolve_setting(section, "model", &format!("{env_prefix}_MODEL"))
            .unwrap_or_else(|| default_model.to_string());
        let dims: usize = resolve_setting(section, "dims", &format!("{env_prefix}_DIMS"))
            .and_then(|s| s.parse().ok())
            .unwrap_or(default_dims);
        let base_url = resolve_setting(section, "base_url", &format!("{env_prefix}_BASE_URL"))
            .unwrap_or_else(|| default_base.to_string());
        let timeout_ms: u64 =
            resolve_setting(section, "timeout_ms", &format!("{env_prefix}_TIMEOUT_MS"))
                .and_then(|s| s.parse().ok())
                .unwrap_or(DEFAULT_EMBED_TIMEOUT_MS);
        let api_key = std::env::var("OPENROUTER_API_KEY")
            .ok()
            .filter(|s| !s.is_empty())
            .or_else(read_openrouter_key_from_config_toml)
            .or_else(|| {
                // A non-OpenRouter endpoint (local ollama, etc.) does not
                // authenticate; a missing key must not disable local embedding.
                (!base_url.contains("openrouter")).then(|| "unused".to_string())
            })?;
        Some(Self {
            api_key,
            model,
            base_url,
            dims,
            timeout_ms,
        })
    }
}

/// Resolve one setting: env var (per-invocation override) → `[section].key`
/// in ~/.config/agent/config.toml → `None` (caller supplies the default).
fn resolve_setting(section: &str, key: &str, env: &str) -> Option<String> {
    std::env::var(env)
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| read_config_toml_value(section, key))
}

/// Read `[openrouter].api_key` from `~/.config/agent/config.toml` as a
/// fallback when the env var is missing or stale.
///
/// Returns `None` on any failure (file missing, key missing, permissions).
fn read_openrouter_key_from_config_toml() -> Option<String> {
    read_config_toml_value("openrouter", "api_key")
}

/// Read `[section].key` from `~/.config/agent/config.toml`. Hand-rolled
/// mini-parser (flat `key = "value"` pairs under bracketed sections; no
/// nesting, no arrays) so we don't add a `toml` crate dep for a fallback
/// path. at-supersession-activation-gf2.4 generalized this from the
/// api_key-only reader so `[embed]` settings resolve the same way.
///
/// Returns `None` on any failure (file missing, key missing, permissions).
pub(crate) fn read_config_toml_value(section: &str, key: &str) -> Option<String> {
    let home = std::env::var("HOME").ok()?;
    let path = format!("{}/.config/agent/config.toml", home);
    let content = std::fs::read_to_string(&path).ok()?;
    read_toml_value_from_str(&content, section, key)
}

/// The parsing half of [`read_config_toml_value`], split out for tests.
fn read_toml_value_from_str(content: &str, section: &str, key: &str) -> Option<String> {
    let header = format!("[{section}]");
    let mut in_section = false;
    for raw in content.lines() {
        let line = raw.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_section = line == header;
            continue;
        }
        if !in_section {
            continue;
        }
        // Match: key = "..." (or single-quoted / bare; whitespace tolerant)
        let (k, value) = match line.split_once('=') {
            Some(p) => p,
            None => continue,
        };
        if k.trim() != key {
            continue;
        }
        // Strip an optional inline comment after the value (`val # comment`).
        let value = value.split('#').next().unwrap_or("").trim();
        // Strip surrounding quotes (either kind).
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
        // Try with the configured key (env value, captured at construction).
        match self.embed_with_key(texts, &self.api_key) {
            Ok(v) => Ok(v),
            Err(e) => {
                // On HTTP 401 specifically, the env key is likely stale (long-
                // running parent process cached an old value at spawn). Try
                // exactly once more with the fresh value from
                // ~/.config/agent/config.toml. If THAT also 401s the key is
                // genuinely revoked — propagate the original error.
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

        // Bounded timeouts so an occupied/unreachable endpoint fails fast
        // instead of hanging the caller (recall used to block ~40s on a busy
        // ollama box before returning empty). (at-7mp)
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(Duration::from_millis(CONNECT_TIMEOUT_MS))
            .timeout_read(Duration::from_millis(self.timeout_ms))
            .build();
        let resp = match agent
            .post(&url)
            .set("Authorization", &format!("Bearer {}", key))
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

        let parsed: EmbeddingResponse = serde_json::from_value(raw.clone()).with_context(|| {
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

/// The embedder chain: primary (`[embed]`) then the optional fallback
/// (`[embed.fallback]`). Empty config falls back to the deterministic mock so
/// wiring/tests still function. recall + the write path + reindex iterate this
/// so the parallel `(memory_id, model)` index stays populated for every
/// configured embedder. (at-7mp)
pub fn embedder_chain() -> Vec<Box<dyn Embedder>> {
    let mut chain: Vec<Box<dyn Embedder>> = Vec::new();
    if let Some(e) = OpenRouterEmbedder::from_env() {
        chain.push(Box::new(e));
    }
    if let Some(e) = OpenRouterEmbedder::fallback_from_config() {
        chain.push(Box::new(e));
    }
    if chain.is_empty() {
        chain.push(Box::new(MockEmbedder::new()));
    }
    chain
}

/// Embed a single query, trying each embedder in the chain (primary, then
/// fallback) until one succeeds. Returns the vector AND the `model_id` that
/// produced it — recall filters `memory_embeddings` by that model, so results
/// are always compared within one vector space. Returns `None` only if EVERY
/// embedder failed (e.g. ollama busy AND OpenRouter unreachable); the caller
/// should then degrade to lexical search rather than hang. (at-7mp)
pub fn embed_query_with_fallback(query: &str) -> Option<(Vec<f32>, String)> {
    let input = [query.to_string()];
    for embedder in embedder_chain() {
        match embedder.embed(&input) {
            Ok(mut vs) => {
                if let Some(v) = vs.drain(..).next() {
                    return Some((v, embedder.model_id().to_string()));
                }
            }
            Err(e) => {
                eprintln!(
                    "info: embedder '{}' unavailable ({e:#}); trying next in chain",
                    embedder.model_id()
                );
            }
        }
    }
    None
}

// ── Storage helpers ──────────────────────────────────────────────────────────

pub fn embed_one(conn: &Connection, embedder: &dyn Embedder, id: &str, text: &str) -> Result<bool> {
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
    // Dual-write: embed under every embedder in the chain so the fallback's
    // (memory_id, model) rows stay populated and recall can match them during
    // a primary-endpoint outage. Fail-open per embedder. (at-7mp)
    for embedder in embedder_chain() {
        if let Err(e) = embed_one(conn, embedder.as_ref(), id, text) {
            eprintln!(
                "warning: embedding ({}) failed for {id}: {e}",
                embedder.model_id()
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
# comment line
[openrouter]
api_key = "sk-or-abc" # expires someday

[embed]
base_url = "http://debian13rtx4000:11434/v1"
model = 'qwen3-embedding:8b'
dims = 4096

[other]
model = "decoy"
"#;

    #[test]
    fn toml_reader_resolves_section_scoped_keys() {
        assert_eq!(
            read_toml_value_from_str(SAMPLE, "embed", "base_url").as_deref(),
            Some("http://debian13rtx4000:11434/v1")
        );
        assert_eq!(
            read_toml_value_from_str(SAMPLE, "embed", "model").as_deref(),
            Some("qwen3-embedding:8b"),
            "single quotes stripped"
        );
        assert_eq!(
            read_toml_value_from_str(SAMPLE, "embed", "dims").as_deref(),
            Some("4096"),
            "bare values pass through"
        );
        assert_eq!(
            read_toml_value_from_str(SAMPLE, "openrouter", "api_key").as_deref(),
            Some("sk-or-abc"),
            "inline comment stripped"
        );
    }

    #[test]
    fn toml_reader_respects_section_boundaries() {
        // `model` exists in [embed] and [other] — section gates the match.
        assert_eq!(
            read_toml_value_from_str(SAMPLE, "other", "model").as_deref(),
            Some("decoy")
        );
        assert_eq!(read_toml_value_from_str(SAMPLE, "embed", "api_key"), None);
        assert_eq!(read_toml_value_from_str(SAMPLE, "missing", "model"), None);
    }

    #[test]
    fn toml_reader_handles_dotted_fallback_section() {
        // [embed.fallback] is a distinct flat section to the hand-rolled
        // reader; keys must not bleed between it and [embed] either way. (at-7mp)
        const S: &str = "\
[embed]
model = \"primary\"

[embed.fallback]
model = \"openai/text-embedding-3-small\"
dims = 1536
";
        assert_eq!(
            read_toml_value_from_str(S, "embed", "model").as_deref(),
            Some("primary")
        );
        assert_eq!(
            read_toml_value_from_str(S, "embed.fallback", "model").as_deref(),
            Some("openai/text-embedding-3-small")
        );
        assert_eq!(
            read_toml_value_from_str(S, "embed.fallback", "dims").as_deref(),
            Some("1536")
        );
        // No bleed: [embed] has no `dims` key here.
        assert_eq!(read_toml_value_from_str(S, "embed", "dims"), None);
    }
}
