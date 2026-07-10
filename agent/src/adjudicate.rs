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
pub(crate) fn config() -> Option<AdjudicateConfig> {
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

pub(crate) fn adjudicate(
    conn: &Connection,
    new_id: &str,
    cfg: &AdjudicateConfig,
) -> Result<String> {
    let new_mem = load_candidate(conn, new_id)?;
    let candidates = nominate(conn, new_id)?;
    if candidates.is_empty() {
        return Ok(String::new());
    }
    let prompt = build_prompt(&new_mem, &candidates);
    let raw = call_llm(cfg, &prompt)?;
    let verdicts = parse_verdicts(&raw)?;
    apply_verdicts(conn, new_id, &candidates, &verdicts, cfg, false)
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

pub(crate) fn build_prompt(new_mem: &Candidate, candidates: &[Candidate]) -> String {
    let mut p = String::from(
        "A new memory was just written to an agent memory store. Compare it against each \
         existing candidate memory and classify their relation:\n\
         - \"corrects\": the candidate states something that was NEVER true and the new memory corrects it\n\
         - \"updates\": the candidate WAS true but the world changed; the new memory is the current state\n\
         - \"duplicate\": same assertion, no new information\n\
         - \"refines\": same topic, the new memory adds detail without contradicting\n\
         - \"unrelated\": none of the above\n\
         Judge content, not phrasing. Temporal succession (lived in X, now lives in Y) is \
         \"updates\", not \"corrects\". \"updates\" requires the candidate to assert a CURRENT \
         state of the world that the new memory explicitly replaces — a newer memory merely \
         post-dating, reaffirming, or reporting later progress on the same topic is NOT \
         \"updates\" (use \"refines\" or \"unrelated\"). Dated historical records (session \
         logs, status-as-of-a-date reports, war stories) are not superseded by later status: \
         they remain true records of their date. If unsure, prefer \"unrelated\" with low \
         confidence.\n\
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

pub(crate) fn call_llm(cfg: &AdjudicateConfig, prompt: &str) -> Result<String> {
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
    updates_queue_only: bool,
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
                } else if rel == "updates" && updates_queue_only {
                    // Sweep calibration (gf2.9 dry-run finding): on the
                    // backlog, the model conflates "newer memory, same
                    // topic" with supersession — 'updates' over-fires on
                    // historical records. In sweep mode 'updates' always
                    // queues for review; only 'corrects' (precise:
                    // falsity detection) auto-edges.
                    if conf >= cfg.queue_threshold {
                        queue_conflict(conn, &cand.id, new_id, rel, conf, &v.rationale, ts)?;
                        actions.push(format!("{} queued ({rel}, conf {conf:.2})", cand.id));
                    }
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

/// Prose markers that flag a memory as carrying a correction in TEXT —
/// the unlinked-backlog seed set (terrain: 123 such memories at the
/// time of writing, ~10% of the store).
const PROSE_MARKERS: &[&str] = &[
    "CORRECTED",
    "SUPERSEDES",
    "superseded",
    "OBSOLETE",
    "DEPRECATED",
    "no longer true",
];

/// SQL predicate: any prose marker appears in content or name.
/// gf2.13: instr() is case-sensitive, so the first live sweep caught
/// only 45 of ~123 prose-marked memories — mixed-case prose
/// ("Superseded by …") slipped the exact-case match. lower() both
/// sides; the markers are ASCII, so SQLite's ASCII-only lower() is
/// sufficient.
fn prose_marker_clause() -> String {
    PROSE_MARKERS
        .iter()
        .map(|m| {
            let m = m.to_ascii_lowercase();
            format!("instr(lower(content), '{m}') > 0 OR instr(lower(name), '{m}') > 0")
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

pub struct SweepOpts {
    pub prose_only: bool,
    pub dry_run: bool,
    pub limit: Option<usize>,
    pub force: bool,
}

/// gf2.9: backlog sweep — run the write-time adjudication pipeline over
/// EXISTING memories, which write-time detection can never reach (it
/// only sees conflicts involving the newest write; survey Q4.7).
/// Prose-marked memories order first: their corrections are written in
/// content text where no filter can see them, and they usually NAME
/// their target — the cheapest wins. Coverage ledger in sweep_state
/// makes re-runs resume (skip swept seeds unless --force); effects are
/// idempotent regardless (OR IGNORE edges and queue rows).
pub fn sweep(conn: &Connection, opts: &SweepOpts) -> Result<()> {
    let Some(cfg) = config() else {
        bail!("sweep requires [adjudicate] config (see config.toml.example)");
    };

    let marker_clause = prose_marker_clause();
    let prose_filter = if opts.prose_only {
        format!(" AND ({marker_clause})")
    } else {
        String::new()
    };
    let skip_swept = if opts.force {
        ""
    } else {
        " AND id NOT IN (SELECT seed_id FROM sweep_state)"
    };
    // Prose-marked first, then the rest, newest first within each group.
    let sql = format!(
        "SELECT id FROM memories
         WHERE is_active = 1 AND lifecycle = 'active'{prose_filter}{skip_swept}
         ORDER BY CASE WHEN ({marker_clause}) THEN 0 ELSE 1 END, updated_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut seeds: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if let Some(n) = opts.limit {
        seeds.truncate(n);
    }
    if seeds.is_empty() {
        println!("sweep: nothing to do (all candidate seeds already swept; --force to redo)");
        return Ok(());
    }
    println!(
        "sweep: {} seed(s){}{}",
        seeds.len(),
        if opts.prose_only {
            " (prose-marked only)"
        } else {
            ""
        },
        if opts.dry_run { " [DRY RUN]" } else { "" }
    );

    let mut edges = 0usize;
    let mut queued = 0usize;
    let mut errors = 0usize;
    for (i, seed) in seeds.iter().enumerate() {
        let outcome = if opts.dry_run {
            sweep_dry_run_one(conn, seed, &cfg)
        } else {
            sweep_one(conn, seed, &cfg)
        };
        match outcome {
            Ok(summary) => {
                edges += summary.matches("-edge").count();
                queued += summary.matches(queue_label(opts.dry_run)).count();
                if !summary.is_empty() {
                    println!("[{}/{}] {seed}: {summary}", i + 1, seeds.len());
                }
                if !opts.dry_run {
                    conn.execute(
                        "INSERT OR REPLACE INTO sweep_state (seed_id, swept_at, outcome)
                         VALUES (?1, ?2, ?3)",
                        params![seed, crate::memory::now(), summary],
                    )?;
                }
            }
            Err(e) => {
                errors += 1;
                log::warn!("sweep: seed {seed} failed (continuing): {e:#}");
                // Deliberately NOT recorded as swept — a transient LLM
                // failure should retry on the next run.
            }
        }
    }
    println!(
        "sweep done: {} seeds, {} edge(s), {} queued, {} error(s)",
        seeds.len(),
        edges,
        queued,
        errors
    );
    Ok(())
}

/// Live sweep for one seed: the adjudication pipeline with the
/// updates-queue-only calibration (see apply_verdicts).
fn sweep_one(conn: &Connection, seed: &str, cfg: &AdjudicateConfig) -> Result<String> {
    let seed_mem = load_candidate(conn, seed)?;
    let candidates = nominate(conn, seed)?;
    if candidates.is_empty() {
        return Ok(String::new());
    }
    let raw = call_llm(cfg, &build_prompt(&seed_mem, &candidates))?;
    let verdicts = parse_verdicts(&raw)?;
    apply_verdicts(conn, seed, &candidates, &verdicts, cfg, true)
}

/// The queue-action label each sweep mode emits in its summary rows.
/// gf2.13: dry-run rows say "would-queue", live rows say "queued" —
/// the tally counted only "queued", so every dry run reported 0
/// queued. Count the label the mode actually prints.
fn queue_label(dry_run: bool) -> &'static str {
    if dry_run {
        "would-queue"
    } else {
        "queued"
    }
}

/// Dry-run variant: same nominate → classify, but verdicts are PRINTED
/// as proposals instead of applied, and nothing is recorded as swept.
fn sweep_dry_run_one(conn: &Connection, seed: &str, cfg: &AdjudicateConfig) -> Result<String> {
    let seed_mem = load_candidate(conn, seed)?;
    let candidates = nominate(conn, seed)?;
    if candidates.is_empty() {
        return Ok(String::new());
    }
    let raw = call_llm(cfg, &build_prompt(&seed_mem, &candidates))?;
    let verdicts = parse_verdicts(&raw)?;
    Ok(format_dry_run_proposals(&candidates, &verdicts, cfg))
}

/// Verdicts → proposal rows ("would-edge" / "would-queue"), mirroring
/// apply_verdicts' thresholds without writing anything.
fn format_dry_run_proposals(
    candidates: &[Candidate],
    verdicts: &[Verdict],
    cfg: &AdjudicateConfig,
) -> String {
    let mut out: Vec<String> = Vec::new();
    for v in verdicts {
        if v.relation == "unrelated" {
            continue;
        }
        if let Some(c) = v.candidate.checked_sub(1).and_then(|i| candidates.get(i)) {
            let action = match v.relation.as_str() {
                "corrects" if v.confidence >= cfg.auto_threshold => "would-edge",
                "corrects" | "updates" | "duplicate" if v.confidence >= cfg.queue_threshold => {
                    "would-queue"
                }
                _ => continue,
            };
            out.push(format!(
                "{action} {} ({}, conf {:.2}: {})",
                c.id, v.relation, v.confidence, v.rationale
            ));
        }
    }
    out.join("; ")
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
        let summary = apply_verdicts(
            &conn,
            "fresh",
            &[cand(&conn, "stale")],
            &verdicts,
            &cfg(),
            false,
        )
        .unwrap();
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
        apply_verdicts(
            &conn,
            "new",
            &[cand(&conn, "old")],
            &verdicts,
            &cfg(),
            false,
        )
        .unwrap();
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
        let summary = apply_verdicts(
            &conn,
            "zombie",
            &[cand(&conn, "tomb")],
            &verdicts,
            &cfg(),
            false,
        )
        .unwrap();
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

    /// gf2.9 dry-run finding, pinned: in sweep mode (updates_queue_only),
    /// even a 0.95-confidence 'updates' must queue, never auto-edge —
    /// the model conflates topic-adjacency with supersession on the
    /// backlog. 'corrects' still auto-edges.
    #[test]
    fn sweep_mode_queues_updates_and_edges_corrects() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "hist", "dated-status", "status as of april");
        seed(&conn, "wrong", "false-claim", "the test passed");
        seed(&conn, "seedm", "newer", "current state + correction");
        let verdicts = vec![
            Verdict {
                candidate: 1,
                relation: "updates".into(),
                confidence: 0.95,
                rationale: "newer status".into(),
            },
            Verdict {
                candidate: 2,
                relation: "corrects".into(),
                confidence: 0.95,
                rationale: "the test never passed".into(),
            },
        ];
        let cands = [cand(&conn, "hist"), cand(&conn, "wrong")];
        let summary = apply_verdicts(&conn, "seedm", &cands, &verdicts, &cfg(), true).unwrap();
        assert!(summary.contains("hist queued (updates"), "{summary}");
        assert!(summary.contains("wrong corrects-edge"), "{summary}");
        let hist_lc: String = conn
            .query_row("SELECT lifecycle FROM memories WHERE id='hist'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            hist_lc, "active",
            "updates in sweep mode must not flip lifecycle"
        );
    }

    /// gf2.13: prose-marker seeding must be case-insensitive — the
    /// first live sweep matched exact case only and caught 45 of ~123
    /// prose-marked memories.
    #[test]
    fn prose_marker_seed_match_is_case_insensitive() {
        let conn = crate::db::open_in_memory().unwrap();
        // Case variants the exact-case match missed: lowercase where
        // the marker is "OBSOLETE", title-case where it is
        // "superseded", and a marker in the NAME column.
        seed(
            &conn,
            "m-lower",
            "note",
            "this claim is obsolete since the migration",
        );
        seed(&conn, "m-mixed", "plan", "Superseded by the 2026 design");
        seed(&conn, "m-name", "Corrected-path", "see the new layout");
        seed(
            &conn,
            "m-plain",
            "status",
            "routine note, nothing corrective",
        );
        let sql = format!(
            "SELECT id FROM memories WHERE {} ORDER BY id",
            prose_marker_clause()
        );
        let mut stmt = conn.prepare(&sql).unwrap();
        let ids: Vec<String> = stmt
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(ids, ["m-lower", "m-mixed", "m-name"]);
    }

    /// gf2.13: the sweep tally must count the label the mode actually
    /// emits — dry-run rows say "would-queue", and counting "queued"
    /// made every dry run report 0 queued.
    #[test]
    fn dry_run_tally_counts_would_queue_rows() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "hist", "dated-status", "status as of april");
        seed(&conn, "wrong", "false-claim", "the test passed");
        let verdicts = vec![
            Verdict {
                candidate: 1,
                relation: "updates".into(),
                confidence: 0.6,
                rationale: "newer status".into(),
            },
            Verdict {
                candidate: 2,
                relation: "corrects".into(),
                confidence: 0.95,
                rationale: "the test never passed".into(),
            },
        ];
        let cands = [cand(&conn, "hist"), cand(&conn, "wrong")];
        let summary = format_dry_run_proposals(&cands, &verdicts, &cfg());
        assert!(summary.contains("would-queue hist (updates"), "{summary}");
        assert!(summary.contains("would-edge wrong (corrects"), "{summary}");
        // The tally counts the dry-run label: nonzero, and would-edge
        // rows are not miscounted as queued.
        assert_eq!(summary.matches(queue_label(true)).count(), 1);
        // Dry-run rows never carry the live label — the two tallies
        // cannot double-count each other.
        assert_eq!(summary.matches(queue_label(false)).count(), 0);
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
