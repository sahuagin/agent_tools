//! Write-time supersession adjudicator — at-supersession-activation-gf2.7.
//!
//! On `agent memory add`, nominate the stored memories most similar to
//! the new one (embeddings ∪ FTS5 — INCLUDING superseded rows, so a
//! re-learned stale fact meets its own tombstone instead of re-entering
//! as fresh truth), ask an LLM for a typed relation per candidate, and:
//!
//!   - high-confidence `corrects`/`updates` → create the supersession
//!     edge through the same machinery as the manual `correct` verb;
//!   - mid-confidence (or `duplicate`) → a `conflict_suspected` queue
//!     row for the operator (resolved with `correct`/`retract`);
//!   - everything else → ignored.
//!
//! FAIL-OPEN is the contract: the add has already succeeded before this
//! runs, and no config/network/model/parse failure may turn into an add
//! failure — every error path degrades to "no adjudication" with a
//! warning. Memory infrastructure must never gate the write path (the
//! same discipline as mu's action_recall).
//!
//! Configuration lives in the `[adjudicate]` section of
//! `~/.config/agent/config.toml` — see **`config.toml.example`** at the
//! repo root for the complete documented reference (all sections, all
//! keys, defaults). No `[adjudicate]` section (or no model) =
//! adjudication off. Env overrides mirror the embedder convention:
//! `AGENT_ADJUDICATE_BASE_URL` / `AGENT_ADJUDICATE_MODEL`, and
//! `AGENT_NO_ADJUDICATE=1` disables.
//!
//! Design: Plan A recommendation.md §3.1
//! (mu/.delegations/overnight-2026-06-12/RESULTS-fable5/); the
//! nominate-then-classify shape is the Mem0/Graphiti write-time pattern,
//! with verdicts as PROPOSALS (auditable, reversible edges) rather than
//! Mem0's destructive DELETE.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection};
use serde::Deserialize;

use crate::embed;

/// Candidates fetched by embedding similarity.
const NOMINATE_TOP_K_VEC: usize = 10;
/// Candidates fetched by FTS5 (exact-term matches embeddings may rank low).
const NOMINATE_TOP_K_FTS: usize = 5;
/// Per-candidate content excerpt in the prompt — enough to judge a
/// contradiction, small enough to keep the call cheap.
const CANDIDATE_EXCERPT_CHARS: usize = 600;
/// LLM call budget. The add path is off the latency-critical path, but
/// a CLI must not hang on a wedged endpoint.
const LLM_TIMEOUT_SECS: u64 = 30;

#[derive(Debug, Clone)]
pub struct AdjudicateConfig {
    pub base_url: String,
    pub model: String,
    pub auto_threshold: f64,
    pub queue_threshold: f64,
}

/// Resolve config: env override → config.toml `[adjudicate]` → off.
fn config() -> Option<AdjudicateConfig> {
    if std::env::var("AGENT_NO_ADJUDICATE").is_ok_and(|v| v == "1") {
        return None;
    }
    let model = std::env::var("AGENT_ADJUDICATE_MODEL")
        .ok()
        .or_else(|| embed::read_config_toml_value("adjudicate", "model"))?;
    let base_url = std::env::var("AGENT_ADJUDICATE_BASE_URL")
        .ok()
        .or_else(|| embed::read_config_toml_value("adjudicate", "base_url"))?;
    let parse_threshold = |key: &str, default: f64| {
        embed::read_config_toml_value("adjudicate", key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    };
    Some(AdjudicateConfig {
        base_url,
        model,
        auto_threshold: parse_threshold("auto_threshold", 0.8),
        queue_threshold: parse_threshold("queue_threshold", 0.5),
    })
}

#[derive(Debug, Clone)]
pub struct Candidate {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub lifecycle: String,
}

/// One adjudication verdict, as returned by the model.
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Verdict {
    /// 1-based index into the candidate list shown to the model.
    pub candidate: usize,
    /// duplicate | refines | updates | corrects | unrelated
    pub relation: String,
    pub confidence: f64,
    #[serde(default)]
    pub rationale: String,
}

/// Fail-open entry point, called at the end of `add`. Never propagates
/// an error into the (already successful) add.
pub fn maybe_adjudicate(conn: &Connection, new_id: &str) {
    let Some(cfg) = config() else {
        log::debug!("adjudication off (no [adjudicate] config)");
        return;
    };
    match adjudicate(conn, new_id, &cfg) {
        Ok(summary) if !summary.is_empty() => log::info!("adjudication: {summary}"),
        Ok(_) => log::debug!("adjudication: no actionable relations"),
        Err(e) => log::warn!("adjudication skipped (fail-open): {e:#}"),
    }
}

fn adjudicate(conn: &Connection, new_id: &str, cfg: &AdjudicateConfig) -> Result<String> {
    let new_mem = load_candidate(conn, new_id)?;
    let candidates = nominate(conn, new_id)?;
    if candidates.is_empty() {
        return Ok(String::new());
    }
    let prompt = build_prompt(&new_mem, &candidates);
    let raw = call_llm(cfg, &prompt)?;
    let verdicts = parse_verdicts(&raw)?;
    apply_verdicts(conn, new_id, &candidates, &verdicts, cfg)
}

fn load_candidate(conn: &Connection, id: &str) -> Result<Candidate> {
    conn.query_row(
        "SELECT id, name, description, content, lifecycle FROM memories WHERE id = ?1",
        params![id],
        |r| {
            Ok(Candidate {
                id: r.get(0)?,
                name: r.get(1)?,
                description: r.get(2)?,
                content: r.get(3)?,
                lifecycle: r.get(4)?,
            })
        },
    )
    .with_context(|| format!("loading memory {id}"))
}

/// Hybrid nomination: embedding cosine top-K ∪ FTS5 top-K.
///
/// THE ZOMBIE RULE: this query must NOT exclude superseded/retracted
/// rows — recall's read-path filter does not apply here. A stale fact
/// re-extracted from old context has to be matched against its own
/// tombstone (and NOOP'd / flagged) instead of re-entering as a fresh
/// active memory. (survey Q1.6: Cassandra's zombie-resurrection class.)
pub fn nominate(conn: &Connection, new_id: &str) -> Result<Vec<Candidate>> {
    let mut out: Vec<Candidate> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    seen.insert(new_id.to_string());

    // Vector leg: brute-force cosine against the new memory's stored
    // embedding (written best-effort by add; absent = leg skipped).
    let embedder = embed::select_embedder();
    let model = embedder.model_id().to_string();
    let new_vec: Option<Vec<u8>> = conn
        .query_row(
            "SELECT vector FROM memory_embeddings WHERE memory_id = ?1 AND model = ?2",
            params![new_id, model],
            |r| r.get(0),
        )
        .ok();
    if let Some(blob) = new_vec {
        let qv = embed::blob_to_f32(&blob);
        let mut stmt = conn.prepare(
            "SELECT m.id, e.vector FROM memory_embeddings e
             JOIN memories m ON m.id = e.memory_id
             WHERE e.model = ?1 AND m.id != ?2",
        )?;
        let mut scored: Vec<(String, f32)> = stmt
            .query_map(params![model, new_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .filter_map(|r| r.ok())
            .map(|(id, blob)| {
                let cos = embed::cosine(&qv, &embed::blob_to_f32(&blob));
                (id, cos)
            })
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (id, _) in scored.into_iter().take(NOMINATE_TOP_K_VEC) {
            if seen.insert(id.clone()) {
                out.push(load_candidate(conn, &id)?);
            }
        }
    } else {
        log::debug!("nominate: no embedding for {new_id} under {model}; vector leg skipped");
    }

    // Lexical leg: OR-joined distinctive tokens from name+description —
    // catches exact terms ('FreeBSD') the embedder may rank lower.
    let new_mem = load_candidate(conn, new_id)?;
    let fts_query = fts_or_query(&format!("{} {}", new_mem.name, new_mem.description));
    if !fts_query.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT m.id FROM memories_fts fts
             JOIN memories m ON m.rowid = fts.rowid
             WHERE memories_fts MATCH ?1 AND m.id != ?2
             ORDER BY rank LIMIT ?3",
        )?;
        let ids: Vec<String> = stmt
            .query_map(params![fts_query, new_id, NOMINATE_TOP_K_FTS as i64], |r| {
                r.get(0)
            })?
            .filter_map(|r| r.ok())
            .collect();
        for id in ids {
            if seen.insert(id.clone()) {
                out.push(load_candidate(conn, &id)?);
            }
        }
    }
    Ok(out)
}

/// Quoted, OR-joined FTS5 query from a text's distinctive tokens (the
/// implicit-AND of fts5_match_query is far too restrictive for
/// nomination — any shared distinctive term should nominate).
fn fts_or_query(text: &str) -> String {
    let mut seen = std::collections::HashSet::new();
    let toks: Vec<String> = text
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 4)
        .map(|t| t.to_lowercase())
        .filter(|t| seen.insert(t.clone()))
        .take(8)
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    toks.join(" OR ")
}

fn excerpt(s: &str) -> &str {
    match s.char_indices().nth(CANDIDATE_EXCERPT_CHARS) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

fn build_prompt(new_mem: &Candidate, candidates: &[Candidate]) -> String {
    let mut p = String::from(
        "A new memory was just written to an agent memory store. Compare it against each \
         existing candidate memory and classify their relation:\n\
         - \"corrects\": the candidate states something that was NEVER true and the new memory corrects it\n\
         - \"updates\": the candidate WAS true but the world changed; the new memory is the current state\n\
         - \"duplicate\": same assertion, no new information\n\
         - \"refines\": same topic, the new memory adds detail without contradicting\n\
         - \"unrelated\": none of the above\n\
         Judge content, not phrasing. Temporal succession (lived in X, now lives in Y) is \
         \"updates\", not \"corrects\". If unsure, prefer \"unrelated\" with low confidence.\n\
         Respond with ONLY a JSON array, one object per candidate:\n\
         [{\"candidate\": <1-based index>, \"relation\": \"...\", \"confidence\": 0.0-1.0, \"rationale\": \"<one line>\"}]\n\n",
    );
    p.push_str(&format!(
        "NEW MEMORY:\nname: {}\ndescription: {}\ncontent: {}\n\nCANDIDATES:\n",
        new_mem.name,
        new_mem.description,
        excerpt(&new_mem.content)
    ));
    for (i, c) in candidates.iter().enumerate() {
        p.push_str(&format!(
            "{}. [{}] (lifecycle: {}) {} — {}\n   {}\n",
            i + 1,
            c.id,
            c.lifecycle,
            c.name,
            c.description,
            excerpt(&c.content)
        ));
    }
    p
}

fn call_llm(cfg: &AdjudicateConfig, prompt: &str) -> Result<String> {
    let url = format!("{}/chat/completions", cfg.base_url.trim_end_matches('/'));
    let body = serde_json::json!({
        "model": cfg.model,
        "temperature": 0,
        "messages": [{"role": "user", "content": prompt}],
    });
    let resp = ureq::post(&url)
        .timeout(std::time::Duration::from_secs(LLM_TIMEOUT_SECS))
        .send_json(body)
        .map_err(|e| anyhow::anyhow!("adjudicator LLM call failed: {e}"))?;
    let v: serde_json::Value = resp.into_json().context("parsing LLM response envelope")?;
    v["choices"][0]["message"]["content"]
        .as_str()
        .map(String::from)
        .ok_or_else(|| anyhow::anyhow!("LLM response had no message content"))
}

/// Tolerant verdict extraction: take the outermost JSON array in the
/// text (models wrap JSON in prose/fences despite instructions).
pub fn parse_verdicts(raw: &str) -> Result<Vec<Verdict>> {
    let start = raw.find('[').context("no JSON array in LLM output")?;
    let end = raw.rfind(']').context("no closing bracket in LLM output")?;
    if end <= start {
        bail!("malformed JSON array bounds in LLM output");
    }
    let verdicts: Vec<Verdict> =
        serde_json::from_str(&raw[start..=end]).context("parsing verdict array")?;
    Ok(verdicts)
}

/// Apply verdicts: auto-edge at high confidence, queue at medium, log
/// the rest. Returns a one-line human summary ("" = nothing actionable).
pub fn apply_verdicts(
    conn: &Connection,
    new_id: &str,
    candidates: &[Candidate],
    verdicts: &[Verdict],
    cfg: &AdjudicateConfig,
) -> Result<String> {
    let ts = crate::memory::now();
    let mut actions: Vec<String> = Vec::new();
    for v in verdicts {
        let Some(cand) = v.candidate.checked_sub(1).and_then(|i| candidates.get(i)) else {
            log::warn!("verdict references unknown candidate index {}", v.candidate);
            continue;
        };
        let conf = v.confidence.clamp(0.0, 1.0);
        match v.relation.as_str() {
            rel @ ("corrects" | "updates") => {
                // Zombie case: the "old" side is already superseded /
                // retracted — nothing to do edge-wise, but say so loudly:
                // the new memory may be a re-learned stale fact.
                if cand.lifecycle != "active" {
                    log::warn!(
                        "new memory {new_id} {rel} ALREADY-{} {} — possible re-learned stale fact",
                        cand.lifecycle,
                        cand.id
                    );
                    queue_conflict(conn, &cand.id, new_id, rel, conf, &v.rationale, ts)?;
                    actions.push(format!("zombie-flag {} ({rel})", cand.id));
                } else if conf >= cfg.auto_threshold {
                    crate::memory::apply_supersession(
                        conn,
                        &cand.id,
                        new_id,
                        rel,
                        &v.rationale,
                        conf,
                        "adjudicator",
                    )?;
                    actions.push(format!("{} {rel}-edge (conf {conf:.2})", cand.id));
                } else if conf >= cfg.queue_threshold {
                    queue_conflict(conn, &cand.id, new_id, rel, conf, &v.rationale, ts)?;
                    actions.push(format!("{} queued ({rel}, conf {conf:.2})", cand.id));
                }
            }
            "duplicate" if conf >= cfg.queue_threshold => {
                queue_conflict(conn, &cand.id, new_id, "duplicate", conf, &v.rationale, ts)?;
                actions.push(format!("{} queued (duplicate, conf {conf:.2})", cand.id));
            }
            _ => {}
        }
    }
    Ok(actions.join("; "))
}

fn queue_conflict(
    conn: &Connection,
    old_id: &str,
    new_id: &str,
    relation: &str,
    confidence: f64,
    rationale: &str,
    ts: i64,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO conflict_suspected
         (old_id, new_id, relation, confidence, rationale, status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
        params![old_id, new_id, relation, confidence, rationale, ts],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    fn cfg() -> AdjudicateConfig {
        AdjudicateConfig {
            base_url: "http://unused".into(),
            model: "stub".into(),
            auto_threshold: 0.8,
            queue_threshold: 0.5,
        }
    }

    fn seed(conn: &Connection, id: &str, name: &str, content: &str) {
        conn.execute(
            "INSERT INTO memories (id, type, name, description, content, created_at, updated_at)
             VALUES (?1, 'project', ?2, 'd', ?3, 1000, 1000)",
            params![id, name, content],
        )
        .unwrap();
    }

    fn cand(conn: &Connection, id: &str) -> Candidate {
        load_candidate(conn, id).unwrap()
    }

    #[test]
    fn parse_verdicts_tolerates_prose_wrapping() {
        let raw = "Sure! Here is the analysis:\n```json\n[\
                   {\"candidate\":1,\"relation\":\"corrects\",\"confidence\":0.95,\"rationale\":\"negation\"}\
                   ]\n```\nDone.";
        let v = parse_verdicts(raw).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].relation, "corrects");
        assert!(parse_verdicts("no json here").is_err());
    }

    #[test]
    fn high_confidence_corrects_creates_edge() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(
            &conn,
            "stale",
            "old-belief",
            "claude-code cannot run on FreeBSD",
        );
        seed(
            &conn,
            "fresh",
            "correction",
            "claude-code DOES run on FreeBSD via linuxulator",
        );
        let verdicts = vec![Verdict {
            candidate: 1,
            relation: "corrects".into(),
            confidence: 0.95,
            rationale: "direct negation".into(),
        }];
        let summary =
            apply_verdicts(&conn, "fresh", &[cand(&conn, "stale")], &verdicts, &cfg()).unwrap();
        assert!(summary.contains("corrects-edge"), "{summary}");
        let (lc, succ): (String, String) = conn
            .query_row(
                "SELECT lifecycle, superseded_by FROM memories WHERE id='stale'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(lc, "superseded");
        assert_eq!(succ, "fresh");
        let (kind, conf, actor): (String, f64, String) = conn
            .query_row(
                "SELECT kind, confidence, actor FROM supersessions WHERE old_id='stale' AND new_id='fresh'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(kind, "corrects");
        assert_eq!(conf, 0.95);
        assert_eq!(actor, "adjudicator");
    }

    #[test]
    fn mid_confidence_queues_instead_of_edging() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "old", "maybe-stale", "x");
        seed(&conn, "new", "maybe-correction", "y");
        let verdicts = vec![Verdict {
            candidate: 1,
            relation: "updates".into(),
            confidence: 0.6,
            rationale: "unsure".into(),
        }];
        apply_verdicts(&conn, "new", &[cand(&conn, "old")], &verdicts, &cfg()).unwrap();
        let lc: String = conn
            .query_row("SELECT lifecycle FROM memories WHERE id='old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(lc, "active", "mid confidence must not flip lifecycle");
        let (rel, status): (String, String) = conn
            .query_row(
                "SELECT relation, status FROM conflict_suspected WHERE old_id='old' AND new_id='new'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(rel, "updates");
        assert_eq!(status, "open");
    }

    #[test]
    fn zombie_candidate_is_flagged_not_edged() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "tomb", "retired-fact", "x");
        seed(&conn, "heir", "its-successor", "y");
        seed(&conn, "zombie", "relearned-stale", "x again");
        conn.execute(
            "UPDATE memories SET lifecycle='superseded', superseded_by='heir' WHERE id='tomb'",
            [],
        )
        .unwrap();
        let verdicts = vec![Verdict {
            candidate: 1,
            relation: "corrects".into(),
            confidence: 0.99,
            rationale: "matches retired fact".into(),
        }];
        let summary =
            apply_verdicts(&conn, "zombie", &[cand(&conn, "tomb")], &verdicts, &cfg()).unwrap();
        assert!(summary.contains("zombie-flag"), "{summary}");
        // No new edge; tombstone untouched; queue row exists.
        let edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM supersessions WHERE new_id='zombie'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edges, 0);
        let queued: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM conflict_suspected WHERE new_id='zombie'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(queued, 1);
    }

    #[test]
    fn nominate_includes_superseded_rows() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(
            &conn,
            "tomb",
            "freebsd-claim",
            "claude-code cannot run on FreeBSD ever",
        );
        seed(
            &conn,
            "fresh",
            "freebsd-truth",
            "claude-code runs on FreeBSD fine",
        );
        conn.execute(
            "UPDATE memories SET lifecycle='superseded' WHERE id='tomb'",
            [],
        )
        .unwrap();
        // No embeddings in the test DB — the vector leg skips; the FTS
        // leg must still nominate the superseded row (the zombie rule).
        let cands = nominate(&conn, "fresh").unwrap();
        assert!(
            cands.iter().any(|c| c.id == "tomb"),
            "superseded row must be nominable: {:?}",
            cands.iter().map(|c| &c.id).collect::<Vec<_>>()
        );
    }

    #[test]
    fn fts_or_query_shape() {
        let q = fts_or_query("FreeBSD claude-code recall ranking");
        assert!(q.contains("\"freebsd\"") && q.contains(" OR "), "{q}");
        assert!(fts_or_query("a an it").is_empty());
    }
}
