//! `agent kx` — the knowledge-index subcommand (PR #1: core + config + storage).
//!
//! A `kx` entry is a POINTER (+ one-line summary + metadata + embedding) to one
//! document in a configured corpus; the documents themselves stay as files on
//! disk. `kx ingest` scans the `[[kx.sources]]` configured in
//! `~/.config/agent/config.toml`, storing one `memories` row per document with
//! `type = 'kx'`; `kx recall` / `kx search` / `kx list` query that corpus.
//!
//! Storage and retrieval REUSE the `memory` machinery rather than duplicating
//! it: rows go in the same `memories` table (embeddings + FTS come for free),
//! and retrieval routes through [`memory::semantic_recall`] / [`memory::search_rows`]
//! parameterized by a [`memory::TypeFilter`] pinned to `kx` — the generic memory
//! paths pin the *opposite* filter so the kx corpus never pollutes personal
//! recall. Graph/lineage and model-summarized ingest are PR #2.

use crate::embed;
use crate::memory::{self, ScopeFilter, TypeFilter};
use anyhow::{anyhow, bail, Context, Result};
use chrono::NaiveDate;
use clap::{Args, Subcommand};
use rusqlite::{params, types::Value, Connection, OptionalExtension};
use serde::Deserialize;
use std::path::{Path, PathBuf};

/// The `memories.type` value every kx row carries.
const KX_TYPE: &str = "kx";
/// Built-in default cosine floor for `kx recall` (overridable via `[kx].min_score`).
const BUILTIN_MIN_SCORE: f32 = 0.60;
/// Built-in default hit cap (overridable via `[kx].max_hits`).
const BUILTIN_MAX_HITS: usize = 4;
/// Built-in recursion cap for the ingest walk (overridable via `[kx].max_depth`).
/// The walk already skips symlinks (so it can't loop or follow a link off-tree);
/// this bounds *real* deep trees, nested mount points, and network shares so a
/// pathological corpus is slow-but-finite rather than runaway.
const BUILTIN_MAX_DEPTH: usize = 64;
/// Default row cap for `kx list` (a browse, not a ranked retrieval).
const LIST_DEFAULT_LIMIT: usize = 20;
/// Description length cap (clarification #14).
const DESC_MAX_CHARS: usize = 160;

// ── Config ───────────────────────────────────────────────────────────────────

/// The whole `~/.config/agent/config.toml`, of which only `[kx]` is modeled.
/// Unknown sections (`[openrouter]`, `[embed]`, …) are ignored by serde — we
/// never read, and never log, the secrets living in those sections.
#[derive(Debug, Default, Deserialize)]
pub struct RootConfig {
    #[serde(default)]
    pub kx: KxSection,
}

/// The `[kx]` section: global scalar knobs plus the `[[kx.sources]]` array.
#[derive(Debug, Deserialize)]
#[serde(default)]
pub struct KxSection {
    /// Scope for entries whose source does not set one (ownership axis).
    pub default_scope: String,
    /// PR #2 model-summarized ingest. Parsed for schema completeness; when
    /// `true`, `kx ingest` errors loudly rather than silently degrading (#3).
    pub summarize: bool,
    pub summarize_model: Option<String>,
    /// Defaults to `[embed].model`; PR #1 embeds via the shared embedder chain.
    pub embed_model: Option<String>,
    /// Cosine floor for `kx recall`.
    pub min_score: f32,
    /// Operational hit cap for recall/search.
    pub max_hits: usize,
    /// Max directory depth the ingest walk descends below each source root
    /// (files in the root are depth 0). Bounds deep/nested-mount/network trees.
    pub max_depth: usize,
    /// File extensions ingested when a source does not override them.
    pub default_extensions: Vec<String>,
    /// Globs excluded from every source (∪ per-source `exclude-globs`).
    pub default_exclude: Vec<String>,
    /// The corpora to scan.
    pub sources: Vec<KxSource>,
}

impl Default for KxSection {
    fn default() -> Self {
        Self {
            default_scope: "work".into(),
            summarize: false,
            summarize_model: None,
            embed_model: None,
            min_score: BUILTIN_MIN_SCORE,
            max_hits: BUILTIN_MAX_HITS,
            max_depth: BUILTIN_MAX_DEPTH,
            default_extensions: vec!["md".into()],
            default_exclude: default_exclude_globs(),
            sources: Vec::new(),
        }
    }
}

/// One configured corpus.
#[derive(Debug, Deserialize)]
pub struct KxSource {
    /// Root directory scanned recursively (there is no separate source "name"
    /// — `--source` matches on this path; clarification #11).
    pub path: String,
    /// Extensions to include; empty ⇒ the section's `default_extensions`.
    #[serde(default)]
    pub extensions: Vec<String>,
    /// Extra excludes, unioned with `default_exclude` (real glob matching).
    #[serde(default, rename = "exclude-globs")]
    pub exclude_globs: Vec<String>,
    /// Ownership scope; falls back to `default_scope`.
    #[serde(default)]
    pub scope: Option<String>,
    /// Tags applied to every entry, before `repo:`/`cat:` are added.
    #[serde(default)]
    pub tags: Vec<String>,
    /// Category → a `cat:<category>` tag.
    #[serde(default)]
    pub category: Option<String>,
}

/// The exact default exclude list (clarification #16).
fn default_exclude_globs() -> Vec<String> {
    [
        "**/.git/**",
        "**/.jj/**",
        "**/target/**",
        "**/node_modules/**",
        "**/.venv/**",
        "**/__pycache__/**",
        "*.log",
        "*.jsonl",
        "*.prompt.txt",
        "*.lock",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

/// Resolve the config path: explicit `--config`, else `~/.config/agent/config.toml`.
fn config_path(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(p) = explicit {
        return Ok(p.to_path_buf());
    }
    let home =
        std::env::var("HOME").context("HOME not set; cannot locate ~/.config/agent/config.toml")?;
    Ok(PathBuf::from(home).join(".config/agent/config.toml"))
}

/// Load the whole config for its `[kx]` section. Errors PROPAGATE (punch-list
/// #4): a missing file is an error (ingest needs sources), and a malformed file
/// surfaces a REDACTED error — never the raw TOML, which holds API keys.
fn load_config(explicit: Option<&Path>) -> Result<RootConfig> {
    let path = config_path(explicit)?;
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading kx config {}", path.display()))?;
    parse_config(&content, &path)
}

/// Like [`load_config`] but a MISSING default file yields `Ok(None)` so queries
/// fall back to built-in defaults. An explicit `--config` that is missing, and
/// any malformed file, still error.
fn load_config_lenient(explicit: Option<&Path>) -> Result<Option<RootConfig>> {
    let path = config_path(explicit)?;
    match std::fs::read_to_string(&path) {
        Ok(content) => Ok(Some(parse_config(&content, &path)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound && explicit.is_none() => Ok(None),
        Err(e) => Err(e).with_context(|| format!("reading kx config {}", path.display())),
    }
}

/// Parse TOML into [`RootConfig`], redacting parse errors so no file content
/// (and thus no secret) is ever echoed. Only the toml locator line is kept.
fn parse_config(content: &str, path: &Path) -> Result<RootConfig> {
    toml::from_str(content).map_err(|e| {
        let first = e
            .to_string()
            .lines()
            .next()
            .unwrap_or("invalid TOML")
            .to_string();
        anyhow!("kx config {}: {}", path.display(), first)
    })
}

/// `(min_score, max_hits)` from `[kx]`, or built-in defaults when the default
/// config file is simply absent. Malformed configs still error (punch-list #4).
fn resolve_query_knobs(explicit: Option<&Path>) -> Result<(f32, usize)> {
    match load_config_lenient(explicit)? {
        Some(cfg) => Ok((cfg.kx.min_score, cfg.kx.max_hits)),
        None => Ok((BUILTIN_MIN_SCORE, BUILTIN_MAX_HITS)),
    }
}

// ── CLI surface ──────────────────────────────────────────────────────────────

#[derive(Args)]
pub struct KxCmd {
    #[command(subcommand)]
    pub action: KxAction,
}

#[derive(Subcommand)]
pub enum KxAction {
    /// Ingest the configured [[kx.sources]] into the index (heuristic v1)
    Ingest(IngestArgs),
    /// Semantic recall over the kx corpus (embeddings + cosine, honors min_score)
    Recall(KxRecallArgs),
    /// FTS lexical search over the kx corpus
    Search(KxSearchArgs),
    /// Browse the kx corpus by tag / scope / recency
    List(KxListArgs),
}

#[derive(Args)]
pub struct IngestArgs {
    /// Ingest only this source (a configured path: exact, or its trailing component)
    #[arg(long)]
    pub source: Option<String>,
    /// Report what would change without writing anything
    #[arg(long)]
    pub dry_run: bool,
    /// Re-ingest even when a document's content hash is unchanged
    #[arg(long)]
    pub force: bool,
    /// Config file to read `[kx]` from (default: ~/.config/agent/config.toml)
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Emit a JSON report instead of human-readable text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct KxRecallArgs {
    /// Free-text query — semantically nearest kx entries are returned
    pub query: String,
    /// Restrict to a scope (work/personal/shared)
    #[arg(long)]
    pub scope: Option<String>,
    /// Require this tag (repeatable; ANDed against the entry's tags)
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    /// Only entries dated on/after this day (YYYY-MM-DD, inclusive)
    #[arg(long)]
    pub from: Option<String>,
    /// Only entries dated on/before this day (YYYY-MM-DD, inclusive whole day)
    #[arg(long)]
    pub to: Option<String>,
    /// Max hits (overrides [kx].max_hits)
    #[arg(long)]
    pub k: Option<usize>,
    /// Cosine floor (overrides [kx].min_score)
    #[arg(long = "min-score")]
    pub min_score: Option<f32>,
    /// Config file supplying the min_score/max_hits defaults
    #[arg(long)]
    pub config: Option<PathBuf>,
    /// Emit JSON instead of human-readable text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct KxSearchArgs {
    /// Free-text query — FTS5 lexical
    pub query: String,
    #[arg(long)]
    pub scope: Option<String>,
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long)]
    pub to: Option<String>,
    /// Max hits (overrides [kx].max_hits)
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub config: Option<PathBuf>,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct KxListArgs {
    #[arg(long)]
    pub scope: Option<String>,
    #[arg(long = "tag")]
    pub tags: Vec<String>,
    #[arg(long)]
    pub from: Option<String>,
    #[arg(long)]
    pub to: Option<String>,
    /// Max rows (default 20)
    #[arg(long)]
    pub limit: Option<usize>,
    #[arg(long)]
    pub json: bool,
}

pub fn run(conn: Connection, cmd: KxCmd) -> Result<()> {
    match cmd.action {
        KxAction::Ingest(a) => ingest(&conn, a),
        KxAction::Recall(a) => recall(&conn, a),
        KxAction::Search(a) => search(&conn, a),
        KxAction::List(a) => list(&conn, a),
    }
}

// ── Query filters (shared by recall / search / list) ─────────────────────────

/// The `--tag` / `--from` / `--to` narrowing, compiled to SQL predicates on the
/// `memories` alias. Built once and reused by all three query verbs so their
/// narrowing is identical.
struct KxFilters {
    tags: Vec<String>,
    /// Inclusive lower bound (epoch of `from` 00:00 UTC).
    from: Option<i64>,
    /// Exclusive upper bound (epoch of `to`+1 day 00:00 UTC) — whole-day
    /// inclusive per clarification #12.
    to_exclusive: Option<i64>,
}

impl KxFilters {
    fn new(tags: &[String], from: Option<&str>, to: Option<&str>) -> Result<Self> {
        let from = match from {
            Some(d) => Some(day_start_epoch(parse_ymd(d)?)),
            None => None,
        };
        let to_exclusive = match to {
            Some(d) => {
                let day = parse_ymd(d)?;
                let next = day.succ_opt().unwrap_or(day);
                Some(day_start_epoch(next))
            }
            None => None,
        };
        Ok(Self {
            tags: tags.to_vec(),
            from,
            to_exclusive,
        })
    }

    /// ` AND ...` predicate fragment plus bound values, on the given alias.
    /// Tag matching is comma-anchored so `cat:ml` never matches `cat:mlops`.
    fn predicates(&self, alias: &str) -> (String, Vec<Value>) {
        let mut sql = String::new();
        let mut vals: Vec<Value> = Vec::new();
        for t in &self.tags {
            sql.push_str(&format!(
                " AND (',' || {alias}.tags || ',') LIKE ? ESCAPE '\\'"
            ));
            vals.push(Value::Text(format!("%,{},%", like_escape(t))));
        }
        if let Some(f) = self.from {
            sql.push_str(&format!(" AND {alias}.valid_from >= ?"));
            vals.push(Value::Integer(f));
        }
        if let Some(t) = self.to_exclusive {
            sql.push_str(&format!(" AND {alias}.valid_from < ?"));
            vals.push(Value::Integer(t));
        }
        (sql, vals)
    }
}

/// Escape LIKE metacharacters (`\`, `%`, `_`) for the `ESCAPE '\'` clause.
fn like_escape(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_")
}

// ── recall / search / list ───────────────────────────────────────────────────

fn recall(conn: &Connection, args: KxRecallArgs) -> Result<()> {
    let (cfg_min, cfg_max) = resolve_query_knobs(args.config.as_deref())?;
    let min_score = args.min_score.unwrap_or(cfg_min);
    let limit = args.k.unwrap_or(cfg_max);

    let filters = KxFilters::new(&args.tags, args.from.as_deref(), args.to.as_deref())?;
    let (extra_sql, extra_vals) = filters.predicates("m");

    let opts = memory::RecallOpts {
        query: &args.query,
        type_filter: TypeFilter::Only(KX_TYPE.to_string()),
        scope: ScopeFilter::for_explicit(args.scope.as_deref()),
        extra_sql,
        extra_vals,
        min_score: Some(min_score),
        limit,
        // Raw cosine + min_score floor: a knowledge corpus wants pure semantic
        // similarity, not the personal-memory trust/freshness reweighting (#13,
        // punch-list #3).
        rank_v1: false,
    };

    let (model, hits) = match memory::semantic_recall(conn, &opts)? {
        Some(pair) => pair,
        None => {
            if args.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "query": args.query,
                        "results": [],
                        "error": "no embedder available; kx recall needs an embedder (see [embed] config)",
                    })
                );
            } else {
                eprintln!(
                    "kx recall: no embedder available (primary + fallback both failed); \
                     cannot run semantic recall. Configure [embed] or retry."
                );
            }
            return Ok(());
        }
    };

    if args.json {
        let results: Vec<serde_json::Value> = hits
            .iter()
            .map(|h| {
                serde_json::json!({
                    "id": h.id,
                    "name": h.name,
                    "description": h.description,
                    "score": h.score,
                    "cosine": h.cosine,
                    "path": pointer_of(&h.content),
                    "tags": h.tags,
                    "valid_from": h.valid_from,
                    "date": date_str(h.valid_from),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "model": model, "query": args.query, "results": results })
        );
        return Ok(());
    }

    println!("## kx recall (model: {model})\n");
    if hits.is_empty() {
        println!("(no entries at or above min_score {min_score:.2})");
        return Ok(());
    }
    for h in &hits {
        println!("[{:.3}] {} — {}", h.score, h.name, h.description);
        println!(
            "  {}  ({})  [{}]",
            pointer_of(&h.content),
            date_str(h.valid_from),
            h.tags
        );
    }
    Ok(())
}

fn search(conn: &Connection, args: KxSearchArgs) -> Result<()> {
    let (_min, cfg_max) = resolve_query_knobs(args.config.as_deref())?;
    let limit = args.limit.unwrap_or(cfg_max);

    let filters = KxFilters::new(&args.tags, args.from.as_deref(), args.to.as_deref())?;
    let (extra_sql, extra_vals) = filters.predicates("m");

    let rows = memory::search_rows(
        conn,
        &args.query,
        &TypeFilter::Only(KX_TYPE.to_string()),
        &ScopeFilter::for_explicit(args.scope.as_deref()),
        None,
        &extra_sql,
        &extra_vals,
        limit,
    )?;

    if args.json {
        let results: Vec<serde_json::Value> = rows
            .iter()
            .map(|r| {
                serde_json::json!({
                    "id": r.id,
                    "name": r.name,
                    "description": r.description,
                    "path": pointer_of(&r.content),
                    "tags": r.tags,
                    "valid_from": r.valid_from,
                    "date": date_str(r.valid_from),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({ "query": args.query, "results": results })
        );
        return Ok(());
    }

    println!("## kx search\n");
    for r in &rows {
        println!("{} — {}", r.name, r.description);
        println!(
            "  {}  ({})  [{}]",
            pointer_of(&r.content),
            date_str(r.valid_from),
            r.tags
        );
    }
    Ok(())
}

fn list(conn: &Connection, args: KxListArgs) -> Result<()> {
    let limit = args.limit.unwrap_or(LIST_DEFAULT_LIMIT);
    let filters = KxFilters::new(&args.tags, args.from.as_deref(), args.to.as_deref())?;
    let (extra_sql, extra_vals) = filters.predicates("m");
    let scope = ScopeFilter::for_explicit(args.scope.as_deref());
    let (scope_sql, scope_vals) = scope.sql_and("m.scope");

    let sql = format!(
        "SELECT m.id, m.name, m.description, m.tags, m.scope, m.valid_from, m.updated_at
         FROM memories m
         WHERE m.type = 'kx' AND m.is_active = 1 AND m.lifecycle = 'active'{scope_sql}{extra_sql}
         ORDER BY m.updated_at DESC
         LIMIT ?"
    );
    let mut p: Vec<Value> = Vec::new();
    p.extend(scope_vals);
    p.extend(extra_vals);
    p.push(Value::Integer(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(p.iter()), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, Option<i64>>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    if args.json {
        let results: Vec<serde_json::Value> = rows
            .iter()
            .map(|(id, name, desc, tags, scope, valid_from)| {
                serde_json::json!({
                    "id": id,
                    "name": name,
                    "description": desc,
                    "tags": tags,
                    "scope": scope,
                    "valid_from": valid_from,
                    "date": date_str(*valid_from),
                })
            })
            .collect();
        println!("{}", serde_json::json!({ "results": results }));
        return Ok(());
    }

    for (id, name, desc, tags, scope, valid_from) in &rows {
        println!(
            "[{id}] {name} <{scope}> ({}) — {desc}  [{tags}]",
            date_str(*valid_from)
        );
    }
    Ok(())
}

/// The pointer (absolute path) stored as the first line of a kx entry's content.
fn pointer_of(content: &str) -> &str {
    content.lines().next().unwrap_or("")
}

/// Format an optional `valid_from` epoch as `YYYY-MM-DD` (empty when absent).
fn date_str(epoch: Option<i64>) -> String {
    match epoch {
        Some(ts) => chrono::DateTime::from_timestamp(ts, 0)
            .map(|d| d.format("%Y-%m-%d").to_string())
            .unwrap_or_default(),
        None => String::new(),
    }
}

// ── Ingest ───────────────────────────────────────────────────────────────────

/// Per-source ingest tally.
#[derive(Debug, Default, Clone)]
struct SourceReport {
    path: String,
    scanned: usize,
    inserted: usize,
    updated: usize,
    skipped: usize,
}

fn ingest(conn: &Connection, args: IngestArgs) -> Result<()> {
    let cfg = load_config(args.config.as_deref())?.kx;

    // Model-summarized ingest is PR #2. Never silently degrade (#3).
    if cfg.summarize {
        bail!("summarize=true not implemented until PR #2 (heuristic ingest only in PR #1)");
    }

    let selected = select_sources(&cfg.sources, args.source.as_deref())?;
    if selected.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::json!({ "dry_run": args.dry_run, "sources": [], "note": "no [kx].sources configured" })
            );
        } else {
            eprintln!("kx ingest: no [kx].sources configured (nothing to do)");
        }
        return Ok(());
    }

    let mut reports: Vec<SourceReport> = Vec::new();
    for src in selected {
        match ingest_source(conn, &cfg, src, &args) {
            Ok(rep) => reports.push(rep),
            Err(e) => {
                // A bad source (missing dir, permissions) is warned and skipped
                // so a multi-corpus cron run is not aborted by one bad path.
                log::warn!("kx ingest: skipping source '{}': {e:#}", src.path);
            }
        }
    }

    report_ingest(&reports, args.dry_run, args.json);
    Ok(())
}

/// Pick the sources to ingest: all, or the one matching `--source` by its path
/// (exact) or the path's trailing component (clarification #11).
fn select_sources<'a>(sources: &'a [KxSource], want: Option<&str>) -> Result<Vec<&'a KxSource>> {
    let Some(want) = want else {
        return Ok(sources.iter().collect());
    };
    let matched: Vec<&KxSource> = sources
        .iter()
        .filter(|s| s.path == want || leaf_component(&s.path) == want)
        .collect();
    if matched.is_empty() {
        bail!("--source '{want}' matches no configured [[kx.sources]]");
    }
    Ok(matched)
}

/// Resolve a configured corpus path against the user's home: `~`, `~/x`, and
/// `$HOME/x` expand; anything else is taken literally.
///
/// Config must stay portable — a shipped example or a synced config that
/// names `/home/<someone>` is wrong on every other machine. Mirrors
/// `code_index::sources::expand_home`; the two crates are independent, so the
/// helper is duplicated rather than dragging in a dependency for 12 lines.
fn expand_home(raw: &str) -> PathBuf {
    let raw = raw.trim();
    let home = || std::env::var_os("HOME").map(PathBuf::from);
    if raw == "~" {
        if let Some(h) = home() {
            return h;
        }
    }
    for prefix in ["~/", "$HOME/"] {
        if let Some(rest) = raw.strip_prefix(prefix) {
            if let Some(h) = home() {
                return h.join(rest);
            }
        }
    }
    PathBuf::from(raw)
}

fn ingest_source(
    conn: &Connection,
    cfg: &KxSection,
    src: &KxSource,
    args: &IngestArgs,
) -> Result<SourceReport> {
    // `~`/`$HOME` first: canonicalize does NOT expand them, so a config
    // written `~/notes` would fail as "no such file" instead of resolving.
    // Strictly additive — a path that resolves today is passed through
    // untouched, only previously-broken tilde paths start working.
    let root = std::fs::canonicalize(expand_home(&src.path))
        .with_context(|| format!("resolving source path {}", src.path))?;
    let root_str = root.to_string_lossy().to_string();

    let extensions: Vec<String> = if src.extensions.is_empty() {
        cfg.default_extensions.clone()
    } else {
        src.extensions.clone()
    };
    let mut excludes = cfg.default_exclude.clone();
    excludes.extend(src.exclude_globs.iter().cloned());

    let scope = src
        .scope
        .clone()
        .unwrap_or_else(|| cfg.default_scope.clone());

    // tags = source.tags ∪ {repo:<leaf>, cat:<category>}, deduped first-seen (#7).
    let mut base_tags: Vec<String> = src.tags.clone();
    base_tags.push(format!("repo:{}", leaf_component(&root_str)));
    if let Some(cat) = &src.category {
        base_tags.push(format!("cat:{cat}"));
    }
    let tags = dedup_first_seen(&base_tags).join(",");

    let mut files: Vec<PathBuf> = Vec::new();
    collect_files(
        &root,
        &root,
        0,
        cfg.max_depth,
        &extensions,
        &excludes,
        &mut files,
    )?;
    files.sort();

    let mut rep = SourceReport {
        path: src.path.clone(),
        ..Default::default()
    };

    for path in files {
        rep.scanned += 1;
        match ingest_file(conn, &root, &root_str, &path, &tags, &scope, args) {
            Ok(Action::Inserted) => rep.inserted += 1,
            Ok(Action::Updated) => rep.updated += 1,
            Ok(Action::Skipped) => rep.skipped += 1,
            Err(e) => log::warn!("kx ingest: skipping {}: {e:#}", path.display()),
        }
    }
    Ok(rep)
}

enum Action {
    Inserted,
    Updated,
    Skipped,
}

#[allow(clippy::too_many_arguments)]
fn ingest_file(
    conn: &Connection,
    root: &Path,
    root_str: &str,
    path: &Path,
    tags: &str,
    scope: &str,
    args: &IngestArgs,
) -> Result<Action> {
    let body =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;

    let abs = path.to_string_lossy().to_string();
    let rel = path.strip_prefix(root).unwrap_or(path);
    let rel_str = rel.to_string_lossy().replace('\\', "/");

    let id = &embed::content_hash(&abs)[..16];
    let slug = slugify(&rel_str);
    let file_hash = embed::content_hash(&body);
    let source_col = format!("kxsha:{file_hash}");

    let filename = path
        .file_name()
        .map(|f| f.to_string_lossy().to_string())
        .unwrap_or_default();
    let valid_from = filename_date(&filename).unwrap_or_else(|| file_mtime(path));

    let (description, entry_text) = extract_entry(&body);
    let content = if entry_text.is_empty() {
        abs.clone()
    } else {
        format!("{abs}\n\n{entry_text}")
    };

    let existing: Option<String> = conn
        .query_row(
            "SELECT source FROM memories WHERE id = ?1",
            params![id],
            |r| r.get(0),
        )
        .optional()?;

    let action = match &existing {
        Some(src) if src.strip_prefix("kxsha:") == Some(file_hash.as_str()) && !args.force => {
            return Ok(Action::Skipped);
        }
        Some(_) => Action::Updated,
        None => Action::Inserted,
    };

    if args.dry_run {
        return Ok(action);
    }

    let now = memory::now();
    match action {
        Action::Updated => {
            // TRUE update (clarification #10): never INSERT OR REPLACE — that
            // is delete+insert in SQLite and would cascade-wipe this row's
            // embedding + FTS. UPDATE keeps the row; the changed content hash
            // makes try_embed_one re-embed below.
            conn.execute(
                "UPDATE memories
                 SET name=?1, description=?2, content=?3, source=?4, tags=?5, cwd=?6,
                     scope=?7, valid_from=?8, updated_at=?9, is_active=1, lifecycle='active'
                 WHERE id=?10",
                params![
                    slug,
                    description,
                    content,
                    source_col,
                    tags,
                    root_str,
                    scope,
                    valid_from,
                    now,
                    id
                ],
            )?;
        }
        Action::Inserted => {
            conn.execute(
                "INSERT INTO memories
                 (id, type, name, description, content, source, tags, cwd, scope,
                  is_active, created_at, updated_at, source_ref, valid_from, author)
                 VALUES (?1, 'kx', ?2, ?3, ?4, ?5, ?6, ?7, ?8, 1, ?9, ?9, ?10, ?11, 'kx')",
                params![
                    id,
                    slug,
                    description,
                    content,
                    source_col,
                    tags,
                    root_str,
                    scope,
                    now,
                    abs,
                    valid_from
                ],
            )?;
        }
        Action::Skipped => unreachable!(),
    }

    // Re-embed via the shared chain (fail-open). FTS auto-populates via the
    // memories_ai / memories_au triggers.
    let text = embed::memory_embed_text(&slug, &description, &content);
    embed::try_embed_one(conn, id, &text);

    Ok(action)
}

fn report_ingest(reports: &[SourceReport], dry_run: bool, json: bool) {
    let (mut ins, mut upd, mut skip, mut scan) = (0, 0, 0, 0);
    for r in reports {
        ins += r.inserted;
        upd += r.updated;
        skip += r.skipped;
        scan += r.scanned;
    }
    if json {
        let sources: Vec<serde_json::Value> = reports
            .iter()
            .map(|r| {
                serde_json::json!({
                    "path": r.path,
                    "scanned": r.scanned,
                    "inserted": r.inserted,
                    "updated": r.updated,
                    "skipped": r.skipped,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "dry_run": dry_run,
                "sources": sources,
                "totals": { "scanned": scan, "inserted": ins, "updated": upd, "skipped": skip },
            })
        );
        return;
    }
    let prefix = if dry_run { "[dry-run] " } else { "" };
    for r in reports {
        println!(
            "{prefix}{}: {} scanned, +{} inserted, ~{} updated, ={} skipped",
            r.path, r.scanned, r.inserted, r.updated, r.skipped
        );
    }
    println!("{prefix}total: {scan} scanned, +{ins} inserted, ~{upd} updated, ={skip} skipped");
}

// ── Filesystem walk ──────────────────────────────────────────────────────────

fn collect_files(
    root: &Path,
    dir: &Path,
    depth: usize,
    max_depth: usize,
    extensions: &[String],
    excludes: &[String],
    out: &mut Vec<PathBuf>,
) -> Result<()> {
    let entries =
        std::fs::read_dir(dir).with_context(|| format!("reading dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let rel = path.strip_prefix(root).unwrap_or(&path);
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        let ft = entry.file_type()?;
        if ft.is_dir() {
            if excludes.iter().any(|g| glob_match(g, &rel_str)) {
                continue;
            }
            if depth + 1 > max_depth {
                log::warn!(
                    "kx ingest: max_depth ({max_depth}) reached, not descending into {}",
                    path.display()
                );
                continue;
            }
            collect_files(root, &path, depth + 1, max_depth, extensions, excludes, out)?;
        } else if ft.is_file() {
            let ext_ok = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| extensions.iter().any(|x| x.eq_ignore_ascii_case(e)))
                .unwrap_or(false);
            if !ext_ok {
                continue;
            }
            if excludes.iter().any(|g| glob_match(g, &rel_str)) {
                continue;
            }
            out.push(path);
        }
    }
    Ok(())
}

// ── Heuristics ───────────────────────────────────────────────────────────────

/// Deterministic slug (clarification #15): lowercase, every run of non-[a-z0-9]
/// collapses to a single `-`, leading/trailing `-` stripped.
fn slugify(rel: &str) -> String {
    let mut out = String::new();
    let mut prev_dash = false;
    for c in rel.chars() {
        let lc = c.to_ascii_lowercase();
        if lc.is_ascii_alphanumeric() {
            out.push(lc);
            prev_dash = false;
        } else if !prev_dash {
            out.push('-');
            prev_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}

/// Heuristic entry (deliverable 6 / clarification #14): returns
/// `(description, entry_text)` where description = first H1/H2 heading if
/// present, else the first non-empty paragraph line, trimmed to ≤160 chars;
/// entry_text combines the heading and first paragraph for the embedding body.
fn extract_entry(content: &str) -> (String, String) {
    let mut heading: Option<String> = None;
    let mut para: Option<String> = None;
    for raw in content.lines() {
        let line = raw.trim();
        if line.is_empty() {
            continue;
        }
        let is_h12 = line.starts_with("# ") || line.starts_with("## ");
        if is_h12 {
            if heading.is_none() {
                let h = line.trim_start_matches('#').trim().to_string();
                if !h.is_empty() {
                    heading = Some(h);
                }
            }
        } else if !line.starts_with('#') && para.is_none() {
            para = Some(line.to_string());
        }
        if heading.is_some() && para.is_some() {
            break;
        }
    }
    let description = heading.clone().or_else(|| para.clone()).unwrap_or_default();
    let description = truncate_chars(&description, DESC_MAX_CHARS);
    let entry_text = match (heading, para) {
        (Some(h), Some(p)) => format!("{h}\n\n{p}"),
        (Some(h), None) => h,
        (None, Some(p)) => p,
        (None, None) => String::new(),
    };
    (description, entry_text)
}

fn truncate_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        s.chars().take(n).collect()
    }
}

/// Parse a `YYYY-MM-DD` anywhere in the filename → that day's 00:00 UTC epoch
/// (clarification #6; YYYY-MM-DD only).
fn filename_date(name: &str) -> Option<i64> {
    let b = name.as_bytes();
    if b.len() < 10 {
        return None;
    }
    for i in 0..=b.len() - 10 {
        let w = &b[i..i + 10];
        let shaped = w[0].is_ascii_digit()
            && w[1].is_ascii_digit()
            && w[2].is_ascii_digit()
            && w[3].is_ascii_digit()
            && w[4] == b'-'
            && w[5].is_ascii_digit()
            && w[6].is_ascii_digit()
            && w[7] == b'-'
            && w[8].is_ascii_digit()
            && w[9].is_ascii_digit();
        if shaped {
            // The byte-shape guarantees ASCII, so this slice is on a boundary.
            if let Ok(d) = NaiveDate::parse_from_str(&name[i..i + 10], "%Y-%m-%d") {
                return Some(day_start_epoch(d));
            }
        }
    }
    None
}

fn file_mtime(path: &Path) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or_else(memory::now)
}

fn parse_ymd(d: &str) -> Result<NaiveDate> {
    NaiveDate::parse_from_str(d, "%Y-%m-%d")
        .with_context(|| format!("invalid date '{d}' (expected YYYY-MM-DD)"))
}

fn day_start_epoch(d: NaiveDate) -> i64 {
    d.and_hms_opt(0, 0, 0)
        .map(|dt| dt.and_utc().timestamp())
        .unwrap_or(0)
}

/// The trailing path component (used for the `repo:` tag and `--source` match).
fn leaf_component(path: &str) -> String {
    let trimmed = path.trim_end_matches('/');
    trimmed
        .rsplit('/')
        .find(|c| !c.is_empty())
        .unwrap_or(trimmed)
        .to_string()
}

fn dedup_first_seen(tags: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for t in tags {
        if t.is_empty() {
            continue;
        }
        if seen.insert(t.clone()) {
            out.push(t.clone());
        }
    }
    out
}

// ── Glob matching (no external crate; only `toml` is the new dependency) ──────

/// Real glob matching (punch-list #6): `*` matches any run of non-`/`, `?` one
/// char, `**` any run of path segments. A slash-less pattern matches the
/// basename (gitignore convention), so `*.log` excludes log files anywhere.
fn glob_match(pattern: &str, path: &str) -> bool {
    let norm = path.replace('\\', "/");
    if !pattern.contains('/') {
        let base = norm.rsplit('/').next().unwrap_or(&norm);
        return seg_match(pattern.as_bytes(), base.as_bytes());
    }
    let pat_segs: Vec<&str> = pattern.split('/').collect();
    let path_segs: Vec<&str> = norm.split('/').collect();
    match_segs(&pat_segs, &path_segs)
}

fn match_segs(pat: &[&str], text: &[&str]) -> bool {
    if pat.is_empty() {
        return text.is_empty();
    }
    if pat[0] == "**" {
        // `**` consumes zero or more path segments.
        for i in 0..=text.len() {
            if match_segs(&pat[1..], &text[i..]) {
                return true;
            }
        }
        return false;
    }
    if text.is_empty() {
        return false;
    }
    if seg_match(pat[0].as_bytes(), text[0].as_bytes()) {
        return match_segs(&pat[1..], &text[1..]);
    }
    false
}

/// Backtracking single-segment matcher: `*` = zero+ chars (no `/`), `?` = one.
fn seg_match(pat: &[u8], text: &[u8]) -> bool {
    let (mut p, mut t) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while t < text.len() {
        if p < pat.len() && (pat[p] == b'?' || pat[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pat.len() && pat[p] == b'*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(sp) = star {
            p = sp + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    while p < pat.len() && pat[p] == b'*' {
        p += 1;
    }
    p == pat.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    // ── pure helpers ─────────────────────────────────────────────────────

    #[test]
    fn slug_matches_spec_example() {
        assert_eq!(
            slugify("harness-model-fit/findings.md"),
            "harness-model-fit-findings-md"
        );
        assert_eq!(slugify("A B/c.MD"), "a-b-c-md");
        assert_eq!(slugify("///weird__name///"), "weird-name");
    }

    #[test]
    fn filename_date_parses_ymd_else_none() {
        assert_eq!(
            filename_date("2024-03-15-notes.md"),
            Some(day_start_epoch(
                NaiveDate::from_ymd_opt(2024, 3, 15).unwrap()
            ))
        );
        assert_eq!(filename_date("notes.md"), None);
        // Not a real date → no match.
        assert_eq!(filename_date("2024-13-40.md"), None);
    }

    #[test]
    fn extract_entry_prefers_h12_heading() {
        let (desc, text) = extract_entry("\n\n## Findings\n\nBody paragraph here.\n");
        assert_eq!(desc, "Findings");
        assert!(text.contains("Findings") && text.contains("Body paragraph"));

        // No heading → first paragraph is the description.
        let (desc2, _) = extract_entry("just a line\nsecond line");
        assert_eq!(desc2, "just a line");

        // H3 is not H1/H2: it is skipped for both heading and paragraph.
        let (desc3, _) = extract_entry("### deep\nreal paragraph");
        assert_eq!(desc3, "real paragraph");
    }

    #[test]
    fn description_capped_at_160_chars() {
        let long = "# ".to_string() + &"x".repeat(500);
        let (desc, _) = extract_entry(&long);
        assert_eq!(desc.chars().count(), 160);
    }

    #[test]
    fn glob_matches_default_excludes_really() {
        // basename patterns
        assert!(glob_match("*.log", "a/b/c.log"));
        assert!(!glob_match("*.log", "a/b/c.md"));
        assert!(glob_match("*.lock", "Cargo.lock"));
        // globstar dir patterns
        assert!(glob_match("**/.git/**", ".git/config"));
        assert!(glob_match("**/.git/**", "sub/dir/.git/HEAD"));
        assert!(glob_match("**/target/**", "target/debug/x"));
        assert!(glob_match(
            "**/node_modules/**",
            "a/node_modules/pkg/index.js"
        ));
        assert!(!glob_match("**/target/**", "src/targetish/x"));
        // NOT a naive substring: `cat` must be a full segment
        assert!(!glob_match("**/git/**", "a/gitlab/x"));
    }

    #[test]
    fn tag_filter_is_comma_anchored() {
        let f = KxFilters::new(&["cat:ml".into()], None, None).unwrap();
        let (sql, vals) = f.predicates("m");
        assert!(sql.contains("LIKE ? ESCAPE"));
        assert_eq!(vals.len(), 1);
        match &vals[0] {
            Value::Text(s) => assert_eq!(s, "%,cat:ml,%"),
            _ => panic!("expected text pattern"),
        }
    }

    #[test]
    fn date_filters_are_inclusive_whole_day() {
        let f = KxFilters::new(&[], Some("2024-01-10"), Some("2024-01-10")).unwrap();
        let from = day_start_epoch(NaiveDate::from_ymd_opt(2024, 1, 10).unwrap());
        let to_excl = day_start_epoch(NaiveDate::from_ymd_opt(2024, 1, 11).unwrap());
        assert_eq!(f.from, Some(from));
        assert_eq!(f.to_exclusive, Some(to_excl));
        // A mtime anytime on the 10th (e.g. noon) is included.
        let noon = from + 12 * 3600;
        assert!(noon >= from && noon < to_excl);
    }

    // ── config ───────────────────────────────────────────────────────────

    #[test]
    fn config_deserializes_kx_section_with_sources() {
        let toml = r#"
[openrouter]
api_key = "sk-or-v1-SECRET"

[kx]
default_scope = "work"
min_score = 0.7
max_hits = 9

[[kx.sources]]
path = "/home/x/mu"
extensions = ["md", "txt"]
exclude-globs = ["**/drafts/**"]
scope = "shared"
tags = ["topic:mu"]
category = "notes"
"#;
        let cfg = parse_config(toml, Path::new("test.toml")).unwrap();
        assert_eq!(cfg.kx.min_score, 0.7);
        assert_eq!(cfg.kx.max_hits, 9);
        assert_eq!(cfg.kx.sources.len(), 1);
        let s = &cfg.kx.sources[0];
        assert_eq!(s.path, "/home/x/mu");
        assert_eq!(s.extensions, vec!["md", "txt"]);
        assert_eq!(s.exclude_globs, vec!["**/drafts/**"]);
        assert_eq!(s.scope.as_deref(), Some("shared"));
        assert_eq!(s.category.as_deref(), Some("notes"));
    }

    #[test]
    fn config_defaults_apply_when_kx_absent() {
        let cfg = parse_config("[embed]\nmodel = \"x\"\n", Path::new("t")).unwrap();
        assert_eq!(cfg.kx.default_scope, "work");
        assert_eq!(cfg.kx.min_score, BUILTIN_MIN_SCORE);
        assert_eq!(cfg.kx.max_hits, BUILTIN_MAX_HITS);
        assert_eq!(cfg.kx.default_extensions, vec!["md".to_string()]);
        assert!(cfg.kx.sources.is_empty());
        assert!(cfg.kx.default_exclude.contains(&"**/.git/**".to_string()));
    }

    #[test]
    fn malformed_config_errors_without_echoing_content() {
        // A syntax error next to a secret must NOT surface the secret line.
        let bad = "[openrouter]\napi_key = \"sk-or-v1-SECRET\"\nthis is not toml =";
        let err = parse_config(bad, Path::new("cfg.toml"))
            .unwrap_err()
            .to_string();
        assert!(!err.contains("SECRET"), "error leaked a secret: {err}");
        assert!(err.contains("cfg.toml"));
    }

    #[test]
    fn shipped_example_config_deserializes() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../config.toml.example");
        let content = std::fs::read_to_string(path).expect("read shipped example");
        let cfg = parse_config(&content, Path::new(path)).expect("example [kx] parses");
        assert!(
            !cfg.kx.sources.is_empty(),
            "shipped example should document at least one [[kx.sources]]"
        );
    }

    #[test]
    fn resolve_query_knobs_reads_config_then_defaults() {
        let dir = temp_dir("kx-knobs");
        let cfg_path = dir.join("config.toml");
        std::fs::write(&cfg_path, "[kx]\nmin_score = 0.75\nmax_hits = 12\n").unwrap();
        let (min, max) = resolve_query_knobs(Some(&cfg_path)).unwrap();
        assert_eq!(min, 0.75);
        assert_eq!(max, 12);
        // A missing default path (explicit=None handled elsewhere); an explicit
        // missing path errors.
        let missing = dir.join("nope.toml");
        assert!(resolve_query_knobs(Some(&missing)).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── ingest end-to-end (no network required; embedding is fail-open) ───

    fn temp_dir(prefix: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("{prefix}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Write a corpus + a config pointing at it; return (dir, config path).
    fn corpus_with_config(files: &[(&str, &str)]) -> (PathBuf, PathBuf) {
        let dir = temp_dir("kx-corpus");
        let corpus = dir.join("mycorpus");
        std::fs::create_dir_all(&corpus).unwrap();
        for (name, body) in files {
            let p = corpus.join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, body).unwrap();
        }
        let cfg = dir.join("config.toml");
        let cfg_body = format!(
            "[[kx.sources]]\npath = \"{}\"\ntags = [\"topic:test\"]\ncategory = \"notes\"\n",
            corpus.to_string_lossy()
        );
        std::fs::write(&cfg, cfg_body).unwrap();
        (dir, cfg)
    }

    fn ingest_args(cfg: &Path, dry_run: bool, force: bool) -> IngestArgs {
        IngestArgs {
            source: None,
            dry_run,
            force,
            config: Some(cfg.to_path_buf()),
            json: false,
        }
    }

    #[test]
    fn ingest_respects_max_depth() {
        let conn = db::open_in_memory().unwrap();
        let dir = temp_dir("kx-depth");
        let corpus = dir.join("c");
        std::fs::create_dir_all(corpus.join("a/b")).unwrap();
        std::fs::write(corpus.join("top.md"), "# Top\n\nbody").unwrap();
        std::fs::write(corpus.join("a/mid.md"), "# Mid\n\nbody").unwrap();
        std::fs::write(corpus.join("a/b/deep.md"), "# Deep\n\nbody").unwrap();
        let cfg = dir.join("config.toml");
        std::fs::write(
            &cfg,
            format!(
                "[kx]\nmax_depth = 1\n\n[[kx.sources]]\npath = \"{}\"\n",
                corpus.to_string_lossy()
            ),
        )
        .unwrap();

        ingest(&conn, ingest_args(&cfg, false, false)).unwrap();

        let mut names: Vec<String> = conn
            .prepare("SELECT name FROM memories WHERE type='kx'")
            .unwrap()
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        names.sort();
        // depth 0 (top) + depth 1 (a/mid) are walked; depth 2 (a/b/deep) is capped out.
        assert!(
            names.iter().any(|n| n.contains("top")),
            "root file ingested: {names:?}"
        );
        assert!(
            names.iter().any(|n| n.contains("mid")),
            "depth-1 file ingested: {names:?}"
        );
        assert!(
            !names.iter().any(|n| n.contains("deep")),
            "depth-2 file beyond max_depth=1 excluded: {names:?}"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn ingest_inserts_rows_with_expected_metadata() {
        let conn = db::open_in_memory().unwrap();
        let (dir, cfg) = corpus_with_config(&[
            ("2024-03-15-alpha.md", "# Alpha\n\nAlpha body paragraph."),
            ("nested/beta.md", "Just a plain paragraph, no heading."),
            ("skip.txt", "not an md file"),
        ]);

        ingest(&conn, ingest_args(&cfg, false, false)).unwrap();

        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories WHERE type='kx'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 2, "only the two .md files ingested (.txt excluded)");

        // Alpha: dated filename, H1 description, tags include topic/repo/cat.
        let (name, desc, tags, scope, source, valid_from): (
            String,
            String,
            String,
            String,
            String,
            i64,
        ) = conn
            .query_row(
                "SELECT name, description, tags, scope, source, valid_from
                 FROM memories WHERE type='kx' AND name LIKE '%alpha%'",
                [],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(name, "2024-03-15-alpha-md");
        assert_eq!(desc, "Alpha");
        assert_eq!(scope, "work", "default_scope applied");
        assert!(tags.contains("topic:test"), "source tag kept: {tags}");
        assert!(tags.contains("cat:notes"), "category tag added: {tags}");
        assert!(tags.contains("repo:mycorpus"), "repo tag from leaf: {tags}");
        assert!(
            source.starts_with("kxsha:"),
            "content hash stored: {source}"
        );
        assert_eq!(
            valid_from,
            day_start_epoch(NaiveDate::from_ymd_opt(2024, 3, 15).unwrap()),
            "filename date drives valid_from"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_updates_changed_and_skips_unchanged() {
        let conn = db::open_in_memory().unwrap();
        let (dir, cfg) = corpus_with_config(&[("doc.md", "# One\n\nfirst body")]);

        // First run inserts.
        ingest(&conn, ingest_args(&cfg, false, false)).unwrap();
        let id: String = conn
            .query_row("SELECT id FROM memories WHERE type='kx'", [], |r| r.get(0))
            .unwrap();
        let hash1: String = conn
            .query_row(
                "SELECT source FROM memories WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();

        // Second run, no change → skipped, same row/hash.
        ingest(&conn, ingest_args(&cfg, false, false)).unwrap();
        let count_after_skip: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories WHERE type='kx'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count_after_skip, 1, "skip must not duplicate the row");
        let hash_after_skip: String = conn
            .query_row(
                "SELECT source FROM memories WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hash_after_skip, hash1, "unchanged doc keeps its hash");

        // Change the file → update (same id, new hash, new description).
        let corpus_doc = {
            // reconstruct the doc path from the config's source
            let content = std::fs::read_to_string(&cfg).unwrap();
            let path_line = content.lines().find(|l| l.contains("path =")).unwrap();
            let start = path_line.find('"').unwrap() + 1;
            let end = path_line.rfind('"').unwrap();
            PathBuf::from(&path_line[start..end]).join("doc.md")
        };
        std::fs::write(&corpus_doc, "# Two\n\nsecond body, changed").unwrap();
        ingest(&conn, ingest_args(&cfg, false, false)).unwrap();

        let (count2, desc2, hash2): (i64, String, String) = {
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM memories WHERE type='kx'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            let row: (String, String) = conn
                .query_row(
                    "SELECT description, source FROM memories WHERE id=?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            (count, row.0, row.1)
        };
        assert_eq!(count2, 1, "update must not create a new row (true UPDATE)");
        assert_eq!(desc2, "Two", "row reflects the changed content");
        assert_ne!(hash2, hash1, "content hash changed on update");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ingest_summarize_true_errors() {
        let conn = db::open_in_memory().unwrap();
        let dir = temp_dir("kx-summ");
        let cfg = dir.join("config.toml");
        std::fs::write(&cfg, "[kx]\nsummarize = true\n").unwrap();
        let err = ingest(&conn, ingest_args(&cfg, false, false))
            .unwrap_err()
            .to_string();
        assert!(err.contains("summarize=true"), "loud error expected: {err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── retrieval narrowing + type isolation (FTS: hermetic, no embedder) ─

    /// Insert a kx row directly (bypassing ingest/embedding) for query tests.
    #[allow(clippy::too_many_arguments)]
    fn seed_kx(conn: &Connection, id: &str, name: &str, tags: &str, scope: &str, valid_from: i64) {
        conn.execute(
            "INSERT INTO memories
             (id, type, name, description, content, source, tags, cwd, scope,
              is_active, created_at, updated_at, source_ref, valid_from, author)
             VALUES (?1, 'kx', ?2, 'desc', ?3, 'kxsha:deadbeef', ?4, '', ?5,
                     1, 1000, 1000, '/abs/path', ?6, 'kx')",
            params![
                id,
                name,
                format!("/abs/{name} findings note"),
                tags,
                scope,
                valid_from
            ],
        )
        .unwrap();
    }

    fn search_ids(conn: &Connection, args: KxSearchArgs) -> Vec<String> {
        let filters = KxFilters::new(&args.tags, args.from.as_deref(), args.to.as_deref()).unwrap();
        let (extra_sql, extra_vals) = filters.predicates("m");
        memory::search_rows(
            conn,
            &args.query,
            &TypeFilter::Only(KX_TYPE.to_string()),
            &ScopeFilter::for_explicit(args.scope.as_deref()),
            None,
            &extra_sql,
            &extra_vals,
            50,
        )
        .unwrap()
        .into_iter()
        .map(|r| r.id)
        .collect()
    }

    fn base_search(query: &str) -> KxSearchArgs {
        KxSearchArgs {
            query: query.to_string(),
            scope: None,
            tags: vec![],
            from: None,
            to: None,
            limit: None,
            config: None,
            json: false,
        }
    }

    #[test]
    fn kx_search_narrows_by_scope_tag_and_date() {
        let conn = db::open_in_memory().unwrap();
        let d = |y, m, day| day_start_epoch(NaiveDate::from_ymd_opt(y, m, day).unwrap());
        seed_kx(
            &conn,
            "w1",
            "note-w1",
            "repo:mu,cat:ml",
            "work",
            d(2024, 1, 5),
        );
        seed_kx(
            &conn,
            "p1",
            "note-p1",
            "repo:mu,cat:ops",
            "personal",
            d(2024, 2, 5),
        );
        seed_kx(
            &conn,
            "s1",
            "note-s1",
            "repo:mu,cat:ml",
            "shared",
            d(2024, 3, 5),
        );

        // Open query: all three (they share the word "findings").
        let mut all = search_ids(&conn, base_search("findings"));
        all.sort();
        assert_eq!(all, vec!["p1", "s1", "w1"]);

        // scope=work → work + shared (never personal).
        let mut by_scope = search_ids(
            &conn,
            KxSearchArgs {
                scope: Some("work".into()),
                ..base_search("findings")
            },
        );
        by_scope.sort();
        assert_eq!(by_scope, vec!["s1", "w1"]);

        // tag cat:ml (comma-anchored) → w1 + s1 (not p1's cat:ops).
        let mut by_tag = search_ids(
            &conn,
            KxSearchArgs {
                tags: vec!["cat:ml".into()],
                ..base_search("findings")
            },
        );
        by_tag.sort();
        assert_eq!(by_tag, vec!["s1", "w1"]);

        // date window Feb..Feb → only p1.
        let by_date = search_ids(
            &conn,
            KxSearchArgs {
                from: Some("2024-02-01".into()),
                to: Some("2024-02-28".into()),
                ..base_search("findings")
            },
        );
        assert_eq!(by_date, vec!["p1"]);

        // combined tag AND scope AND date → empty (w1 is Jan, s1 is shared+ml
        // but March; requiring cat:ml + Feb window leaves nothing).
        let combined = search_ids(
            &conn,
            KxSearchArgs {
                tags: vec!["cat:ml".into()],
                from: Some("2024-02-01".into()),
                to: Some("2024-02-28".into()),
                ..base_search("findings")
            },
        );
        assert!(combined.is_empty());
    }

    #[test]
    fn generic_memory_excludes_kx_but_kx_filter_includes_it() {
        let conn = db::open_in_memory().unwrap();
        // one kx row + one ordinary project row, both matching "widget".
        seed_kx(&conn, "k1", "kx-widget", "repo:mu", "work", 1000);
        conn.execute(
            "INSERT INTO memories
             (id, type, name, description, content, created_at, updated_at, author)
             VALUES ('m1','project','proj-widget','d','widget content',1000,1000,'a')",
            [],
        )
        .unwrap();

        // ExcludeKx (generic memory search) must NOT return the kx row.
        let generic = memory::search_rows(
            &conn,
            "widget",
            &TypeFilter::ExcludeKx(None),
            &ScopeFilter::All,
            None,
            "",
            &[],
            50,
        )
        .unwrap();
        let generic_ids: Vec<&str> = generic.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(generic_ids, vec!["m1"], "generic recall hides type='kx'");

        // Only('kx') returns exactly the kx row.
        let kx = search_ids(&conn, base_search("widget"));
        assert_eq!(kx, vec!["k1"]);
    }
}
