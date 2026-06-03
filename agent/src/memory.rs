use crate::embed;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Args, Subcommand};
use rusqlite::{params, params_from_iter, types::Value, Connection};
use std::collections::HashMap;
use std::io::Read;
use std::path::PathBuf;
use uuid::Uuid;

#[derive(Args)]
pub struct MemoryCmd {
    #[command(subcommand)]
    pub action: MemoryAction,
}

#[derive(Subcommand)]
pub enum MemoryAction {
    /// Add a new memory
    Add(AddArgs),
    /// Update an existing memory
    Update(UpdateArgs),
    /// Full-text search memories
    Search(SearchArgs),
    /// Most recently updated memories
    Recent(RecentArgs),
    /// List memories filtered by type/tag/cwd
    List(ListArgs),
    /// Output relevant memories for session start (hook use)
    Context(ContextArgs),
    /// Show recent context call log (for tuning)
    ContextStats(ContextStatsArgs),
    /// Rebuild the topic index for all active memories
    RebuildIndex,
    /// Semantic recall via embedding similarity
    Recall(RecallArgs),
    /// (Re-)embed memories using the configured embedder
    Reindex(ReindexArgs),
    /// Export all active memories as markdown
    Export,
    /// Import existing markdown memory files into the database
    Migrate(MigrateArgs),
}

#[derive(Args)]
pub struct AddArgs {
    #[arg(long)]
    pub r#type: String,
    #[arg(long)]
    pub name: String,
    #[arg(long)]
    pub description: String,
    /// Memory body. Pass "-" to read from stdin. For large/free-text content
    /// prefer --content-file to avoid shell-quoting the body on the command line.
    #[arg(long)]
    pub content: Option<String>,
    /// Read the memory body from this file (takes precedence over --content).
    #[arg(long)]
    pub content_file: Option<PathBuf>,
    #[arg(long, default_value = "")]
    pub tags: String,
    #[arg(long, default_value = "")]
    pub cwd: String,
    #[arg(long, default_value = "curated")]
    pub source: String,
}

#[derive(Args)]
pub struct UpdateArgs {
    pub id: String,
    #[arg(long)]
    pub name: Option<String>,
    #[arg(long)]
    pub description: Option<String>,
    #[arg(long)]
    pub content: Option<String>,
    /// Read replacement body from this file (takes precedence over --content;
    /// "-" on --content reads stdin).
    #[arg(long)]
    pub content_file: Option<PathBuf>,
    #[arg(long)]
    pub tags: Option<String>,
    #[arg(long)]
    pub active: Option<bool>,
}

#[derive(Args)]
pub struct SearchArgs {
    pub query: String,
    #[arg(long)]
    pub r#type: Option<String>,
    #[arg(long, default_value = "10")]
    pub limit: usize,
}

#[derive(Args)]
pub struct RecentArgs {
    #[arg(long, default_value = "10")]
    pub n: usize,
    #[arg(long)]
    pub r#type: Option<String>,
}

#[derive(Args)]
pub struct ListArgs {
    #[arg(long)]
    pub r#type: Option<String>,
    #[arg(long)]
    pub tag: Option<String>,
    #[arg(long)]
    pub cwd: Option<String>,
    #[arg(long, default_value = "50")]
    pub limit: usize,
}

#[derive(Args)]
pub struct ContextArgs {
    #[arg(long, default_value = "")]
    pub cwd: String,
    /// Extra space-separated signal terms to boost retrieval
    #[arg(long, default_value = "")]
    pub signals: String,
    /// Limit per category (project/reference)
    #[arg(long, default_value = "5")]
    pub limit: usize,
    /// Print scoring detail to stderr for tuning
    #[arg(long)]
    pub verbose: bool,
}

#[derive(Args)]
pub struct ContextStatsArgs {
    /// Number of recent context calls to show
    #[arg(long, default_value = "10")]
    pub n: usize,
}

#[derive(Args)]
pub struct MigrateArgs {
    /// Directory containing markdown memory files
    #[arg(long)]
    pub dir: Option<PathBuf>,
    /// Dry run — print what would be imported without writing
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Args)]
pub struct RecallArgs {
    /// Free-form query — semantically nearest memories are returned
    pub query: String,
    /// Number of results to return
    #[arg(long, default_value = "5")]
    pub k: usize,
    /// Restrict to a memory type (user/feedback/project/reference)
    #[arg(long)]
    pub r#type: Option<String>,
    /// Also run the FTS5 lexical search for side-by-side comparison
    #[arg(long)]
    pub compare: bool,
}

#[derive(Args)]
pub struct ReindexArgs {
    /// Only embed memories that are missing an embedding for the active model
    #[arg(long)]
    pub missing: bool,
    /// Batch size for embedding calls
    #[arg(long, default_value = "16")]
    pub batch: usize,
}

struct Memory {
    id: String,
    type_: String,
    name: String,
    description: String,
    content: String,
    source: String,
    tags: String,
    cwd: String,
    is_active: bool,
    created_at: i64,
    updated_at: i64,
}

fn short_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

fn now() -> i64 {
    Utc::now().timestamp()
}

pub fn run(conn: Connection, cmd: MemoryCmd) -> Result<()> {
    match cmd.action {
        MemoryAction::Add(args) => add(&conn, args),
        MemoryAction::Update(args) => update(&conn, args),
        MemoryAction::Search(args) => search(&conn, args),
        MemoryAction::Recent(args) => recent(&conn, args),
        MemoryAction::List(args) => list(&conn, args),
        MemoryAction::Context(args) => context(&conn, args),
        MemoryAction::ContextStats(args) => context_stats(&conn, args),
        MemoryAction::RebuildIndex => rebuild_full_index(&conn),
        MemoryAction::Recall(args) => recall(&conn, args),
        MemoryAction::Reindex(args) => reindex(&conn, args),
        MemoryAction::Export => export(&conn),
        MemoryAction::Migrate(args) => migrate(&conn, args),
    }
}

// ── Tokenizer ────────────────────────────────────────────────────────────────

const STOPWORDS: &[&str] = &[
    "the", "and", "for", "with", "from", "that", "this", "have", "been", "were", "they", "their",
    "what", "when", "where", "which", "will", "your", "about", "into", "through", "before",
    "after", "above", "below", "between", "each", "more", "also", "than", "then", "some", "other",
    "such", "only", "same", "both", "over", "here", "there", "just", "used", "using", "use", "via",
    "per", "can", "has", "not", "all", "but", "are", "was", "its", "our", "you", "her", "him",
    "his", "she", "they", "who", "src", "home", "tcovert",
];

fn tokenize(text: &str) -> Vec<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 2)
        .map(|w| w.to_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

fn signals_from_cwd(cwd: &str) -> Vec<String> {
    // Extract terms from the last two path components so
    // e.g. /home/tcovert/src/pi-claude-poc → ["pi", "claude", "poc", "src"]
    let parts: Vec<&str> = cwd.trim_end_matches('/').split('/').collect();
    let tail = parts
        .iter()
        .rev()
        .take(2)
        .copied()
        .collect::<Vec<_>>()
        .join("-");
    tokenize(&tail)
}

// ── Topic index ───────────────────────────────────────────────────────────────

fn rebuild_index_for(conn: &Connection, memory_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_topic_index WHERE memory_id = ?1",
        params![memory_id],
    )?;

    let m: Option<Memory> = {
        let mut stmt = conn.prepare(
            "SELECT id, type, name, description, content, source, tags, cwd,
                    is_active, created_at, updated_at
             FROM memories WHERE id = ?1 AND is_active = 1",
        )?;
        let mut rows = stmt.query_map(params![memory_id], row_to_memory)?;
        rows.next().transpose()?
    };

    let m = match m {
        Some(m) => m,
        None => return Ok(()), // inactive or gone — index entries already deleted
    };

    let mut term_weights: HashMap<String, f64> = HashMap::new();

    for t in tokenize(&m.tags) {
        *term_weights.entry(t).or_default() += 3.0;
    }
    for t in tokenize(&m.name) {
        *term_weights.entry(t).or_default() += 2.0;
    }
    for t in tokenize(&m.description) {
        *term_weights.entry(t).or_default() += 1.5;
    }
    let preview: String = m.content.chars().take(300).collect();
    for t in tokenize(&preview) {
        *term_weights.entry(t).or_default() += 1.0;
    }
    if !m.cwd.is_empty() {
        for t in signals_from_cwd(&m.cwd) {
            *term_weights.entry(t).or_default() += 2.0;
        }
    }

    for (term, weight) in term_weights {
        conn.execute(
            "INSERT OR REPLACE INTO memory_topic_index (term, memory_id, weight)
             VALUES (?1, ?2, ?3)",
            params![term, memory_id, weight],
        )?;
    }

    Ok(())
}

fn rebuild_full_index(conn: &Connection) -> Result<()> {
    conn.execute_batch("DELETE FROM memory_topic_index")?;
    let ids: Vec<String> = {
        let mut stmt = conn.prepare("SELECT id FROM memories WHERE is_active = 1")?;
        let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let n = ids.len();
    for id in &ids {
        rebuild_index_for(conn, id)?;
    }
    eprintln!("rebuilt index: {n} memories");
    Ok(())
}

// ── Scored retrieval ──────────────────────────────────────────────────────────

fn score_context_memories(
    conn: &Connection,
    signal_terms: &[String],
    type_: &str,
    limit: usize,
) -> Result<Vec<(Memory, f64)>> {
    let now_ts = now();

    if signal_terms.is_empty() {
        // No signals — fall back to pure recency
        let memories = query_by_type(conn, type_, limit)?;
        return Ok(memories
            .into_iter()
            .map(|m| {
                let days = ((now_ts - m.updated_at) as f64 / 86400.0).max(0.0);
                let score = 1.0 / (1.0 + days.ln_1p());
                (m, score)
            })
            .collect());
    }

    // Build: ?1=type, ?2=fetch_limit, ?3..=terms
    let placeholders = (3..=signal_terms.len() + 2)
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(
        "SELECT m.id, m.type, m.name, m.description, m.content, m.source,
                m.tags, m.cwd, m.is_active, m.created_at, m.updated_at,
                SUM(mti.weight) AS raw_score
         FROM memories m
         JOIN memory_topic_index mti ON mti.memory_id = m.id
         WHERE m.type = ?1 AND m.is_active = 1
           AND mti.term IN ({placeholders})
         GROUP BY m.id
         ORDER BY raw_score DESC
         LIMIT ?2"
    );

    let mut dyn_params: Vec<Value> = vec![
        Value::Text(type_.to_string()),
        Value::Integer((limit * 4) as i64), // fetch extra, re-rank after decay
    ];
    for t in signal_terms {
        dyn_params.push(Value::Text(t.clone()));
    }

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(dyn_params.iter()), |r| {
        Ok((row_to_memory(r)?, r.get::<_, f64>(11)?))
    })?;

    let mut scored: Vec<(Memory, f64)> = rows
        .map(|r| {
            let (m, raw) = r?;
            let days = ((now_ts - m.updated_at) as f64 / 86400.0).max(0.0);
            let recency = 1.0 / (1.0 + days.ln_1p());
            Ok((m, raw * recency))
        })
        .collect::<Result<_>>()?;

    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(limit);
    Ok(scored)
}

// ── Context log ───────────────────────────────────────────────────────────────

fn log_context_call(
    conn: &Connection,
    cwd: &str,
    signals: &[String],
    n_scored: usize,
    returned: &[(String, String, f64)], // (id, name, score)
) -> Result<()> {
    let returned_json = serde_json::to_string(
        &returned
            .iter()
            .map(|(id, name, score)| serde_json::json!({"id": id, "name": name, "score": score}))
            .collect::<Vec<_>>(),
    )?;
    conn.execute(
        "INSERT INTO memory_context_log (created_at, cwd, signals, n_scored, returned)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![
            now(),
            cwd,
            signals.join(" "),
            n_scored as i64,
            returned_json
        ],
    )?;
    Ok(())
}

// ── CRUD ──────────────────────────────────────────────────────────────────────

/// Resolve a memory body from the three input modes. Precedence:
///   --content-file <path>  >  --content -  (stdin)  >  --content <inline>.
/// Returns None only when none was supplied.
fn resolve_content(
    content: Option<String>,
    content_file: Option<PathBuf>,
) -> Result<Option<String>> {
    if let Some(path) = content_file {
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading --content-file {}", path.display()))?;
        return Ok(Some(body));
    }
    if content.as_deref() == Some("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading content from stdin")?;
        return Ok(Some(buf));
    }
    Ok(content)
}

fn add(conn: &Connection, args: AddArgs) -> Result<()> {
    let valid_types = ["user", "feedback", "project", "reference"];
    if !valid_types.contains(&args.r#type.as_str()) {
        bail!("type must be one of: {}", valid_types.join(", "));
    }
    let content = match resolve_content(args.content, args.content_file)? {
        Some(c) => c,
        None => bail!(
            "content required: pass --content <text>, --content-file <path>, or --content - to read stdin"
        ),
    };
    let id = short_id();
    let ts = now();
    conn.execute(
        "INSERT INTO memories (id, type, name, description, content, source, tags, cwd, is_active, created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?9)",
        params![id, args.r#type, args.name, args.description, content, args.source, args.tags, args.cwd, ts],
    )?;
    rebuild_index_for(conn, &id)?;
    let text = embed::memory_embed_text(&args.name, &args.description, &content);
    embed::try_embed_one(conn, &id, &text);
    println!("{id}");
    Ok(())
}

fn update(conn: &Connection, args: UpdateArgs) -> Result<()> {
    let ts = now();
    let mut updated = 0;
    let content = resolve_content(args.content, args.content_file)?;

    if let Some(name) = args.name {
        updated += conn.execute(
            "UPDATE memories SET name=?1, updated_at=?2 WHERE id=?3",
            params![name, ts, args.id],
        )?;
    }
    if let Some(desc) = args.description {
        updated += conn.execute(
            "UPDATE memories SET description=?1, updated_at=?2 WHERE id=?3",
            params![desc, ts, args.id],
        )?;
    }
    if let Some(content) = content {
        updated += conn.execute(
            "UPDATE memories SET content=?1, updated_at=?2 WHERE id=?3",
            params![content, ts, args.id],
        )?;
    }
    if let Some(tags) = args.tags {
        updated += conn.execute(
            "UPDATE memories SET tags=?1, updated_at=?2 WHERE id=?3",
            params![tags, ts, args.id],
        )?;
    }
    if let Some(active) = args.active {
        updated += conn.execute(
            "UPDATE memories SET is_active=?1, updated_at=?2 WHERE id=?3",
            params![active as i64, ts, args.id],
        )?;
    }

    if updated == 0 {
        bail!("no memory found with id={}", args.id);
    }

    rebuild_index_for(conn, &args.id)?;

    // Re-embed from the post-update state
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT name, description, content FROM memories WHERE id = ?1 AND is_active = 1",
            params![args.id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some((name, desc, content)) = row {
        let text = embed::memory_embed_text(&name, &desc, &content);
        embed::try_embed_one(conn, &args.id, &text);
    }

    Ok(())
}

// ── Queries ───────────────────────────────────────────────────────────────────

fn search(conn: &Connection, args: SearchArgs) -> Result<()> {
    let sql = "SELECT m.id, m.type, m.name, m.description, m.content, m.tags, m.updated_at
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ?1 AND m.is_active = 1
           AND (?2 IS NULL OR m.type = ?2)
         ORDER BY rank
         LIMIT ?3";

    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![args.query, args.r#type, args.limit as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;

    for row in rows {
        let (id, type_, name, desc, content, tags, updated_at) = row?;
        let ts = fmt_ts(updated_at);
        println!("[{id}] ({type_}) {name} — {desc}  [{ts}]");
        if !tags.is_empty() {
            println!("  tags: {tags}");
        }
        println!("{}", indent(&content, "  "));
        println!();
    }
    Ok(())
}

fn recent(conn: &Connection, args: RecentArgs) -> Result<()> {
    let sql = "SELECT id, type, name, description, updated_at FROM memories
         WHERE is_active = 1 AND (?1 IS NULL OR type = ?1)
         ORDER BY updated_at DESC LIMIT ?2";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![args.r#type, args.n as i64], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
        ))
    })?;
    for row in rows {
        let (id, type_, name, desc, updated_at) = row?;
        println!("[{id}] ({type_}) {name} — {desc}  [{}]", fmt_ts(updated_at));
    }
    Ok(())
}

fn list(conn: &Connection, args: ListArgs) -> Result<()> {
    let cwd_pat = args.cwd.as_ref().map(|c| {
        let e = c
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("%{e}%")
    });
    let tag_pat = args.tag.as_ref().map(|t| {
        let e = t
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("%{e}%")
    });
    let sql = "SELECT id, type, name, description, tags, updated_at FROM memories
         WHERE is_active = 1
           AND (?1 IS NULL OR type = ?1)
           AND (?2 IS NULL OR cwd LIKE ?2 ESCAPE '\\')
           AND (?3 IS NULL OR tags LIKE ?3 ESCAPE '\\')
         ORDER BY updated_at DESC LIMIT ?4";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![args.r#type, cwd_pat, tag_pat, args.limit as i64],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
            ))
        },
    )?;
    for row in rows {
        let (id, type_, name, desc, tags, updated_at) = row?;
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags)
        };
        println!(
            "[{id}] ({type_}) {name}{tag_str} — {desc}  [{}]",
            fmt_ts(updated_at)
        );
    }
    Ok(())
}

// ── Context ───────────────────────────────────────────────────────────────────

fn context(conn: &Connection, args: ContextArgs) -> Result<()> {
    // Build signal term set
    let mut signal_terms = signals_from_cwd(&args.cwd);
    if !args.signals.is_empty() {
        signal_terms.extend(tokenize(&args.signals));
    }
    signal_terms.sort();
    signal_terms.dedup();

    // feedback and user: always all-active, no scoring needed
    let feedback = query_by_type(conn, "feedback", 20)?;
    let user = query_by_type(conn, "user", 5)?;

    // project and reference: topic-scored
    let project_limit = args.limit.max(5);
    let ref_limit = (args.limit / 2).max(3);

    let scored_project = score_context_memories(conn, &signal_terms, "project", project_limit)?;
    let scored_reference = score_context_memories(conn, &signal_terms, "reference", ref_limit)?;

    if args.verbose {
        eprintln!("[context] signals: {}", signal_terms.join(", "));
        eprintln!("[context] project ({} returned):", scored_project.len());
        for (m, s) in &scored_project {
            eprintln!("  {:.2}  {} — {}", s, m.id, m.description);
        }
        eprintln!("[context] reference ({} returned):", scored_reference.len());
        for (m, s) in &scored_reference {
            eprintln!("  {:.2}  {} — {}", s, m.id, m.description);
        }
    }

    if feedback.is_empty()
        && user.is_empty()
        && scored_project.is_empty()
        && scored_reference.is_empty()
    {
        return Ok(());
    }

    // Log this call for tuning
    let n_scored = scored_project.len() + scored_reference.len();
    let mut returned_log: Vec<(String, String, f64)> = Vec::new();
    for (m, s) in &scored_project {
        returned_log.push((m.id.clone(), m.name.clone(), *s));
    }
    for (m, s) in &scored_reference {
        returned_log.push((m.id.clone(), m.name.clone(), *s));
    }
    // Non-fatal — don't let a log write break context output
    let _ = log_context_call(conn, &args.cwd, &signal_terms, n_scored, &returned_log);

    println!("## Active Memory Context\n");

    if !feedback.is_empty() {
        println!("### Behavioral Rules (Feedback)\n");
        for m in &feedback {
            println!("**{}**: {}", m.name, m.description);
            println!("{}\n", m.content);
        }
    }

    if !user.is_empty() {
        println!("### User Profile\n");
        for m in &user {
            println!("{}\n", m.content);
        }
    }

    if !scored_project.is_empty() {
        println!("### Project Context\n");
        for (m, _) in &scored_project {
            println!("**{}**: {}", m.name, m.description);
            println!("{}\n", m.content);
        }
    }

    if !scored_reference.is_empty() {
        println!("### References\n");
        for (m, _) in &scored_reference {
            println!("**{}**: {}\n", m.name, m.content);
        }
    }

    Ok(())
}

fn context_stats(conn: &Connection, args: ContextStatsArgs) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, created_at, cwd, signals, n_scored, returned
         FROM memory_context_log
         ORDER BY created_at DESC
         LIMIT ?1",
    )?;
    let rows = stmt.query_map(params![args.n as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;

    for row in rows {
        let (id, created_at, cwd, signals, n_scored, returned_json) = row?;
        let cwd_short = cwd.split('/').last().unwrap_or(&cwd).to_string();
        println!(
            "#{id}  {}  cwd={}  signals=[{}]  scored={}",
            fmt_ts(created_at),
            cwd_short,
            signals,
            n_scored
        );

        if let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&returned_json) {
            for e in &entries {
                let name = e["name"].as_str().unwrap_or("?");
                let score = e["score"].as_f64().unwrap_or(0.0);
                println!("  {score:.2}  {name}");
            }
        }
        println!();
    }
    Ok(())
}

// ── Export / migrate ──────────────────────────────────────────────────────────

fn export(conn: &Connection) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, type, name, description, content, tags FROM memories
         WHERE is_active = 1 ORDER BY type, updated_at DESC",
    )?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;
    for row in rows {
        let (id, type_, name, desc, content, tags) = row?;
        println!("---");
        println!("id: {id}");
        println!("name: {name}");
        println!("description: {desc}");
        println!("type: {type_}");
        if !tags.is_empty() {
            println!("tags: {tags}");
        }
        println!("---\n");
        println!("{content}\n");
    }
    Ok(())
}

fn migrate(conn: &Connection, args: MigrateArgs) -> Result<()> {
    let dir = args.dir.unwrap_or_else(|| {
        let mut p = std::env::var("HOME").map(PathBuf::from).unwrap_or_default();
        p.push(".claude-personal/projects/-home-tcovert/memory");
        p
    });

    let entries: Vec<_> = std::fs::read_dir(&dir)?
        .filter_map(|e| e.ok())
        .filter(|e| {
            e.path().extension().map(|x| x == "md").unwrap_or(false) && e.file_name() != "MEMORY.md"
        })
        .collect();

    let mut imported = 0;
    let mut skipped = 0;

    for entry in entries {
        let path = entry.path();
        let raw = std::fs::read_to_string(&path)?;
        match parse_frontmatter(&raw) {
            Some((type_, name, description, content)) => {
                let stem = path.file_stem().unwrap_or_default().to_string_lossy();
                let id = stem.as_ref();
                let ts = now();

                if args.dry_run {
                    println!("would import: [{id}] ({type_}) {name}");
                    imported += 1;
                    continue;
                }

                let exists: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM memories WHERE id = ?1)",
                    [id],
                    |r| r.get(0),
                )?;

                if exists {
                    skipped += 1;
                    continue;
                }

                conn.execute(
                    "INSERT INTO memories (id, type, name, description, content, source, tags, cwd, is_active, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, 'curated', '', '', 1, ?6, ?6)",
                    params![id, type_, name, description, content, ts],
                )?;
                rebuild_index_for(conn, id)?;
                println!("imported: [{id}] ({type_}) {name}");
                imported += 1;
            }
            None => {
                eprintln!("skipping (no valid frontmatter): {}", path.display());
                skipped += 1;
            }
        }
    }

    eprintln!("done: {imported} imported, {skipped} skipped");
    Ok(())
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn query_by_type(conn: &Connection, type_: &str, limit: usize) -> Result<Vec<Memory>> {
    let mut stmt = conn.prepare(
        "SELECT id, type, name, description, content, source, tags, cwd, is_active, created_at, updated_at
         FROM memories WHERE type = ?1 AND is_active = 1 ORDER BY updated_at DESC LIMIT ?2",
    )?;
    let mapped = stmt.query_map(params![type_, limit as i64], row_to_memory)?;
    collect_memories(mapped)
}

fn row_to_memory(r: &rusqlite::Row) -> rusqlite::Result<Memory> {
    Ok(Memory {
        id: r.get(0)?,
        type_: r.get(1)?,
        name: r.get(2)?,
        description: r.get(3)?,
        content: r.get(4)?,
        source: r.get(5)?,
        tags: r.get(6)?,
        cwd: r.get(7)?,
        is_active: r.get::<_, i64>(8)? != 0,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

fn collect_memories<I>(iter: I) -> Result<Vec<Memory>>
where
    I: Iterator<Item = rusqlite::Result<Memory>>,
{
    iter.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn fmt_ts(ts: i64) -> String {
    chrono::DateTime::from_timestamp(ts, 0)
        .map(|d| d.format("%Y-%m-%d").to_string())
        .unwrap_or_default()
}

fn parse_frontmatter(raw: &str) -> Option<(String, String, String, String)> {
    let raw = raw.trim_start();
    let rest = raw.strip_prefix("---")?.trim_start_matches('\n');
    let end = rest.find("\n---")?;
    let front = &rest[..end];
    let after_close = rest[end + 4..].trim_start_matches('\n');
    let body = after_close;

    let mut type_ = String::new();
    let mut name = String::new();
    let mut description = String::new();

    for line in front.lines() {
        if let Some(v) = line.strip_prefix("type:") {
            type_ = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().to_string();
        }
    }

    if type_.is_empty() || name.is_empty() || description.is_empty() {
        return None;
    }

    Some((type_, name, description, body.to_string()))
}

fn indent(s: &str, prefix: &str) -> String {
    s.lines()
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

// ── Embedding-based recall ───────────────────────────────────────────────────

fn recall(conn: &Connection, args: RecallArgs) -> Result<()> {
    let embedder = embed::select_embedder();
    let model = embedder.model_id().to_string();

    let query_vec = embedder
        .embed(&[args.query.clone()])?
        .into_iter()
        .next()
        .ok_or_else(|| anyhow::anyhow!("embedder returned no vector"))?;

    let sql = "SELECT m.id, m.type, m.name, m.description, m.updated_at, e.vector
               FROM memory_embeddings e
               JOIN memories m ON m.id = e.memory_id
               WHERE m.is_active = 1 AND e.model = ?1
                 AND (?2 IS NULL OR m.type = ?2)";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![model, args.r#type], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Vec<u8>>(5)?,
        ))
    })?;

    let mut scored: Vec<(String, String, String, String, i64, f32)> = Vec::new();
    for row in rows {
        let (id, type_, name, desc, updated_at, blob) = row?;
        let v = embed::blob_to_f32(&blob);
        let sim = embed::cosine(&query_vec, &v);
        scored.push((id, type_, name, desc, updated_at, sim));
    }

    if scored.is_empty() {
        eprintln!("no embeddings found for model '{model}'. Run `agent memory reindex` first.");
        return Ok(());
    }

    scored.sort_by(|a, b| b.5.partial_cmp(&a.5).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(args.k);

    println!("## Semantic recall (model: {})\n", model);
    for (id, type_, name, desc, updated_at, sim) in &scored {
        println!(
            "[{sim:.3}] [{id}] ({type_}) {name} — {desc}  [{}]",
            fmt_ts(*updated_at)
        );
    }

    if args.compare {
        println!("\n## FTS5 lexical comparison\n");
        search(
            conn,
            SearchArgs {
                query: args.query.clone(),
                r#type: args.r#type.clone(),
                limit: args.k,
            },
        )?;
    }

    Ok(())
}

fn reindex(conn: &Connection, args: ReindexArgs) -> Result<()> {
    let embedder = embed::select_embedder();
    let model = embedder.model_id().to_string();

    let row_to_tuple = |r: &rusqlite::Row| -> rusqlite::Result<(String, String, String, String)> {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    };

    let rows: Vec<(String, String, String, String)> = if args.missing {
        let mut stmt = conn.prepare(
            "SELECT m.id, m.name, m.description, m.content
             FROM memories m
             LEFT JOIN memory_embeddings e
               ON e.memory_id = m.id AND e.model = ?1
             WHERE m.is_active = 1 AND e.memory_id IS NULL",
        )?;
        let out: rusqlite::Result<Vec<_>> = stmt.query_map(params![model], row_to_tuple)?.collect();
        out?
    } else {
        let mut stmt = conn.prepare(
            "SELECT id, name, description, content
             FROM memories
             WHERE is_active = 1",
        )?;
        let out: rusqlite::Result<Vec<_>> = stmt.query_map([], row_to_tuple)?.collect();
        out?
    };

    let total = rows.len();
    if total == 0 {
        eprintln!("nothing to embed (model={model})");
        return Ok(());
    }
    eprintln!(
        "embedding {total} memories with {model} (batch={})...",
        args.batch
    );

    let batch = args.batch.max(1);
    let mut done = 0usize;
    let mut failed = 0usize;
    for chunk in rows.chunks(batch) {
        let texts: Vec<String> = chunk
            .iter()
            .map(|(_, n, d, c)| embed::memory_embed_text(n, d, c))
            .collect();

        let vectors = match embedder.embed(&texts) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("  batch failed: {e}");
                failed += chunk.len();
                continue;
            }
        };

        let now_ts = now();
        for ((id, name, desc, content), vec) in chunk.iter().zip(vectors.iter()) {
            let text = embed::memory_embed_text(name, desc, content);
            let hash = embed::content_hash(&text);
            let blob = embed::f32_to_blob(vec);
            conn.execute(
                "INSERT OR REPLACE INTO memory_embeddings
                 (memory_id, model, dims, content_hash, embedded_at, vector)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![id, &model, vec.len() as i64, hash, now_ts, blob],
            )?;
            done += 1;
        }
        eprintln!("  {done}/{total} done");
    }

    eprintln!("reindex complete: {done} embedded, {failed} failed");
    Ok(())
}
