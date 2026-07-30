use crate::embed;
use anyhow::{bail, Context, Result};
use chrono::Utc;
use clap::{Args, Subcommand};
use rusqlite::{params, params_from_iter, types::Value, Connection};
use serde::Deserialize;
use std::collections::{BTreeSet, HashMap};
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
    /// Show full memory contents by id or exact name
    Show(ShowArgs),
    /// Archive a memory: hide from normal retrieval but keep restorable
    Archive(LifecycleArgs),
    /// Move a memory to trash: hide from normal retrieval, pending purge
    Trash(LifecycleArgs),
    /// Restore an archived or trashed memory to active retrieval
    Restore(LifecycleArgs),
    /// Show lifecycle/change events for a memory
    Events(EventsArgs),
    /// Review-friendly patch/event history with diff hints
    PatchLog(EventsArgs),
    /// Render before/after diff for a memory event
    Diff(DiffArgs),
    /// Apply selected candidates from a machine-readable memory plan
    ApplyPlan(ApplyPlanArgs),
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
    /// Analyze recall query log for coverage gaps and hotspots
    RecallStats(RecallStatsArgs),
    /// (Re-)embed memories using the configured embedder
    Reindex(ReindexArgs),
    /// Mark OLD as superseded by NEW (testimony correction; read paths
    /// then show the successor and label the stale entry)
    Correct(CorrectArgs),
    /// Retract a memory: no longer true, nothing replaces it (AGM
    /// contraction — hidden everywhere; restorable)
    Retract(RetractArgs),
    /// Follow a memory's supersession chain to its current head
    Resolve(ResolveArgs),
    /// List/dismiss suspected-conflict queue rows (write-time adjudicator
    /// and sweep park uncertain relations here; resolve with correct/retract)
    Conflicts(ConflictsArgs),
    /// Backlog sweep: run supersession adjudication over existing memories
    Sweep(SweepArgs),
    /// Identity-kernel editor: see and curate exactly what
    /// `context --tier identity` injects (at-kernel-editor-oio)
    Kernel(KernelCmd),
    /// Stamp a memory as terrain-checked now (sets verified_at)
    Verify(VerifyArgs),
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
    /// Profile scope (work/personal/shared). Defaults to $CLAUDE_PROFILE, else "shared".
    #[arg(long)]
    pub scope: Option<String>,
    /// Evidence pointer (transcript path, daemon:session:event_seq, URL)
    #[arg(long)]
    pub source_ref: Option<String>,
    /// Witness: who asserts this. Defaults $AGENT_AUTHOR, else $CLAUDE_PROFILE.
    #[arg(long)]
    pub author: Option<String>,
    /// Skip write-time supersession adjudication (bulk imports, tests)
    #[arg(long)]
    pub no_adjudicate: bool,
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
    /// Re-scope this memory (work/personal/shared).
    #[arg(long)]
    pub scope: Option<String>,
}

#[derive(Args)]
pub struct ShowArgs {
    /// Memory id or exact memory name
    pub key: String,
    /// Include inactive memories in lookup
    #[arg(long)]
    pub inactive: bool,
    /// Emit JSON instead of human-readable text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct LifecycleArgs {
    /// Memory id
    pub id: String,
    /// Reason recorded in memory_events
    #[arg(long, default_value = "")]
    pub reason: String,
    /// Source report or artifact path recorded in memory_events
    #[arg(long, default_value = "")]
    pub source_report: String,
}

#[derive(Args)]
pub struct EventsArgs {
    /// Memory id
    pub id: String,
    #[arg(long, default_value = "20")]
    pub limit: usize,
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct DiffArgs {
    /// Memory id
    pub id: String,
    /// Specific memory_events.id to diff; defaults to latest event for memory
    #[arg(long)]
    pub event: Option<i64>,
    /// Emit JSON diff instead of human-readable text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ApplyPlanArgs {
    /// JSON apply-plan path
    pub file: PathBuf,
    /// Candidate id to apply. Repeatable. Required unless --dry-run.
    #[arg(long = "select")]
    pub select: Vec<String>,
    /// Print selected actions without mutating memory
    #[arg(long)]
    pub dry_run: bool,
}

#[derive(Deserialize)]
struct ApplyPlan {
    candidates: Vec<ApplyCandidate>,
}

#[derive(Deserialize)]
struct ApplyCandidate {
    id: String,
    action: String,
    #[serde(default)]
    memory_id: Option<String>,
    #[serde(default, rename = "type")]
    type_: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    tags: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    active: Option<bool>,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    source_report: Option<String>,
}

#[derive(Args)]
pub struct SearchArgs {
    pub query: String,
    #[arg(long)]
    pub r#type: Option<String>,
    #[arg(long, default_value = "10")]
    pub limit: usize,
    /// Restrict to a scope (work/personal/shared/*). Spans all scopes if omitted.
    #[arg(long)]
    pub scope: Option<String>,
    /// Restrict to a witness (author). Hive-mind (all authors) if omitted.
    #[arg(long)]
    pub author: Option<String>,
}

#[derive(Args)]
pub struct CorrectArgs {
    /// The stale memory id being corrected
    pub old: String,
    /// The memory id that supersedes it
    #[arg(long = "with")]
    pub with_id: String,
    /// Why the old fact is superseded
    #[arg(long, default_value = "")]
    pub reason: String,
    /// Supersession kind: "corrects" (old was never true), "updates"
    /// (the world changed — old stays true history, its valid_to is
    /// closed), "refines", or "consolidates"
    #[arg(long, default_value = "corrects",
          value_parser = ["corrects", "updates", "refines", "consolidates"])]
    pub kind: String,
}

#[derive(Args)]
pub struct ResolveArgs {
    /// Memory id (or exact name) to resolve to its current head
    pub key: String,
    /// Emit JSON instead of human-readable text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct ConflictsArgs {
    /// Dismiss a queue row by its numeric id (false positive; logged)
    #[arg(long)]
    pub dismiss: Option<i64>,
    /// Why this suspected conflict is a false positive (with --dismiss)
    #[arg(long, default_value = "")]
    pub reason: String,
    /// Include dismissed/resolved rows, not just open ones
    #[arg(long)]
    pub all: bool,
    /// Emit JSON instead of human-readable text
    #[arg(long)]
    pub json: bool,
}

#[derive(Args)]
pub struct SweepArgs {
    /// Only sweep prose-marked memories (CORRECTED/SUPERSEDES/OBSOLETE/...)
    #[arg(long)]
    pub prose_only: bool,
    /// Print proposals without creating edges/queue rows or recording coverage
    #[arg(long)]
    pub dry_run: bool,
    /// Sweep at most N seeds this run
    #[arg(long)]
    pub limit: Option<usize>,
    /// Re-sweep seeds already covered in sweep_state
    #[arg(long)]
    pub force: bool,
}

#[derive(Args)]
pub struct RetractArgs {
    /// Memory id to retract (no longer true, nothing replaces it)
    pub id: String,
    /// Why this is retracted — required: a retraction without a reason
    /// is indistinguishable from vandalism in the audit log
    #[arg(long)]
    pub reason: String,
}

/// at-kernel-editor-oio ("Morty's Mind Blowers"): operator-facing
/// editor for the injected identity kernel. Kernel membership is the
/// `identity` tag (see [`IDENTITY_TAG`]) — these commands are a thin
/// projection over the `context --tier identity` selection plus
/// tag mutations, every change logged to `memory_events`.
#[derive(Args)]
pub struct KernelCmd {
    #[command(subcommand)]
    pub action: KernelAction,
}

#[derive(Subcommand)]
pub enum KernelAction {
    /// Render the kernel exactly as `context --tier identity` selects
    /// it: id, type, trust, token estimate, injection count per row
    Show(KernelShowArgs),
    /// Remove a memory from the kernel (drops the `identity` tag; the
    /// memory stays recall-able). Requires --reason: kernel membership
    /// changes are ledger acts
    Demote(KernelDemoteArgs),
    /// Add a memory to the kernel (adds the `identity` tag).
    /// `pin` is an alias
    #[command(visible_alias = "pin")]
    Promote(KernelPromoteArgs),
    /// Supersede a kernel row — alias of `memory correct OLD --with NEW`
    Supersede(CorrectArgs),
}

#[derive(Args)]
pub struct KernelShowArgs {
    /// Active profile scope (same semantics as `context --scope`)
    #[arg(long)]
    pub scope: Option<String>,
}

#[derive(Args)]
pub struct KernelDemoteArgs {
    /// Memory id to demote out of the kernel
    pub id: String,
    /// Why this row leaves the kernel (required — initial the strike)
    #[arg(long)]
    pub reason: String,
}

#[derive(Args)]
pub struct KernelPromoteArgs {
    /// Memory id to promote into the kernel
    pub id: String,
    /// Why this row joins the kernel
    #[arg(long, default_value = "")]
    pub reason: String,
}

#[derive(Args)]
pub struct VerifyArgs {
    /// Memory id that was terrain-checked
    pub id: String,
    /// Optional note about how it was verified
    #[arg(long, default_value = "")]
    pub note: String,
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
    /// Lifecycle state to list: active, archived, or trashed
    #[arg(long)]
    pub lifecycle: Option<String>,
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
    /// Active profile scope. Defaults to $CLAUDE_PROFILE; "*" spans all scopes.
    /// A profile sees its own + shared memories; absent/unknown spans all (back-compat).
    #[arg(long)]
    pub scope: Option<String>,
    /// at-0q9: which injection tier to emit. "full" (default) is the
    /// classic four-section wall; "identity" is the small kernel —
    /// user profile first, then feedback rules tagged 'identity'
    /// (~600-800 tokens). Tier, not topic: the kernel carries who the
    /// operator is and how to engage, never task detail (task detail
    /// is recall-only — see mu's memory-hierarchy-and-trust spec,
    /// "Injection economics: small kernel, discoverable tail").
    #[arg(long, default_value = "full")]
    pub tier: String,
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
    /// Restrict to a scope (work/personal/shared/*). Spans all scopes if omitted.
    #[arg(long)]
    pub scope: Option<String>,
    /// Emit JSON instead of human-readable text (for programmatic use)
    #[arg(long)]
    pub json: bool,
    /// Include full content body in results (default: id/name/description only)
    #[arg(long)]
    pub full: bool,
    /// Ranking function: "v1" (cosine x trust x freshness + recency
    /// tie-break, at-supersession-activation-gf2.1) or "legacy"
    /// (pure cosine, the pre-gf2.1 behavior)
    #[arg(long, default_value = "v1", value_parser = ["v1", "legacy"])]
    pub rank: String,
}

#[derive(Args)]
pub struct RecallStatsArgs {
    /// Show queries where top_score was below threshold (weak-recall queries)
    #[arg(long)]
    pub gaps: bool,
    /// Threshold for --gaps (cosine similarity)
    #[arg(long, default_value = "0.45")]
    pub gaps_threshold: f64,
    /// Group queries by first significant token; rank by avg top_score
    #[arg(long)]
    pub hotspots: bool,
    /// Show queries where recall surfaced a high-scoring result FTS missed (requires --compare in original call)
    #[arg(long)]
    pub recall_vs_search: bool,
    /// Threshold for --recall-vs-search (cosine similarity floor)
    #[arg(long, default_value = "0.6")]
    pub rvs_threshold: f64,
    /// Restrict to last N days
    #[arg(long, default_value = "30")]
    pub days: i64,
    /// Limit rows shown per view
    #[arg(long, default_value = "20")]
    pub limit: usize,
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

/// Row mirror of the `memories` table. Not every column is read by every
/// command; the unused ones are kept so the struct documents the full row
/// shape rather than a per-command projection.
#[allow(dead_code)]
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
    lifecycle: String,
    created_at: i64,
    updated_at: i64,
    verified_at: Option<i64>,
    author: String,
}

fn short_id() -> String {
    Uuid::new_v4().to_string()[..8].to_string()
}

pub(crate) fn now() -> i64 {
    Utc::now().timestamp()
}

// ── Profile scoping ─────────────────────────────────────────────────────────────
//
// Memories carry a `scope`: a profile name (`work`, `personal`) or the reserved
// `shared` (visible to every profile). The active profile comes from the
// `--scope` flag, else the `CLAUDE_PROFILE` env var set by the cc-work/cc-personal
// launchers. See ~/.claude-personal/specs/profile-scoping.md.

const SHARED_SCOPE: &str = "shared";
const ALL_SCOPES: &str = "*";

/// Effective scope for a WRITE (add / re-scope): explicit `--scope`, else
/// `$CLAUDE_PROFILE`, else `shared`.
fn resolve_write_scope(flag: Option<&str>) -> String {
    if let Some(s) = flag.filter(|s| !s.is_empty()) {
        return s.to_string();
    }
    std::env::var("CLAUDE_PROFILE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| SHARED_SCOPE.to_string())
}

/// Which scopes a READ should include.
pub(crate) enum ScopeFilter {
    /// No predicate — span every scope.
    All,
    /// `scope IN (...)` — the listed scopes only.
    Include(Vec<String>),
}

impl ScopeFilter {
    /// Expand a resolved scope name into the set of scopes a read should see.
    /// A concrete profile sees itself + `shared`; `shared` sees only `shared`;
    /// `*` (or absent) spans everything.
    fn from_resolved(resolved: Option<&str>) -> ScopeFilter {
        match resolved.filter(|s| !s.is_empty()) {
            None => ScopeFilter::All,
            Some(s) if s == ALL_SCOPES => ScopeFilter::All,
            Some(s) if s == SHARED_SCOPE => ScopeFilter::Include(vec![SHARED_SCOPE.to_string()]),
            Some(s) => ScopeFilter::Include(vec![s.to_string(), SHARED_SCOPE.to_string()]),
        }
    }

    /// Filter for `context` (session start): `--scope`, else `$CLAUDE_PROFILE`,
    /// else span all (back-compat for accounts that don't set CLAUDE_PROFILE).
    fn for_context(flag: Option<&str>) -> ScopeFilter {
        let resolved = flag.map(str::to_string).or_else(|| {
            std::env::var("CLAUDE_PROFILE")
                .ok()
                .filter(|s| !s.is_empty())
        });
        ScopeFilter::from_resolved(resolved.as_deref())
    }

    /// Filter for `recall` / `search`: span all by default, narrow only when
    /// `--scope` is explicitly passed (keep semantic recall global). Shared with
    /// the `kx` retrieval paths.
    pub(crate) fn for_explicit(flag: Option<&str>) -> ScopeFilter {
        ScopeFilter::from_resolved(flag)
    }

    /// A ` AND <col> IN (?, ?)` SQL fragment plus its bound values. Uses
    /// anonymous placeholders, so the caller must bind these values at the
    /// position the fragment appears in the statement. Empty when `All`.
    pub(crate) fn sql_and(&self, col: &str) -> (String, Vec<Value>) {
        match self {
            ScopeFilter::All => (String::new(), Vec::new()),
            ScopeFilter::Include(scopes) => {
                let ph = scopes.iter().map(|_| "?").collect::<Vec<_>>().join(", ");
                let frag = format!(" AND {col} IN ({ph})");
                let vals = scopes.iter().map(|s| Value::Text(s.clone())).collect();
                (frag, vals)
            }
        }
    }
}

/// How a retrieval path constrains `memories.type`. The generic `memory
/// recall`/`search` paths must never surface the `kx` knowledge-index corpus;
/// the `kx` paths pin to a single type. Both route their type predicate through
/// this one helper (clarification #9 / punch-list #5) rather than hand-writing
/// per-command SQL.
pub(crate) enum TypeFilter {
    /// Generic memory: exclude `type = 'kx'`, optionally restrict to one type.
    ExcludeKx(Option<String>),
    /// Pin to exactly this type (the kx corpus passes `"kx"`).
    Only(String),
}

impl TypeFilter {
    /// A ` AND <col> ...` predicate fragment plus its bound values, appended in
    /// statement order like [`ScopeFilter::sql_and`]. `'kx'` is a compile-time
    /// constant (inlined, no injection surface); the optional/pinned type binds.
    fn sql_and(&self, col: &str) -> (String, Vec<Value>) {
        match self {
            TypeFilter::ExcludeKx(None) => (format!(" AND {col} != 'kx'"), Vec::new()),
            TypeFilter::ExcludeKx(Some(t)) => (
                format!(" AND {col} != 'kx' AND {col} = ?"),
                vec![Value::Text(t.clone())],
            ),
            TypeFilter::Only(t) => (format!(" AND {col} = ?"), vec![Value::Text(t.clone())]),
        }
    }
}

pub fn run(conn: Connection, cmd: MemoryCmd) -> Result<()> {
    match cmd.action {
        MemoryAction::Add(args) => add(&conn, args),
        MemoryAction::Update(args) => update(&conn, args),
        MemoryAction::Show(args) => show(&conn, args),
        MemoryAction::Archive(args) => set_lifecycle(&conn, args, "archived"),
        MemoryAction::Trash(args) => set_lifecycle(&conn, args, "trashed"),
        MemoryAction::Restore(args) => set_lifecycle(&conn, args, "active"),
        MemoryAction::Events(args) => events(&conn, args),
        MemoryAction::PatchLog(args) => patch_log(&conn, args),
        MemoryAction::Diff(args) => diff(&conn, args),
        MemoryAction::ApplyPlan(args) => apply_plan(&conn, args),
        MemoryAction::Search(args) => search(&conn, args),
        MemoryAction::Recent(args) => recent(&conn, args),
        MemoryAction::List(args) => list(&conn, args),
        MemoryAction::Context(args) => context(&conn, args),
        MemoryAction::ContextStats(args) => context_stats(&conn, args),
        MemoryAction::RebuildIndex => rebuild_full_index(&conn),
        MemoryAction::Recall(args) => recall(&conn, args),
        MemoryAction::RecallStats(args) => recall_stats(&conn, args),
        MemoryAction::Reindex(args) => reindex(&conn, args),
        MemoryAction::Correct(args) => correct(&conn, args),
        MemoryAction::Retract(args) => retract(&conn, args),
        MemoryAction::Resolve(args) => resolve(&conn, args),
        MemoryAction::Conflicts(args) => conflicts(&conn, args),
        MemoryAction::Sweep(args) => crate::adjudicate::sweep(
            &conn,
            &crate::adjudicate::SweepOpts {
                prose_only: args.prose_only,
                dry_run: args.dry_run,
                limit: args.limit,
                force: args.force,
            },
        ),
        MemoryAction::Kernel(cmd) => kernel(&conn, cmd),
        MemoryAction::Verify(args) => verify(&conn, args),
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
    // e.g. <home>/src/pi-claude-poc → ["pi", "claude", "poc", "src"]
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
                    is_active, lifecycle, created_at, updated_at, verified_at, author
             FROM memories WHERE id = ?1 AND is_active = 1 AND lifecycle = 'active'",
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
    scope: &ScopeFilter,
) -> Result<Vec<(Memory, f64)>> {
    let now_ts = now();

    if signal_terms.is_empty() {
        // No signals — fall back to pure recency
        let memories = query_by_type(conn, type_, limit, scope)?;
        return Ok(memories
            .into_iter()
            .map(|m| {
                let days = ((now_ts - m.updated_at) as f64 / 86400.0).max(0.0);
                let score = 1.0 / (1.0 + days.ln_1p());
                (m, score)
            })
            .collect());
    }

    // Anonymous placeholders bound in statement order: type, terms…, scope…, limit.
    let term_ph = signal_terms
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(", ");
    let (scope_sql, scope_vals) = scope.sql_and("m.scope");

    let sql = format!(
        "SELECT m.id, m.type, m.name, m.description, m.content, m.source,
                m.tags, m.cwd, m.is_active, m.lifecycle, m.created_at, m.updated_at,
                m.verified_at, m.author,
                SUM(mti.weight) AS raw_score
         FROM memories m
         JOIN memory_topic_index mti ON mti.memory_id = m.id
         WHERE m.type = ? AND m.is_active = 1 AND m.lifecycle = 'active'
           AND mti.term IN ({term_ph}){scope_sql}
         GROUP BY m.id
         ORDER BY raw_score DESC
         LIMIT ?"
    );

    let mut dyn_params: Vec<Value> = vec![Value::Text(type_.to_string())];
    for t in signal_terms {
        dyn_params.push(Value::Text(t.clone()));
    }
    dyn_params.extend(scope_vals);
    dyn_params.push(Value::Integer((limit * 4) as i64)); // fetch extra, re-rank after decay

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(dyn_params.iter()), |r| {
        Ok((row_to_memory(r)?, r.get::<_, f64>(14)?))
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
/// Returns None only when none was supplied. A body that WAS supplied but
/// resolves to empty/whitespace-only is an ERROR, never a silent blank write
/// (at-efx: an agent-mcp backend runs with null stdin, so `--content -` from
/// an old forwarding client resolves to "" here — fail loud, don't blank).
fn resolve_content(
    content: Option<String>,
    content_file: Option<PathBuf>,
) -> Result<Option<String>> {
    let resolved = if let Some(path) = content_file {
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("reading --content-file {}", path.display()))?;
        Some(body)
    } else if content.as_deref() == Some("-") {
        let mut buf = String::new();
        std::io::stdin()
            .read_to_string(&mut buf)
            .context("reading content from stdin")?;
        Some(buf)
    } else {
        content
    };
    if resolved.as_deref().is_some_and(|c| c.trim().is_empty()) {
        bail!("memory content resolved to empty/whitespace-only; refusing to store a blank body");
    }
    Ok(resolved)
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
    let scope = resolve_write_scope(args.scope.as_deref());
    let author = args.author.clone().unwrap_or_else(default_author);
    conn.execute(
        "INSERT INTO memories (id, type, name, description, content, source, tags, cwd, scope, is_active, created_at, updated_at, source_ref, author)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 1, ?10, ?10, ?11, ?12)",
        params![id, args.r#type, args.name, args.description, content, args.source, args.tags, args.cwd, scope, ts, args.source_ref, author],
    )?;
    rebuild_index_for(conn, &id)?;
    let text = embed::memory_embed_text(&args.name, &args.description, &content);
    embed::try_embed_one(conn, &id, &text);
    let after = memory_row_json(conn, &id)?;
    conn.execute(
        "INSERT INTO memory_events (ts, actor, action, memory_id, before_json, after_json)
         VALUES (?1, 'agent', 'add', ?2, NULL, ?3)",
        params![ts, id, after],
    )?;
    // gf2.7: the add has fully succeeded — adjudication runs after and
    // can only ADD information (edges / queue rows), never fail the add.
    if !args.no_adjudicate {
        crate::adjudicate::maybe_adjudicate(conn, &id);
    }
    println!("{id}");
    Ok(())
}

fn print_memory(memory: &Memory) {
    println!(
        "[{}] ({}) {} — {}  [{}]",
        memory.id,
        memory.type_,
        memory.name,
        memory.description,
        fmt_ts(memory.updated_at)
    );
    println!("  source: {}", memory.source);
    if !memory.tags.is_empty() {
        println!("  tags: {}", memory.tags);
    }
    if !memory.cwd.is_empty() {
        println!("  cwd: {}", memory.cwd);
    }
    println!("  active: {}", memory.is_active);
    println!("  lifecycle: {}", memory.lifecycle);
    println!("  created: {}", fmt_ts(memory.created_at));
    println!("  updated: {}", fmt_ts(memory.updated_at));
    println!();
    println!("{}", memory.content);
}

fn memory_to_json(memory: &Memory) -> serde_json::Value {
    serde_json::json!({
        "id": memory.id,
        "type": memory.type_,
        "name": memory.name,
        "description": memory.description,
        "content": memory.content,
        "source": memory.source,
        "tags": memory.tags,
        "cwd": memory.cwd,
        "is_active": memory.is_active,
        "lifecycle": memory.lifecycle,
        "created_at": memory.created_at,
        "updated_at": memory.updated_at,
    })
}

fn update(conn: &Connection, args: UpdateArgs) -> Result<()> {
    let ts = now();
    let id = args.id.clone();
    let before = memory_row_json(conn, &id)?;
    let mut updated = 0;
    let content = resolve_content(args.content, args.content_file)?;

    if let Some(name) = args.name {
        updated += conn.execute(
            "UPDATE memories SET name=?1, updated_at=?2 WHERE id=?3",
            params![name, ts, id],
        )?;
    }
    if let Some(desc) = args.description {
        updated += conn.execute(
            "UPDATE memories SET description=?1, updated_at=?2 WHERE id=?3",
            params![desc, ts, id],
        )?;
    }
    if let Some(content) = content {
        updated += conn.execute(
            "UPDATE memories SET content=?1, updated_at=?2 WHERE id=?3",
            params![content, ts, id],
        )?;
    }
    if let Some(tags) = args.tags {
        updated += conn.execute(
            "UPDATE memories SET tags=?1, updated_at=?2 WHERE id=?3",
            params![tags, ts, id],
        )?;
    }
    if let Some(active) = args.active {
        updated += conn.execute(
            "UPDATE memories SET is_active=?1, updated_at=?2 WHERE id=?3",
            params![active as i64, ts, id],
        )?;
    }
    if let Some(scope) = args.scope {
        updated += conn.execute(
            "UPDATE memories SET scope=?1, updated_at=?2 WHERE id=?3",
            params![scope, ts, args.id],
        )?;
    }

    if updated == 0 {
        bail!("no memory found with id={}", id);
    }

    rebuild_index_for(conn, &id)?;
    let after = memory_row_json(conn, &id)?;
    conn.execute(
        "INSERT INTO memory_events (ts, actor, action, memory_id, before_json, after_json)
         VALUES (?1, 'agent', 'update', ?2, ?3, ?4)",
        params![ts, id, before, after],
    )?;

    // Re-embed from the post-update state
    let row: Option<(String, String, String)> = conn
        .query_row(
            "SELECT name, description, content FROM memories WHERE id = ?1 AND is_active = 1",
            params![id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();
    if let Some((name, desc, content)) = row {
        let text = embed::memory_embed_text(&name, &desc, &content);
        embed::try_embed_one(conn, &args.id, &text);
    }

    Ok(())
}

fn show(conn: &Connection, args: ShowArgs) -> Result<()> {
    let active_clause = if args.inactive {
        ""
    } else {
        " AND is_active = 1"
    };
    let sql = format!(
        "SELECT id, type, name, description, content, source, tags, cwd, is_active, lifecycle, created_at, updated_at, verified_at, author
         FROM memories
         WHERE (id = ?1 OR name = ?1){active_clause}
         ORDER BY CASE WHEN id = ?1 THEN 0 ELSE 1 END, updated_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let memories = collect_memories(stmt.query_map(params![args.key], row_to_memory)?)?;

    match memories.len() {
        0 => bail!("no memory found for id/name={}", args.key),
        1 => {
            let memory = &memories[0];
            if args.json {
                println!("{}", memory_to_json(memory));
            } else {
                print_memory(memory);
                // at-usl: trust metadata (v8 columns live outside the Memory
                // struct to keep its many consumers untouched).
                if let Ok((verified_at, source_ref, superseded_by, reason, author)) = conn
                    .query_row(
                        "SELECT verified_at, source_ref, superseded_by, supersede_reason, author
                         FROM memories WHERE id = ?1",
                        params![memory.id],
                        |r| {
                            Ok((
                                r.get::<_, Option<i64>>(0)?,
                                r.get::<_, Option<String>>(1)?,
                                r.get::<_, Option<String>>(2)?,
                                r.get::<_, Option<String>>(3)?,
                                r.get::<_, Option<String>>(4)?,
                            ))
                        },
                    )
                {
                    println!();
                    println!(
                        "trust: {}",
                        trust_label(
                            memory.created_at,
                            verified_at,
                            &memory.lifecycle,
                            superseded_by.as_deref(),
                            author.as_deref().unwrap_or("")
                        )
                    );
                    if let Some(sr) = source_ref.filter(|v| !v.is_empty()) {
                        println!("source_ref: {sr}");
                    }
                    if let Some(rs) = reason.filter(|v| !v.is_empty()) {
                        println!("supersede_reason: {rs}");
                    }
                    if let Some(succ) = superseded_by {
                        println!();
                        // gf2.5: show the chain HEAD, not a possibly-stale
                        // middle link.
                        let hops = resolve_current(conn, &succ)?;
                        let head = hops.last().map(String::as_str).unwrap_or(&succ);
                        print_successor(conn, head)?;
                        if head != succ {
                            println!(
                                "  (resolved through {} superseded link(s): {} → {})",
                                hops.len(),
                                succ,
                                hops.join(" → ")
                            );
                        }
                    }
                }
            }
        }
        _ => {
            if args.json {
                let values: Vec<serde_json::Value> = memories.iter().map(memory_to_json).collect();
                println!("{}", serde_json::json!({ "matches": values }));
            } else {
                println!(
                    "multiple memories matched id/name={}; use exact id",
                    args.key
                );
                for memory in &memories {
                    println!(
                        "[{}] ({}) {} — {}  [{}]",
                        memory.id,
                        memory.type_,
                        memory.name,
                        memory.description,
                        fmt_ts(memory.updated_at)
                    );
                }
            }
        }
    }
    Ok(())
}

fn memory_row_json(conn: &Connection, id: &str) -> Result<Option<String>> {
    let sql = "SELECT json_object(
            'id', id,
            'type', type,
            'name', name,
            'description', description,
            'content', content,
            'source', source,
            'tags', tags,
            'cwd', cwd,
            'is_active', is_active,
            'lifecycle', lifecycle,
            'created_at', created_at,
            'updated_at', updated_at,
            'archived_at', archived_at,
            'trashed_at', trashed_at,
            'purge_after', purge_after,
            'superseded_by', superseded_by,
            'supersede_reason', supersede_reason,
            'verified_at', verified_at,
            'source_ref', source_ref,
            'author', author
        ) FROM memories WHERE id = ?1";
    match conn.query_row(sql, params![id], |r| r.get::<_, String>(0)) {
        Ok(json) => Ok(Some(json)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

fn set_lifecycle(conn: &Connection, args: LifecycleArgs, lifecycle: &str) -> Result<()> {
    let before = memory_row_json(conn, &args.id)?
        .ok_or_else(|| anyhow::anyhow!("no memory found with id={}", args.id))?;
    let ts = now();

    let (action, changed) = match lifecycle {
        // gf2.12: restore must also clear the supersession FK fields —
        // an active row still pointing at a "successor" lies to raw
        // readers, and a later supersession would silently overwrite the
        // stale pointer. The supersessions edge row is deliberately KEPT
        // (it records that a supersession happened); the reversal itself
        // is auditable via this call's before/after memory_events json.
        "active" => (
            "restore",
            conn.execute(
                "UPDATE memories
                 SET lifecycle='active', is_active=1, archived_at=NULL, trashed_at=NULL,
                     purge_after=NULL, superseded_by=NULL, supersede_reason=NULL,
                     updated_at=?1
                 WHERE id=?2",
                params![ts, args.id],
            )?,
        ),
        "archived" => (
            "archive",
            conn.execute(
                "UPDATE memories
                 SET lifecycle='archived', is_active=0, archived_at=?1, trashed_at=NULL,
                     updated_at=?1
                 WHERE id=?2",
                params![ts, args.id],
            )?,
        ),
        "trashed" => (
            "trash",
            conn.execute(
                "UPDATE memories
                 SET lifecycle='trashed', is_active=0, trashed_at=?1, updated_at=?1
                 WHERE id=?2",
                params![ts, args.id],
            )?,
        ),
        other => bail!("invalid lifecycle: {other}"),
    };

    if changed == 0 {
        bail!("no memory found with id={}", args.id);
    }

    rebuild_index_for(conn, &args.id)?;
    let after = memory_row_json(conn, &args.id)?;
    conn.execute(
        "INSERT INTO memory_events (ts, actor, action, memory_id, before_json, after_json, reason, source_report)
         VALUES (?1, 'agent', ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            ts,
            action,
            args.id,
            before,
            after,
            args.reason,
            args.source_report
        ],
    )?;
    println!("{action}: {}", args.id);
    Ok(())
}

fn events(conn: &Connection, args: EventsArgs) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, actor, action, before_json, after_json, reason, source_report
         FROM memory_events
         WHERE memory_id = ?1
         ORDER BY ts DESC, id DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![args.id, args.limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, Option<String>>(4)?,
            r.get::<_, Option<String>>(5)?,
            r.get::<_, String>(6)?,
            r.get::<_, String>(7)?,
        ))
    })?;

    let mut values = Vec::new();
    for row in rows {
        let (id, ts, actor, action, before_json, after_json, reason, source_report) = row?;
        if args.json {
            values.push(serde_json::json!({
                "id": id,
                "ts": ts,
                "actor": actor,
                "action": action,
                "before_json": before_json,
                "after_json": after_json,
                "reason": reason,
                "source_report": source_report,
            }));
        } else {
            println!("[{id}] {} {action} by {actor}", fmt_ts(ts));
            if !reason.is_empty() {
                println!("  reason: {reason}");
            }
            if !source_report.is_empty() {
                println!("  source: {source_report}");
            }
        }
    }

    if args.json {
        println!("{}", serde_json::json!({ "events": values }));
    }
    Ok(())
}

fn patch_log(conn: &Connection, args: EventsArgs) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT id, ts, actor, action, reason, source_report
         FROM memory_events
         WHERE memory_id = ?1
         ORDER BY ts DESC, id DESC
         LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![args.id, args.limit as i64], |r| {
        Ok((
            r.get::<_, i64>(0)?,
            r.get::<_, i64>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
        ))
    })?;

    let mut values = Vec::new();
    for row in rows {
        let (event_id, ts, actor, action, reason, source_report) = row?;
        let diff_cmd = format!("agent memory diff {} --event {}", args.id, event_id);
        if args.json {
            values.push(serde_json::json!({
                "event_id": event_id,
                "ts": ts,
                "actor": actor,
                "action": action,
                "reason": reason,
                "source_report": source_report,
                "diff_command": diff_cmd,
            }));
        } else {
            println!("[{event_id}] {} {action} by {actor}", fmt_ts(ts));
            if !reason.is_empty() {
                println!("  reason: {reason}");
            }
            if !source_report.is_empty() {
                println!("  source: {source_report}");
            }
            println!("  diff: {diff_cmd}");
        }
    }

    if args.json {
        println!(
            "{}",
            serde_json::json!({ "memory_id": args.id, "patches": values })
        );
    }
    Ok(())
}

fn apply_plan(conn: &Connection, args: ApplyPlanArgs) -> Result<()> {
    let raw = std::fs::read_to_string(&args.file)?;
    let plan: ApplyPlan = serde_json::from_str(&raw)?;
    let selected: BTreeSet<String> = args.select.iter().cloned().collect();

    if !args.dry_run && selected.is_empty() {
        bail!("apply-plan requires at least one --select unless --dry-run is set");
    }

    let plan_ids: BTreeSet<String> = plan.candidates.iter().map(|c| c.id.clone()).collect();
    for id in &selected {
        if !plan_ids.contains(id) {
            bail!("selected candidate not found in plan: {id}");
        }
    }

    for candidate in &plan.candidates {
        if selected.is_empty() || selected.contains(&candidate.id) {
            if args.dry_run {
                print_apply_candidate(candidate);
            } else {
                apply_candidate(conn, candidate)?;
            }
        }
    }

    Ok(())
}

fn print_apply_candidate(candidate: &ApplyCandidate) {
    println!("{}: {}", candidate.id, candidate.action);
    if let Some(memory_id) = &candidate.memory_id {
        println!("  memory_id: {memory_id}");
    }
    if let Some(name) = &candidate.name {
        println!("  name: {name}");
    }
    if let Some(description) = &candidate.description {
        println!("  description: {description}");
    }
    if let Some(reason) = &candidate.reason {
        if !reason.is_empty() {
            println!("  reason: {reason}");
        }
    }
    if let Some(source_report) = &candidate.source_report {
        if !source_report.is_empty() {
            println!("  source: {source_report}");
        }
    }
}

fn required<'a>(
    value: &'a Option<String>,
    candidate: &ApplyCandidate,
    field: &str,
) -> Result<&'a str> {
    value.as_deref().ok_or_else(|| {
        anyhow::anyhow!(
            "candidate {} action {} missing {field}",
            candidate.id,
            candidate.action
        )
    })
}

fn apply_candidate(conn: &Connection, candidate: &ApplyCandidate) -> Result<()> {
    match candidate.action.as_str() {
        "add" => add(
            conn,
            AddArgs {
                r#type: required(&candidate.type_, candidate, "type")?.to_string(),
                name: required(&candidate.name, candidate, "name")?.to_string(),
                description: required(&candidate.description, candidate, "description")?
                    .to_string(),
                content: Some(required(&candidate.content, candidate, "content")?.to_string()),
                content_file: None,
                tags: candidate.tags.clone().unwrap_or_default(),
                cwd: candidate.cwd.clone().unwrap_or_default(),
                source: candidate
                    .source
                    .clone()
                    .unwrap_or_else(|| "apply-plan".into()),
                scope: candidate.scope.clone(),
                source_ref: candidate.source_report.clone(),
                author: None,
                no_adjudicate: true,
            },
        ),
        "update" => update(
            conn,
            UpdateArgs {
                id: required(&candidate.memory_id, candidate, "memory_id")?.to_string(),
                name: candidate.name.clone(),
                description: candidate.description.clone(),
                content: candidate.content.clone(),
                content_file: None,
                tags: candidate.tags.clone(),
                active: candidate.active,
                scope: candidate.scope.clone(),
            },
        ),
        "archive" => set_lifecycle(conn, lifecycle_args_from_candidate(candidate)?, "archived"),
        "trash" => set_lifecycle(conn, lifecycle_args_from_candidate(candidate)?, "trashed"),
        "restore" => set_lifecycle(conn, lifecycle_args_from_candidate(candidate)?, "active"),
        other => bail!("candidate {} has unsupported action: {other}", candidate.id),
    }
}

fn lifecycle_args_from_candidate(candidate: &ApplyCandidate) -> Result<LifecycleArgs> {
    Ok(LifecycleArgs {
        id: required(&candidate.memory_id, candidate, "memory_id")?.to_string(),
        reason: candidate.reason.clone().unwrap_or_default(),
        source_report: candidate.source_report.clone().unwrap_or_default(),
    })
}

fn parse_event_json(raw: Option<String>) -> Result<serde_json::Value> {
    match raw {
        Some(s) => Ok(serde_json::from_str(&s)?),
        None => Ok(serde_json::Value::Null),
    }
}

fn value_for_key<'a>(value: &'a serde_json::Value, key: &str) -> &'a serde_json::Value {
    value
        .as_object()
        .and_then(|obj| obj.get(key))
        .unwrap_or(&serde_json::Value::Null)
}

fn diff(conn: &Connection, args: DiffArgs) -> Result<()> {
    let sql = if args.event.is_some() {
        "SELECT id, ts, actor, action, before_json, after_json, reason, source_report
         FROM memory_events
         WHERE memory_id = ?1 AND id = ?2"
    } else {
        "SELECT id, ts, actor, action, before_json, after_json, reason, source_report
         FROM memory_events
         WHERE memory_id = ?1
         ORDER BY ts DESC, id DESC
         LIMIT 1"
    };

    let event = if let Some(event_id) = args.event {
        conn.query_row(sql, params![args.id, event_id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })
    } else {
        conn.query_row(sql, params![args.id], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, Option<String>>(5)?,
                r.get::<_, String>(6)?,
                r.get::<_, String>(7)?,
            ))
        })
    }
    .map_err(|e| match e {
        rusqlite::Error::QueryReturnedNoRows => anyhow::anyhow!("no memory event found"),
        other => anyhow::Error::from(other),
    })?;

    let (event_id, ts, actor, action, before_json, after_json, reason, source_report) = event;
    let before = parse_event_json(before_json)?;
    let after = parse_event_json(after_json)?;

    let mut keys = BTreeSet::new();
    if let Some(obj) = before.as_object() {
        keys.extend(obj.keys().cloned());
    }
    if let Some(obj) = after.as_object() {
        keys.extend(obj.keys().cloned());
    }

    let mut changes = Vec::new();
    for key in keys {
        let old = value_for_key(&before, &key);
        let new = value_for_key(&after, &key);
        if old != new {
            changes.push((key, old.clone(), new.clone()));
        }
    }

    if args.json {
        let changes_json: Vec<serde_json::Value> = changes
            .iter()
            .map(|(field, before, after)| {
                serde_json::json!({
                    "field": field,
                    "before": before,
                    "after": after,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "event_id": event_id,
                "memory_id": args.id,
                "ts": ts,
                "actor": actor,
                "action": action,
                "reason": reason,
                "source_report": source_report,
                "changes": changes_json,
            })
        );
        return Ok(());
    }

    println!(
        "memory {} event {event_id}: {action} by {actor} ({})",
        args.id,
        fmt_ts(ts)
    );
    if !reason.is_empty() {
        println!("reason: {reason}");
    }
    if !source_report.is_empty() {
        println!("source: {source_report}");
    }
    println!();

    if changes.is_empty() {
        println!("(no field changes)");
    } else {
        for (field, before, after) in changes {
            println!("{field}:");
            println!("- {}", before);
            println!("+ {}", after);
        }
    }

    Ok(())
}

// ── Queries ───────────────────────────────────────────────────────────────────

/// One row of an FTS lexical search — the projection [`search_rows`] returns.
/// `valid_from` rides along for the `kx` renderers (doc date); the `memory`
/// renderer ignores it.
pub(crate) struct SearchRow {
    pub(crate) id: String,
    pub(crate) type_: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) content: String,
    pub(crate) tags: String,
    pub(crate) updated_at: i64,
    pub(crate) created_at: i64,
    pub(crate) verified_at: Option<i64>,
    pub(crate) lifecycle: String,
    pub(crate) superseded_by: Option<String>,
    pub(crate) author: Option<String>,
    pub(crate) valid_from: Option<i64>,
}

/// Shared FTS5 retrieval used by BOTH `memory search` and `kx search`
/// (punch-list #5). Runs the neutralized MATCH query with the given
/// type/scope/author filters plus an extra predicate fragment (`kx` tag/date),
/// in FTS `rank` order. Only the filters + rendering differ per caller.
///
/// `extra_sql` is appended verbatim to the WHERE clause and must reference the
/// `m` alias; `extra_vals` binds its `?` placeholders in order.
#[allow(clippy::too_many_arguments)] // orthogonal filters; a struct would only shift the noise
pub(crate) fn search_rows(
    conn: &Connection,
    query: &str,
    type_filter: &TypeFilter,
    scope: &ScopeFilter,
    author: Option<&str>,
    extra_sql: &str,
    extra_vals: &[Value],
    limit: usize,
) -> Result<Vec<SearchRow>> {
    // The raw query is free text. FTS5 treats `-`, `:`, `*`, `^`, `(`, `)` and
    // the bareword operators AND/OR/NOT as query syntax, so an unescaped query
    // like `mu-slat` errored (`no such column: slat`) and silently returned
    // nothing. fts5_match_query() neutralizes that — see its doc comment.
    let match_query = fts5_match_query(query);
    if match_query.is_empty() {
        return Ok(Vec::new());
    }

    let (type_sql, type_vals) = type_filter.sql_and("m.type");
    let (scope_sql, scope_vals) = scope.sql_and("m.scope");
    let (author_sql, author_vals) = match author {
        Some(a) => (
            " AND m.author = ?".to_string(),
            vec![Value::Text(a.to_string())],
        ),
        None => (String::new(), Vec::new()),
    };
    let sql = format!(
        "SELECT m.id, m.type, m.name, m.description, m.content, m.tags, m.updated_at,
                m.created_at, m.verified_at, m.lifecycle, m.superseded_by, m.author, m.valid_from
         FROM memories_fts fts
         JOIN memories m ON m.rowid = fts.rowid
         WHERE memories_fts MATCH ? AND m.is_active = 1{type_sql}{author_sql}{scope_sql}{extra_sql}
         ORDER BY rank
         LIMIT ?"
    );

    let mut p: Vec<Value> = vec![Value::Text(match_query)];
    p.extend(type_vals);
    p.extend(author_vals);
    p.extend(scope_vals);
    p.extend(extra_vals.iter().cloned());
    p.push(Value::Integer(limit as i64));

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(p.iter()), |r| {
        Ok(SearchRow {
            id: r.get(0)?,
            type_: r.get(1)?,
            name: r.get(2)?,
            description: r.get(3)?,
            content: r.get(4)?,
            tags: r.get(5)?,
            updated_at: r.get(6)?,
            created_at: r.get(7)?,
            verified_at: r.get(8)?,
            lifecycle: r.get(9)?,
            superseded_by: r.get(10)?,
            author: r.get(11)?,
            valid_from: r.get(12)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

fn search(conn: &Connection, args: SearchArgs) -> Result<()> {
    let scope = ScopeFilter::for_explicit(args.scope.as_deref());
    let type_filter = TypeFilter::ExcludeKx(args.r#type.clone());
    let rows = search_rows(
        conn,
        &args.query,
        &type_filter,
        &scope,
        args.author.as_deref(),
        "",
        &[],
        args.limit,
    )?;

    for row in rows {
        let SearchRow {
            id,
            type_,
            name,
            description: desc,
            content,
            tags,
            updated_at,
            created_at,
            verified_at,
            lifecycle,
            superseded_by,
            author,
            valid_from: _,
        } = row;
        let ts = fmt_ts(updated_at);
        // at-usl: a superseded memory is never shown without its successor —
        // successor first with full content, the stale entry as a labeled stub.
        // gf2.5: the successor shown is the CHAIN HEAD, not the direct
        // successor — a middle link of A→B→C is itself stale, and showing
        // it as "CURRENT" reintroduces the very failure supersession fixes.
        if lifecycle == "superseded" {
            if let Some(succ_id) = superseded_by.as_deref() {
                let hops = resolve_current(conn, succ_id)?;
                let head = hops.last().map(String::as_str).unwrap_or(succ_id);
                print_successor(conn, head)?;
                if head != succ_id {
                    println!(
                        "  (resolved through {} superseded link(s): {} → {})",
                        hops.len(),
                        succ_id,
                        hops.join(" → ")
                    );
                }
            }
            println!(
                "[{id}] ({type_}) {name} — {desc}  [{ts}]\n  {}",
                trust_label(
                    created_at,
                    verified_at,
                    &lifecycle,
                    superseded_by.as_deref(),
                    author.as_deref().unwrap_or("")
                )
            );
            println!();
            continue;
        }
        println!("[{id}] ({type_}) {name} — {desc}  [{ts}]");
        println!(
            "  {}",
            trust_label(
                created_at,
                verified_at,
                &lifecycle,
                superseded_by.as_deref(),
                author.as_deref().unwrap_or("")
            )
        );
        if !tags.is_empty() {
            println!("  tags: {tags}");
        }
        println!("{}", indent(&content, "  "));
        println!();
    }
    Ok(())
}

/// at-usl: render the successor of a superseded memory, full content,
/// clearly marked as the fact currently in force.
fn print_successor(conn: &Connection, succ_id: &str) -> Result<()> {
    let row = conn.query_row(
        "SELECT id, type, name, description, content, created_at, verified_at, lifecycle, superseded_by, author
         FROM memories WHERE id = ?1",
        params![succ_id],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<i64>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, Option<String>>(8)?,
                r.get::<_, Option<String>>(9)?,
            ))
        },
    );
    match row {
        Ok((
            id,
            type_,
            name,
            desc,
            content,
            created_at,
            verified_at,
            lifecycle,
            superseded_by,
            author,
        )) => {
            println!("[{id}] ({type_}) {name} — {desc}  [CURRENT — supersedes the entry below]");
            println!(
                "  {}",
                trust_label(
                    created_at,
                    verified_at,
                    &lifecycle,
                    superseded_by.as_deref(),
                    author.as_deref().unwrap_or("")
                )
            );
            println!("{}", indent(&content, "  "));
            println!();
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => {
            println!("  (successor {succ_id} not found — ORPHANED supersession)");
        }
        Err(e) => return Err(e.into()),
    }
    Ok(())
}

/// gf2.5: maximum supersession-chain hops before we declare the data
/// pathological. Chains are human-made and short; 16 is far beyond any
/// legitimate history.
const RESOLVE_MAX_DEPTH: usize = 16;

/// gf2.5: follow `superseded_by` (the primary-successor fast path) to
/// the terminal head. Returns the hop list AFTER `start` (empty = start
/// is not superseded / already the head). Errors on a cycle or a chain
/// deeper than RESOLVE_MAX_DEPTH — both mean corrupted supersession
/// data and must be loud, not silently truncated.
fn resolve_current(conn: &Connection, start: &str) -> Result<Vec<String>> {
    let mut hops: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    seen.insert(start.to_string());
    let mut cur = start.to_string();
    loop {
        let next: Option<String> = conn
            .query_row(
                "SELECT superseded_by FROM memories WHERE id = ?1",
                params![cur],
                |r| r.get(0),
            )
            .unwrap_or(None);
        match next {
            None => return Ok(hops),
            Some(n) => {
                if !seen.insert(n.clone()) {
                    bail!(
                        "supersession CYCLE at {n} (path: {start} → {})",
                        hops.join(" → ")
                    );
                }
                if hops.len() >= RESOLVE_MAX_DEPTH {
                    bail!("supersession chain from {start} exceeds {RESOLVE_MAX_DEPTH} hops");
                }
                hops.push(n.clone());
                cur = n;
            }
        }
    }
}

/// at-usl: testimony label shown on every read path. Memories are
/// testimony with dates, not ground truth — this is where the dates live.
fn trust_label(
    created_at: i64,
    verified_at: Option<i64>,
    lifecycle: &str,
    superseded_by: Option<&str>,
    author: &str,
) -> String {
    let mut parts = vec![format!("recorded {}", fmt_ts(created_at))];
    match verified_at {
        Some(ts) => parts.push(format!("verified {}", fmt_ts(ts))),
        None => parts.push("never verified".to_string()),
    }
    if !author.is_empty() {
        parts.push(format!("by {author}"));
    }
    if lifecycle == "superseded" {
        match superseded_by {
            Some(succ) => parts.push(format!("SUPERSEDED by {succ}")),
            None => parts.push("SUPERSEDED".to_string()),
        }
    } else if lifecycle == "orphaned" {
        parts.push("ORPHANED — source no longer resolvable".to_string());
    }
    parts.join(" · ")
}

/// at-baj: trust_label for a Memory row — the context injection path uses
/// this. superseded_by is None by construction: every query that produces a
/// Memory filters lifecycle = 'active', so a superseded row can't get here.
fn memory_trust_label(m: &Memory) -> String {
    trust_label(m.created_at, m.verified_at, &m.lifecycle, None, &m.author)
}

fn default_author() -> String {
    std::env::var("AGENT_AUTHOR")
        .or_else(|_| std::env::var("CLAUDE_PROFILE"))
        .unwrap_or_default()
}

/// at-usl: mark OLD superseded by NEW. The read paths take it from here —
/// search shows the successor inline; recall drops the stale entry.
fn correct(conn: &Connection, args: CorrectArgs) -> Result<()> {
    apply_supersession(
        conn,
        &args.old,
        &args.with_id,
        &args.kind,
        &args.reason,
        1.0,
        &default_author(),
    )?;
    log::info!(
        "{} superseded by {} ({})",
        args.old,
        args.with_id,
        args.kind
    );
    Ok(())
}

/// gf2.7: the one supersession effector — shared by the manual `correct`
/// verb (confidence 1.0, the invoking author) and the write-time
/// adjudicator (its model confidence, actor "adjudicator"). FK fast path +
/// typed edge + validity closure + topic-index drop + audit event:
/// identical effects regardless of who decided.
pub(crate) fn apply_supersession(
    conn: &Connection,
    old: &str,
    new: &str,
    kind: &str,
    reason: &str,
    confidence: f64,
    actor: &str,
) -> Result<()> {
    if old == new {
        bail!("a memory cannot supersede itself");
    }
    let before = memory_row_json(conn, old)?
        .ok_or_else(|| anyhow::anyhow!("no memory found with id={old}"))?;
    memory_row_json(conn, new)?
        .ok_or_else(|| anyhow::anyhow!("no successor memory with id={new}"))?;
    let ts = now();
    conn.execute(
        "UPDATE memories
         SET superseded_by=?1, supersede_reason=?2, lifecycle='superseded', updated_at=?3
         WHERE id=?4",
        params![new, reason, ts, old],
    )?;
    // gf2.2: the typed edge is the full relation (the FK above stays as
    // the fast path to the primary successor). OR IGNORE: re-asserting
    // an existing pair is idempotent (the change history lives in
    // memory_events, not in duplicate edges).
    conn.execute(
        "INSERT OR IGNORE INTO supersessions (old_id, new_id, kind, reason, confidence, actor, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![old, new, kind, reason, confidence, actor, ts],
    )?;
    // 'updates' = the world changed: the old fact WAS true — close its
    // validity interval instead of leaving it ambiguous. 'corrects' = it
    // was never true; the interval stays untouched (there is no era in
    // which it held).
    if kind == "updates" {
        conn.execute(
            "UPDATE memories SET valid_to = COALESCE(valid_to, ?1) WHERE id = ?2",
            params![ts, old],
        )?;
    }
    // lifecycle != 'active' drops it from the topic index (context injection).
    rebuild_index_for(conn, old)?;
    let after = memory_row_json(conn, old)?;
    conn.execute(
        "INSERT INTO memory_events (ts, actor, action, memory_id, before_json, after_json, reason)
         VALUES (?1, ?2, 'supersede', ?3, ?4, ?5, ?6)",
        params![ts, actor, old, before, after, reason],
    )?;
    // gf2.8: an edge settles the question — close any matching open
    // queue row (either orientation: the adjudicator queues new-corrects-
    // old, but the operator may correct in the opposite direction).
    conn.execute(
        "UPDATE conflict_suspected SET status='resolved'
         WHERE status='open'
           AND ((old_id=?1 AND new_id=?2) OR (old_id=?2 AND new_id=?1))",
        params![old, new],
    )?;
    Ok(())
}

/// gf2.2: AGM contraction — "that's no longer true" with no successor.
/// The FK model can't express it (superseded_by demands a new row); the
/// edge table can (new_id NULL, kind 'retracts'). Hidden everywhere
/// (is_active=0 like trash) but restorable via `restore`.
fn retract(conn: &Connection, args: RetractArgs) -> Result<()> {
    if args.reason.trim().is_empty() {
        bail!("retract requires a non-empty --reason");
    }
    let before = memory_row_json(conn, &args.id)?
        .ok_or_else(|| anyhow::anyhow!("no memory found with id={}", args.id))?;
    let ts = now();
    conn.execute(
        "UPDATE memories
         SET lifecycle='retracted', is_active=0, updated_at=?1
         WHERE id=?2",
        params![ts, args.id],
    )?;
    conn.execute(
        "INSERT INTO supersessions (old_id, new_id, kind, reason, confidence, actor, created_at)
         VALUES (?1, NULL, 'retracts', ?2, 1.0, ?3, ?4)",
        params![args.id, args.reason, default_author(), ts],
    )?;
    // gf2.8: retraction settles any open suspicion touching this memory.
    conn.execute(
        "UPDATE conflict_suspected SET status='resolved'
         WHERE status='open' AND (old_id=?1 OR new_id=?1)",
        params![args.id],
    )?;
    rebuild_index_for(conn, &args.id)?;
    let after = memory_row_json(conn, &args.id)?;
    conn.execute(
        "INSERT INTO memory_events (ts, actor, action, memory_id, before_json, after_json, reason)
         VALUES (?1, 'agent', 'retract', ?2, ?3, ?4, ?5)",
        params![ts, args.id, before, after, args.reason],
    )?;
    log::info!("{} retracted", args.id);
    Ok(())
}

/// gf2.5: `agent memory resolve <id|name>` — walk the supersession
/// chain to its head, showing each hop's kind/reason from the edge
/// table. Turns "superseded, silence" into "superseded; current is X".
fn resolve(conn: &Connection, args: ResolveArgs) -> Result<()> {
    let start: String = conn
        .query_row(
            "SELECT id FROM memories WHERE id = ?1 OR name = ?1
             ORDER BY CASE WHEN id = ?1 THEN 0 ELSE 1 END LIMIT 1",
            params![args.key],
            |r| r.get(0),
        )
        .map_err(|_| anyhow::anyhow!("no memory found for id/name={}", args.key))?;
    let hops = resolve_current(conn, &start)?;
    let head = hops.last().cloned().unwrap_or_else(|| start.clone());

    // Annotate each hop with the typed edge, where one exists (edges
    // older than schema v9 have FK-only supersessions — kind unknown).
    let mut edges: Vec<(String, String, Option<String>, Option<String>)> = Vec::new();
    let mut from = start.clone();
    for to in &hops {
        let edge: Option<(String, String)> = conn
            .query_row(
                "SELECT kind, reason FROM supersessions WHERE old_id = ?1 AND new_id = ?2",
                params![from, to],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .ok();
        let (kind, reason) = match edge {
            Some((k, r)) => (Some(k), Some(r).filter(|s| !s.is_empty())),
            None => (None, None),
        };
        edges.push((from.clone(), to.clone(), kind, reason));
        from = to.clone();
    }

    if args.json {
        let hops_json: Vec<serde_json::Value> = edges
            .iter()
            .map(|(f, t, k, r)| serde_json::json!({"from": f, "to": t, "kind": k, "reason": r}))
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "start": start,
                "head": head,
                "depth": hops.len(),
                "current": hops.is_empty(),
                "hops": hops_json,
            })
        );
        return Ok(());
    }

    if hops.is_empty() {
        println!("{start} is current (no supersession)");
        return Ok(());
    }
    println!("{start}");
    for (_, to, kind, reason) in &edges {
        let kind = kind.as_deref().unwrap_or("superseded-by (untyped, pre-v9)");
        match reason {
            Some(r) => println!("  → {kind} → {to}  ({r})"),
            None => println!("  → {kind} → {to}"),
        }
    }
    println!("current: {head}");
    Ok(())
}

/// gf2.8: the suspected-conflict queue — list what the adjudicator /
/// sweep parked, dismiss false positives. Resolution happens through
/// the normal verbs (`correct` / `retract` close matching rows).
fn conflicts(conn: &Connection, args: ConflictsArgs) -> Result<()> {
    if let Some(row_id) = args.dismiss {
        let (old_id, new_id): (String, String) = conn
            .query_row(
                "SELECT old_id, new_id FROM conflict_suspected WHERE id=?1 AND status='open'",
                params![row_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|_| anyhow::anyhow!("no open conflict row with id={row_id}"))?;
        conn.execute(
            "UPDATE conflict_suspected SET status='dismissed' WHERE id=?1",
            params![row_id],
        )?;
        let ts = now();
        conn.execute(
            "INSERT INTO memory_events (ts, actor, action, memory_id, before_json, after_json, reason)
             VALUES (?1, ?2, 'conflict-dismiss', ?3, NULL, ?4, ?5)",
            params![
                ts,
                default_author(),
                old_id,
                format!("{{\"row\":{row_id},\"new_id\":\"{new_id}\"}}"),
                args.reason
            ],
        )?;
        log::info!("conflict {row_id} ({old_id} vs {new_id}) dismissed");
        return Ok(());
    }

    let status_clause = if args.all {
        ""
    } else {
        " WHERE c.status='open'"
    };
    let sql = format!(
        "SELECT c.id, c.old_id, c.new_id, c.relation, c.confidence, c.rationale,
                c.status, c.created_at,
                mo.name, mn.name
         FROM conflict_suspected c
         JOIN memories mo ON mo.id = c.old_id
         JOIN memories mn ON mn.id = c.new_id{status_clause}
         ORDER BY c.created_at DESC"
    );
    let mut stmt = conn.prepare(&sql)?;
    // One row of the suspected-conflict queue:
    // (id, old_id, new_id, relation, confidence, rationale, status,
    //  created_at, old_name, new_name).
    type ConflictRow = (
        i64,
        String,
        String,
        String,
        Option<f64>,
        String,
        String,
        i64,
        String,
        String,
    );
    let rows: Vec<ConflictRow> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
                r.get(6)?,
                r.get(7)?,
                r.get(8)?,
                r.get(9)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;

    if args.json {
        let out: Vec<serde_json::Value> = rows
            .iter()
            .map(|(id, old, new, rel, conf, rat, status, ts, oname, nname)| {
                serde_json::json!({
                    "id": id, "old_id": old, "old_name": oname,
                    "new_id": new, "new_name": nname,
                    "relation": rel, "confidence": conf,
                    "rationale": rat, "status": status, "created_at": ts,
                })
            })
            .collect();
        println!("{}", serde_json::json!({"conflicts": out}));
        return Ok(());
    }
    if rows.is_empty() {
        println!(
            "no {} conflicts",
            if args.all { "recorded" } else { "open" }
        );
        return Ok(());
    }
    println!("## Suspected conflicts ({})\n", rows.len());
    for (id, old, new, rel, conf, rat, status, ts, oname, nname) in &rows {
        let conf_s = conf
            .map(|c| format!("{c:.2}"))
            .unwrap_or_else(|| "-".into());
        println!(
            "#{id} [{status}] {new} ({nname}) --{rel} {conf_s}--> {old} ({oname})  [{}]",
            fmt_ts(*ts)
        );
        if !rat.is_empty() {
            println!("    {rat}");
        }
        println!("    resolve: agent memory correct {old} --with {new}   |   dismiss: agent memory conflicts --dismiss {id}");
    }
    Ok(())
}

/// at-usl: stamp a memory as terrain-checked now.
fn verify(conn: &Connection, args: VerifyArgs) -> Result<()> {
    let before = memory_row_json(conn, &args.id)?
        .ok_or_else(|| anyhow::anyhow!("no memory found with id={}", args.id))?;
    let ts = now();
    conn.execute(
        "UPDATE memories SET verified_at=?1, updated_at=?1 WHERE id=?2",
        params![ts, args.id],
    )?;
    let after = memory_row_json(conn, &args.id)?;
    conn.execute(
        "INSERT INTO memory_events (ts, actor, action, memory_id, before_json, after_json, reason)
         VALUES (?1, 'agent', 'verify', ?2, ?3, ?4, ?5)",
        params![ts, args.id, before, after, args.note],
    )?;
    log::info!("{} verified {}", args.id, fmt_ts(ts));
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
    if let Some(lifecycle) = &args.lifecycle {
        let valid = ["active", "archived", "trashed"];
        if !valid.contains(&lifecycle.as_str()) {
            bail!("lifecycle must be one of: {}", valid.join(", "));
        }
    }
    let lifecycle = args.lifecycle.unwrap_or_else(|| "active".into());
    let sql = "SELECT id, type, name, description, tags, lifecycle, updated_at FROM memories
         WHERE lifecycle = ?1
           AND (?2 IS NULL OR type = ?2)
           AND (?3 IS NULL OR cwd LIKE ?3 ESCAPE '\\')
           AND (?4 IS NULL OR tags LIKE ?4 ESCAPE '\\')
         ORDER BY updated_at DESC LIMIT ?5";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(
        params![lifecycle, args.r#type, cwd_pat, tag_pat, args.limit as i64],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, i64>(6)?,
            ))
        },
    )?;
    for row in rows {
        let (id, type_, name, desc, tags, lifecycle, updated_at) = row?;
        let tag_str = if tags.is_empty() {
            String::new()
        } else {
            format!("  [{}]", tags)
        };
        println!(
            "[{id}] ({type_}) {name}{tag_str} <{lifecycle}> — {desc}  [{}]",
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

    // Scope the session-start context to the active profile + shared. Absent a
    // profile (no --scope, no $CLAUDE_PROFILE) this spans all scopes.
    let scope = ScopeFilter::for_context(args.scope.as_deref());

    // at-0q9: the identity tier short-circuits the four-section wall.
    match args.tier.as_str() {
        "identity" => return context_identity(conn, &args, &scope),
        "full" => {}
        other => bail!("invalid --tier: {other} (expected 'identity' or 'full')"),
    }

    // feedback and user: always all-active, no scoring needed
    let feedback = query_by_type(conn, "feedback", 20, &scope)?;
    let user = query_by_type(conn, "user", 5, &scope)?;

    // project and reference: topic-scored
    let project_limit = args.limit.max(5);
    let ref_limit = (args.limit / 2).max(3);

    let scored_project =
        score_context_memories(conn, &signal_terms, "project", project_limit, &scope)?;
    let scored_reference =
        score_context_memories(conn, &signal_terms, "reference", ref_limit, &scope)?;

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

    // at-baj: every injected memory carries its testimony label — this output
    // feeds claude-code session-start hooks AND mu's recall providers, so the
    // label here is the whole mu integration.
    if !feedback.is_empty() {
        println!("### Behavioral Rules (Feedback)\n");
        for m in &feedback {
            println!("**{}**: {}", m.name, m.description);
            println!("*{}*", memory_trust_label(m));
            println!("{}\n", m.content);
        }
    }

    if !user.is_empty() {
        println!("### User Profile\n");
        for m in &user {
            println!("*{}*", memory_trust_label(m));
            println!("{}\n", m.content);
        }
    }

    if !scored_project.is_empty() {
        println!("### Project Context\n");
        for (m, _) in &scored_project {
            println!("**{}**: {}", m.name, m.description);
            println!("*{}*", memory_trust_label(m));
            println!("{}\n", m.content);
        }
    }

    if !scored_reference.is_empty() {
        println!("### References\n");
        for (m, _) in &scored_reference {
            println!("**{}**: {}", m.name, m.content);
            println!("*{}*\n", memory_trust_label(m));
        }
    }

    Ok(())
}

/// at-0q9: the tag that marks a feedback memory as identity-tier.
/// Membership is a data edit (`agent memory update ID --tags ...`),
/// not a code change — curation stays operator-blessable.
const IDENTITY_TAG: &str = "identity";

/// True iff `m` carries the [`IDENTITY_TAG`].
fn has_identity_tag(m: &Memory) -> bool {
    m.tags.split(',').any(|t| t.trim() == IDENTITY_TAG)
}

/// at-0q9: select the identity kernel — `user` AND `feedback`
/// memories tagged [`IDENTITY_TAG`]. Both types are tag-gated: the
/// live store's user rows include multi-paragraph war stories that
/// alone would blow the 600–800 token budget; the kernel is exactly
/// what the operator blessed, nothing by default. Tier, not topic:
/// who the operator is and how to engage. Task detail never
/// qualifies; it stays recall-only.
fn identity_kernel(conn: &Connection, scope: &ScopeFilter) -> Result<(Vec<Memory>, Vec<Memory>)> {
    let user = query_by_type(conn, "user", 50, scope)?
        .into_iter()
        .filter(has_identity_tag)
        .collect();
    let feedback = query_by_type(conn, "feedback", 50, scope)?
        .into_iter()
        .filter(has_identity_tag)
        .collect();
    Ok((user, feedback))
}

/// at-0q9: render the identity tier — the small kernel injected at
/// session start in place of the four-section wall. User profile
/// FIRST (mu-42x8 lever a: the stable who-is-this slice leads), then
/// the identity-tagged behavioral rules. Everything else in the
/// store is reachable via `agent memory recall` / `search` — the
/// kernel says so explicitly, because a kernel that doesn't teach
/// discovery amputates the tail it demoted.
fn context_identity(conn: &Connection, args: &ContextArgs, scope: &ScopeFilter) -> Result<()> {
    let (user, feedback) = identity_kernel(conn, scope)?;

    if user.is_empty() && feedback.is_empty() {
        return Ok(());
    }

    // Same tuning log as the full tier; n_scored=0 stays the marker for
    // an identity-tier call in context-stats. at-kernel-editor-oio: the
    // injected ids now ride in `returned` (score 0.0) so `kernel show`
    // can answer "how often does this row actually get injected" —
    // before this, identity-tier calls logged no ids at all.
    let injected: Vec<(String, String, f64)> = user
        .iter()
        .chain(feedback.iter())
        .map(|m| (m.id.clone(), m.name.clone(), 0.0))
        .collect();
    let _ = log_context_call(
        conn,
        &args.cwd,
        &["tier:identity".to_string()],
        0,
        &injected,
    );

    println!("## Identity Kernel\n");

    if !user.is_empty() {
        println!("### User Profile\n");
        for m in &user {
            println!("*{}*", memory_trust_label(m));
            println!("{}\n", m.content);
        }
    }

    if !feedback.is_empty() {
        println!("### Standing Rules\n");
        // Injection economics: the rule (name + description) is the
        // kernel; the WHY (full content, incidents) stays one
        // `agent memory show <name>` away. Full content here would
        // blow the 600–800 token budget on the first long rule.
        for m in &feedback {
            println!("**{}**: {}", m.name, m.description);
            println!("*{}*\n", memory_trust_label(m));
        }
    }

    println!(
        "*Everything else is recall-only: `agent memory recall \"<topic>\"` \
         (semantic), `agent memory search \"<term>\"` (lexical), \
         `agent memory show <name>` (full rule + why). \
         Memories are testimony — check the labels.*"
    );

    if args.verbose {
        // Estimate what was actually printed: user full content,
        // feedback one-liners.
        let chars: usize = user.iter().map(|m| m.content.len()).sum::<usize>()
            + feedback
                .iter()
                .map(|m| m.name.len() + m.description.len())
                .sum::<usize>();
        eprintln!(
            "[context] tier=identity: {} user + {} feedback ≈ {} tokens (chars/4)",
            user.len(),
            feedback.len(),
            chars / 4
        );
    }

    Ok(())
}

/// at-kernel-editor-oio: dispatch for the `memory kernel` group.
fn kernel(conn: &Connection, cmd: KernelCmd) -> Result<()> {
    match cmd.action {
        KernelAction::Show(args) => kernel_show(conn, args),
        KernelAction::Demote(args) => kernel_set_membership(conn, &args.id, false, &args.reason),
        KernelAction::Promote(args) => kernel_set_membership(conn, &args.id, true, &args.reason),
        // Thin alias: a kernel row is corrected exactly like any other
        // memory; the alias exists so the operator editing the kernel
        // doesn't have to context-switch command groups.
        KernelAction::Supersede(args) => correct(conn, args),
    }
}

/// Per-memory identity-tier injection stats from `memory_context_log`:
/// (times injected, last-injected unix ts). Only rows logged AFTER
/// at-kernel-editor-oio carry ids for the identity tier, so counts
/// start at the deploy date — the column header says "logged" to keep
/// that honest.
fn kernel_injection_stats(
    conn: &Connection,
) -> Result<std::collections::HashMap<String, (u64, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT created_at, returned FROM memory_context_log
         WHERE signals LIKE '%tier:identity%' AND returned != '' AND returned != '[]'",
    )?;
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?;
    let mut stats: std::collections::HashMap<String, (u64, i64)> = std::collections::HashMap::new();
    for row in rows {
        let (ts, returned) = row?;
        let Ok(entries) = serde_json::from_str::<Vec<serde_json::Value>>(&returned) else {
            continue;
        };
        for e in entries {
            if let Some(id) = e.get("id").and_then(|v| v.as_str()) {
                let s = stats.entry(id.to_string()).or_insert((0, ts));
                s.0 += 1;
                s.1 = s.1.max(ts);
            }
        }
    }
    Ok(stats)
}

/// `kernel show` — the kernel as `context --tier identity` selects it,
/// row for row (same [`identity_kernel`] call), with the per-row
/// numbers the operator needs to curate: token estimate (chars/4 of
/// what that row actually injects) and logged injection count.
fn kernel_show(conn: &Connection, args: KernelShowArgs) -> Result<()> {
    let scope = ScopeFilter::for_context(args.scope.as_deref());
    let (user, feedback) = identity_kernel(conn, &scope)?;

    if user.is_empty() && feedback.is_empty() {
        println!(
            "identity kernel is empty — nothing carries the '{IDENTITY_TAG}' tag in scope. \
             Promote rows with `agent memory kernel promote <id>`."
        );
        return Ok(());
    }

    let stats = kernel_injection_stats(conn).unwrap_or_default();
    let mut total_chars = 0usize;
    let mut print_row = |m: &Memory, injected_chars: usize| {
        total_chars += injected_chars;
        let (count, last) = stats.get(&m.id).copied().unwrap_or((0, 0));
        let last = if last > 0 {
            format!(", last {}", fmt_ts(last))
        } else {
            String::new()
        };
        println!(
            "  {}  ~{} tok  injected {}x (logged{})  [{}]",
            m.id,
            injected_chars / 4,
            count,
            last,
            memory_trust_label(m),
        );
        println!("      {}", m.name);
    };

    println!(
        "## Identity Kernel — {} user + {} rules\n",
        user.len(),
        feedback.len()
    );
    if !user.is_empty() {
        println!("USER PROFILE (injects full content)");
        for m in &user {
            // Mirrors context_identity: user rows inject content.
            print_row(m, m.content.len());
        }
    }
    if !feedback.is_empty() {
        println!("STANDING RULES (inject name + description)");
        for m in &feedback {
            // Mirrors context_identity: rules inject the one-liner.
            print_row(m, m.name.len() + m.description.len());
        }
    }
    println!(
        "\n≈ {} tokens total (chars/4). Curate: `kernel demote <id> --reason ...`, \
         `kernel promote <id>`, `kernel supersede <old> --with <new>`.",
        total_chars / 4
    );
    Ok(())
}

/// Add/remove the [`IDENTITY_TAG`] on a memory — the kernel-membership
/// mutation behind `kernel promote` / `kernel demote`. Logged to
/// `memory_events` as action=promote/demote with before/after
/// snapshots; a no-op (already in the requested state) logs nothing.
fn kernel_set_membership(conn: &Connection, id: &str, member: bool, reason: &str) -> Result<()> {
    let before = memory_row_json(conn, id)?
        .ok_or_else(|| anyhow::anyhow!("no memory found with id={id}"))?;
    let tags: String =
        conn.query_row("SELECT tags FROM memories WHERE id=?1", params![id], |r| {
            r.get(0)
        })?;
    let mut parts: Vec<String> = tags
        .split(',')
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    let has = parts.iter().any(|t| t == IDENTITY_TAG);
    if member == has {
        println!(
            "no change: {id} is already {} the kernel",
            if member { "in" } else { "out of" }
        );
        return Ok(());
    }
    if member {
        parts.push(IDENTITY_TAG.to_string());
    } else {
        parts.retain(|t| t != IDENTITY_TAG);
    }
    let new_tags = parts.join(",");
    let ts = now();
    conn.execute(
        "UPDATE memories SET tags=?1, updated_at=?2 WHERE id=?3",
        params![new_tags, ts, id],
    )?;
    let after = memory_row_json(conn, id)?;
    let action = if member { "promote" } else { "demote" };
    conn.execute(
        "INSERT INTO memory_events (ts, actor, action, memory_id, before_json, after_json, reason)
         VALUES (?1, 'agent', ?2, ?3, ?4, ?5, ?6)",
        params![ts, action, id, before, after, reason],
    )?;
    println!(
        "{action}d {id} {} the identity kernel (takes effect next session start)",
        if member { "into" } else { "out of" }
    );
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
        let cwd_short = cwd.split('/').next_back().unwrap_or(&cwd).to_string();
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

fn query_by_type(
    conn: &Connection,
    type_: &str,
    limit: usize,
    scope: &ScopeFilter,
) -> Result<Vec<Memory>> {
    let (scope_sql, scope_vals) = scope.sql_and("scope");
    let sql = format!(
        "SELECT id, type, name, description, content, source, tags, cwd, is_active, lifecycle, created_at, updated_at, verified_at, author
         FROM memories WHERE type = ? AND is_active = 1 AND lifecycle = 'active'{scope_sql} ORDER BY updated_at DESC LIMIT ?"
    );
    let mut p: Vec<Value> = vec![Value::Text(type_.to_string())];
    p.extend(scope_vals);
    p.push(Value::Integer(limit as i64));
    let mut stmt = conn.prepare(&sql)?;
    let mapped = stmt.query_map(params_from_iter(p.iter()), row_to_memory)?;
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
        lifecycle: r.get(9)?,
        created_at: r.get(10)?,
        updated_at: r.get(11)?,
        verified_at: r.get(12)?,
        author: r.get(13)?,
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

/// Turn a free-text recall query into a safe FTS5 MATCH expression.
///
/// FTS5's query grammar reserves `-`, `:`, `*`, `^`, `(`, `)` and the bareword
/// operators `AND`/`OR`/`NOT`. Passing user text straight into MATCH meant a
/// hyphenated term like `mu-slat orchestration` was parsed as syntax and
/// errored with `no such column: slat`, silently breaking all hyphenated recall.
///
/// We split on whitespace and wrap each token as a double-quoted FTS5 *phrase*
/// (doubling any embedded `"`). Inside a phrase FTS5 still tokenizes, so
/// `"mu-slat"` matches the adjacent tokens `mu slat` — i.e. the literal term —
/// while every metacharacter is treated as inert. Tokens with no alphanumeric
/// content are dropped (a bare `-` would otherwise produce an empty phrase).
/// Multiple phrases keep FTS5's default implicit-AND, so recall means "all of
/// these words appear". This intentionally trades away power-user prefix (`*`)
/// and boolean syntax for queries that just work; an empty result means "search
/// for nothing", and the caller short-circuits.
fn fts5_match_query(raw: &str) -> String {
    raw.split_whitespace()
        .filter(|tok| tok.chars().any(|c| c.is_alphanumeric()))
        .map(|tok| format!("\"{}\"", tok.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" ")
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

// ── Recall ranking v1 (at-supersession-activation-gf2.1) ─────────────────────
//
// score = cosine · trust · fresh, where freshness decays from t_eff =
// max(created, updated, verified) — so VERIFYING an old-but-true memory
// resets its decay clock without touching the text, and old facts nobody
// re-confirms fade gently. Trust is a bounded boost for recent
// verification, never a gate: with most of the store never verified, a
// gate would zero it. Design: Plan A recommendation.md §2.1
// (mu/.delegations/overnight-2026-06-12/RESULTS-fable5/).

/// Freshness floor: fresh ∈ [FLOOR, 1.0]. The floor makes semantic
/// dominance PROVABLE: the worst-case non-semantic spread is
/// (1 + RANK_TRUST_BOOST) / RANK_FRESH_FLOOR ≈ 1.47, so a candidate
/// with a ≥1.48× cosine advantage can never be flipped by trust +
/// freshness — recency nudges ties and modest gaps (the stale-vs-
/// correction regime), never decisive matches. (An unfloored decay
/// reached 0.43 at 2 years → 2.9× spread → identity facts washed out;
/// the cosine-dominance test caught it.)
const RANK_FRESH_FLOOR: f64 = 0.85;
/// In-band decay rate (log decay, same family as `context`'s
/// score_context_memories — deliberately NOT the Generative-Agents
/// exponential that halves in days).
const RANK_FRESH_LAMBDA: f64 = 1.0;
/// Max verification boost (+25% for a just-verified memory).
const RANK_TRUST_BOOST: f64 = 0.25;
/// Verification boost decay scale in days (~6 months).
const RANK_TRUST_DECAY_DAYS: f64 = 180.0;
/// Tie-break window: scores within this relative distance count as
/// ties. Calibrated on the live store: a genuinely-stale pair arrives
/// here gap-compressed by freshness (the FreeBSD incident shape lands
/// ≈1-2%), while recent complementary memories keep their full cosine
/// gap — 15% admitted a 14.5%-gap non-tie pair on the first live smoke
/// test; 10% does not.
const RANK_TIE_REL_WINDOW: f32 = 0.10;
/// Tie-break topical gate: candidates this similar to EACH OTHER are
/// "the same topic" — only then may recency reorder them. Keeps recency
/// from ever overriding a decisively better match on a different topic.
const RANK_TIE_PAIR_COSINE: f32 = 0.85;

/// Effective timestamp for freshness: verification counts as renewal.
fn rank_t_eff(created_at: i64, updated_at: i64, verified_at: Option<i64>) -> i64 {
    created_at.max(updated_at).max(verified_at.unwrap_or(0))
}

/// Freshness factor in [RANK_FRESH_FLOOR, 1.0]: 1.0 at age 0, log
/// decay toward (never below) the floor.
fn rank_fresh(now_ts: i64, t_eff: i64) -> f32 {
    let days = ((now_ts - t_eff) as f64 / 86400.0).max(0.0);
    let decay = 1.0 / (1.0 + RANK_FRESH_LAMBDA * days.ln_1p());
    (RANK_FRESH_FLOOR + (1.0 - RANK_FRESH_FLOOR) * decay) as f32
}

/// Trust factor in [1.0, 1.25]: a boost for recent verification,
/// baseline 1.0 for never-verified (a boost, never a gate).
fn rank_trust(now_ts: i64, verified_at: Option<i64>) -> f32 {
    match verified_at {
        Some(v) => {
            let days = ((now_ts - v) as f64 / 86400.0).max(0.0);
            (1.0 + RANK_TRUST_BOOST * (-days / RANK_TRUST_DECAY_DAYS).exp()) as f32
        }
        None => 1.0,
    }
}

/// One scored recall candidate. Lifted out of `recall()` so the ranking
/// passes are unit-testable; `vector` is retained for the tie-break's
/// pairwise-similarity gate. Crate-visible so the shared [`semantic_recall`]
/// engine can hand ranked hits to both `memory recall` and `kx recall`.
pub(crate) struct Scored {
    pub(crate) id: String,
    pub(crate) type_: String,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) content: String,
    pub(crate) tags: String,
    pub(crate) updated_at: i64,
    pub(crate) created_at: i64,
    pub(crate) verified_at: Option<i64>,
    pub(crate) lifecycle: String,
    pub(crate) author: Option<String>,
    /// The document date (`valid_from`) — used by the `kx` renderers; the
    /// `memory` recall renderer ignores it.
    pub(crate) valid_from: Option<i64>,
    /// Raw semantic similarity to the query.
    pub(crate) cosine: f32,
    /// Ranking score (== cosine under --rank legacy / kx).
    pub(crate) score: f32,
    pub(crate) t_eff: i64,
    pub(crate) vector: Vec<f32>,
    /// gf2.6: set when this (active) result's match strength was
    /// credited from a superseded chain member: "member-id (date)".
    pub(crate) via: Option<String>,
}

/// ε-window recency tie-break for UNLINKED near-duplicates (the
/// supersession edge handles linked ones): where two adjacent results
/// are score-ties (within RANK_TIE_REL_WINDOW) AND same-topic (pairwise
/// cosine > RANK_TIE_PAIR_COSINE), the newer t_eff wins. Bubble passes:
/// k is capped small, and the loop stops at the first stable pass.
fn rank_tie_break(scored: &mut [Scored]) {
    let n = scored.len();
    for _ in 0..n {
        let mut swapped = false;
        for i in 0..n.saturating_sub(1) {
            let (a, b) = (&scored[i], &scored[i + 1]);
            let tie = a.score > 0.0 && (a.score - b.score) / a.score <= RANK_TIE_REL_WINDOW;
            if tie
                && embed::cosine(&a.vector, &b.vector) > RANK_TIE_PAIR_COSINE
                && b.t_eff > a.t_eff
            {
                scored.swap(i, i + 1);
                swapped = true;
            }
        }
        if !swapped {
            break;
        }
    }
}

/// gf2.6: head-resolution match crediting — the structural fix for
/// "the stale text is the stronger semantic match". A superseded
/// candidate's match strength is CREDITED to its chain head instead of
/// competing with it: no down-weight can do this (any penalty big
/// enough to flip the worst case misfires elsewhere). Zep preserves and
/// annotates instead — and dumps the temporal reasoning on the consuming
/// LLM per query; here the ranker settles it. Returns active heads only;
/// `via` records the member a credit came through (also a staleness
/// signal: heads repeatedly reached through members should be rewritten/
/// re-embedded). Cycle/depth errors in a chain drop that member with a
/// warning — recall must not fail on corrupted supersession data.
fn credit_heads(conn: &Connection, scored: Vec<Scored>, model: &str) -> Result<Vec<Scored>> {
    let mut heads: Vec<Scored> = Vec::new();
    let mut by_id: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    let mut members: Vec<Scored> = Vec::new();
    for s in scored {
        if s.lifecycle == "superseded" {
            members.push(s);
        } else {
            by_id.insert(s.id.clone(), heads.len());
            heads.push(s);
        }
    }
    for m in members {
        let head_id = match resolve_current(conn, &m.id) {
            Ok(hops) => match hops.last() {
                Some(h) => h.clone(),
                None => continue, // superseded but no FK — orphaned; drop
            },
            Err(e) => {
                log::warn!("recall: skipping {} (chain unresolvable: {e:#})", m.id);
                continue;
            }
        };
        let annotation = format!("{} ({})", m.id, fmt_ts(m.updated_at));
        if let Some(&i) = by_id.get(&head_id) {
            if m.cosine > heads[i].cosine {
                heads[i].cosine = m.cosine;
                heads[i].via = Some(annotation);
            }
        } else {
            // Head wasn't nominated by the query at all — load it. Only
            // ACTIVE heads enter results (a chain ending on a retracted
            // row surfaces nothing).
            match load_scored_head(conn, &head_id, model)? {
                Some(mut h) if h.lifecycle == "active" => {
                    if m.cosine > h.cosine {
                        h.cosine = m.cosine;
                        h.via = Some(annotation);
                    }
                    by_id.insert(head_id, heads.len());
                    heads.push(h);
                }
                _ => log::debug!("recall: head {head_id} of {} not active; dropped", m.id),
            }
        }
    }
    Ok(heads)
}

/// Load one memory as a Scored row (cosine 0 until credited; vector from
/// the store when present so the tie-break can still compare it).
fn load_scored_head(conn: &Connection, id: &str, model: &str) -> Result<Option<Scored>> {
    let row = conn.query_row(
        "SELECT m.id, m.type, m.name, m.description, m.content, m.tags, m.updated_at,
                m.created_at, m.verified_at, m.lifecycle, m.author, e.vector, m.valid_from
         FROM memories m
         LEFT JOIN memory_embeddings e ON e.memory_id = m.id AND e.model = ?2
         WHERE m.id = ?1 AND m.is_active = 1",
        params![id, model],
        |r| {
            Ok(Scored {
                id: r.get(0)?,
                type_: r.get(1)?,
                name: r.get(2)?,
                description: r.get(3)?,
                content: r.get(4)?,
                tags: r.get(5)?,
                updated_at: r.get(6)?,
                created_at: r.get(7)?,
                verified_at: r.get(8)?,
                lifecycle: r.get(9)?,
                author: r.get(10)?,
                valid_from: r.get(12)?,
                cosine: 0.0,
                score: 0.0,
                t_eff: 0,
                vector: r
                    .get::<_, Option<Vec<u8>>>(11)?
                    .map(|b| embed::blob_to_f32(&b))
                    .unwrap_or_default(),
                via: None,
            })
        },
    );
    match row {
        Ok(mut h) => {
            h.t_eff = rank_t_eff(h.created_at, h.updated_at, h.verified_at);
            Ok(Some(h))
        }
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// gf2.10: pairwise-similarity gate for the possible-conflict flag.
/// Distinct knob from RANK_TIE_PAIR_COSINE even though the v1 values
/// coincide — the tie-break asks "same topic, may recency reorder?",
/// this asks "same topic, might these DISAGREE?"; they tune
/// independently (AT-9's replay harness is the tuning surface).
const CONFLICT_PAIR_COSINE: f32 = 0.85;

/// gf2.10: read-time possible-conflict flag — the safety net for
/// write-time adjudication misses and the unlinked backlog. Recall
/// only ever returns ACTIVE rows (superseded/retracted are filtered),
/// so any two returned results this similar are by construction
/// unlinked: surface that the agent may be looking at a contradiction
/// pair. Deliberately weak ("possible"): a statement and its negation
/// embed nearly identically (negation-blindness), but so do
/// complementary near-duplicates — a false flag costs only attention.
/// No LLM, no NLI in v1. Returns, per result, the ids of its
/// high-similarity partners (empty = no flag).
fn conflict_partners(scored: &[Scored]) -> Vec<Vec<String>> {
    let mut partners = vec![Vec::new(); scored.len()];
    for i in 0..scored.len() {
        for j in (i + 1)..scored.len() {
            if embed::cosine(&scored[i].vector, &scored[j].vector) > CONFLICT_PAIR_COSINE {
                partners[i].push(scored[j].id.clone());
                partners[j].push(scored[i].id.clone());
            }
        }
    }
    partners
}

/// Options controlling the shared semantic-recall engine [`semantic_recall`].
pub(crate) struct RecallOpts<'a> {
    /// Free-text query — embedded and cosine-compared to every candidate.
    pub(crate) query: &'a str,
    /// Type predicate: `ExcludeKx` for `memory recall`, `Only("kx")` for `kx`.
    pub(crate) type_filter: TypeFilter,
    /// Scope predicate (profile ownership).
    pub(crate) scope: ScopeFilter,
    /// Extra ` AND ...` predicate on the `m` alias (kx tag/date); "" for memory.
    pub(crate) extra_sql: String,
    /// Bound values for `extra_sql`'s `?` placeholders, in order.
    pub(crate) extra_vals: Vec<Value>,
    /// Cosine/score floor; hits below it are dropped (kx `min_score`). `None`
    /// keeps all (memory recall returns the top `limit` regardless of score).
    pub(crate) min_score: Option<f32>,
    /// Max hits returned after ranking.
    pub(crate) limit: usize,
    /// v1 trust/freshness ranking (memory) vs raw-cosine (kx / `--rank legacy`).
    pub(crate) rank_v1: bool,
}

/// Shared semantic-recall engine: embed the query (primary then fallback
/// embedder), score every candidate memory of the requested type/scope/extra
/// predicate by cosine, apply v1 trust/freshness ranking (memory) or a
/// raw-cosine floor (kx), and return the ranked hits plus the model that
/// produced the query vector. Returns `None` when NO embedder is available so
/// the caller can degrade (memory → FTS lexical; kx → a structured note).
/// `memory recall` and `kx recall` both route through this — the retrieval
/// logic lives in exactly one place (punch-list #5 / clarification #9).
pub(crate) fn semantic_recall(
    conn: &Connection,
    opts: &RecallOpts,
) -> Result<Option<(String, Vec<Scored>)>> {
    // `model` is whichever embedder produced the query vector, so the
    // `e.model = ?` filter below always compares within a single vector space.
    let (query_vec, model) = match embed::embed_query_with_fallback(opts.query) {
        Some(pair) => pair,
        None => return Ok(None),
    };

    let (type_sql, type_vals) = opts.type_filter.sql_and("m.type");
    let (scope_sql, scope_vals) = opts.scope.sql_and("m.scope");
    // gf2.6: v1 ranking pulls superseded rows too — their match strength is
    // credited to their chain heads. Legacy/kx keep the exclusion.
    let lifecycle_sql = if opts.rank_v1 {
        ""
    } else {
        " AND m.lifecycle != 'superseded'"
    };
    let extra_sql = opts.extra_sql.as_str();
    let sql = format!(
        "SELECT m.id, m.type, m.name, m.description, m.content, m.tags, m.updated_at, e.vector,
                m.created_at, m.verified_at, m.lifecycle, m.author, m.valid_from
               FROM memory_embeddings e
               JOIN memories m ON m.id = e.memory_id
               WHERE m.is_active = 1 AND e.model = ?{lifecycle_sql}{type_sql}{scope_sql}{extra_sql}"
    );
    let mut p: Vec<Value> = vec![Value::Text(model.clone())];
    p.extend(type_vals);
    p.extend(scope_vals);
    p.extend(opts.extra_vals.iter().cloned());

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params_from_iter(p.iter()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
            r.get::<_, String>(4)?,
            r.get::<_, String>(5)?,
            r.get::<_, i64>(6)?,
            r.get::<_, Vec<u8>>(7)?,
            r.get::<_, i64>(8)?,
            r.get::<_, Option<i64>>(9)?,
            r.get::<_, String>(10)?,
            r.get::<_, Option<String>>(11)?,
            r.get::<_, Option<i64>>(12)?,
        ))
    })?;

    let now_ts = now();
    let mut scored: Vec<Scored> = Vec::new();
    for row in rows {
        let (
            id,
            type_,
            name,
            description,
            content,
            tags,
            updated_at,
            blob,
            created_at,
            verified_at,
            lifecycle,
            author,
            valid_from,
        ) = row?;
        let v = embed::blob_to_f32(&blob);
        let cosine = embed::cosine(&query_vec, &v);
        let t_eff = rank_t_eff(created_at, updated_at, verified_at);
        // v1 final scores are computed AFTER head-crediting (below); until then
        // score carries the raw cosine (also the final score for kx/legacy).
        scored.push(Scored {
            id,
            type_,
            name,
            description,
            content,
            tags,
            updated_at,
            created_at,
            verified_at,
            lifecycle,
            author,
            valid_from,
            cosine,
            score: cosine,
            t_eff,
            vector: v,
            via: None,
        });
    }

    if opts.rank_v1 {
        scored = credit_heads(conn, scored, &model)?;
        for s in &mut scored {
            s.score = s.cosine * rank_trust(now_ts, s.verified_at) * rank_fresh(now_ts, s.t_eff);
        }
    }
    // kx honors a cosine floor (min_score); memory passes None and keeps all.
    if let Some(floor) = opts.min_score {
        scored.retain(|s| s.score >= floor);
    }
    scored.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    scored.truncate(opts.limit);
    if opts.rank_v1 {
        rank_tie_break(&mut scored);
    }
    Ok(Some((model, scored)))
}

fn recall(conn: &Connection, args: RecallArgs) -> Result<()> {
    let rank_v1 = args.rank == "v1";
    let opts = RecallOpts {
        query: &args.query,
        // memory recall never surfaces the kx corpus (deliverable #4).
        type_filter: TypeFilter::ExcludeKx(args.r#type.clone()),
        scope: ScopeFilter::for_explicit(args.scope.as_deref()),
        extra_sql: String::new(),
        extra_vals: Vec::new(),
        min_score: None,
        limit: args.k,
        rank_v1,
    };

    // If EVERY embedder is down (e.g. ollama busy AND OpenRouter unreachable),
    // degrade to lexical FTS search rather than hanging or erroring. (at-7mp)
    let (model, scored) = match semantic_recall(conn, &opts)? {
        Some(pair) => pair,
        None => {
            eprintln!(
                "recall: no embedder available (primary + fallback both failed); \
                 falling back to FTS lexical search"
            );
            if args.json {
                // Human-readable FTS output would corrupt stdout for
                // programmatic consumers; surface a valid structured note
                // instead and let them rerun without --json. (at-7mp)
                println!(
                    "{}",
                    serde_json::json!({
                        "model": "lexical-fallback",
                        "query": args.query,
                        "results": [],
                        "error": "all embedders unavailable; rerun without --json for FTS lexical results",
                    })
                );
                return Ok(());
            }
            return search(
                conn,
                SearchArgs {
                    query: args.query.clone(),
                    r#type: args.r#type.clone(),
                    limit: args.k,
                    scope: args.scope.clone(),
                    author: None,
                },
            );
        }
    };

    if scored.is_empty() {
        if args.json {
            println!(
                "{}",
                serde_json::json!({
                    "model": model,
                    "query": args.query,
                    "results": [],
                    "error": format!("no embeddings found for model '{model}'; run `agent memory reindex` first"),
                })
            );
        } else {
            eprintln!("no embeddings found for model '{model}'. Run `agent memory reindex` first.");
        }
        return Ok(());
    }

    // gf2.10: flag possible contradiction pairs among the returned set
    // (v1 only — legacy output stays bit-identical).
    let conflicts = if rank_v1 {
        conflict_partners(&scored)
    } else {
        vec![Vec::new(); scored.len()]
    };

    // Telemetry: log this recall call before returning results.
    let log_rows: Vec<(&str, &str, f32, f32)> = scored
        .iter()
        .map(|s| (s.id.as_str(), s.name.as_str(), s.score, s.cosine))
        .collect();
    log_recall(conn, &args, &log_rows);

    if args.json {
        let results: Vec<serde_json::Value> = scored
            .iter()
            .zip(&conflicts)
            .map(|(s, conflict)| {
                let mut obj = serde_json::json!({
                    "id": s.id,
                    "type": s.type_,
                    "name": s.name,
                    "description": s.description,
                    "tags": s.tags,
                    "updated_at": s.updated_at,
                    "score": s.score,
                    // at-baj: testimony fields for structured consumers,
                    // plus the rendered label injection paths show verbatim.
                    "created_at": s.created_at,
                    "verified_at": s.verified_at,
                    "lifecycle": s.lifecycle,
                    "author": s.author,
                    "trust": trust_label(
                        s.created_at,
                        s.verified_at,
                        &s.lifecycle,
                        None,
                        s.author.as_deref().unwrap_or(""),
                    ),
                });
                if rank_v1 {
                    // gf2.1: raw similarity alongside the rank score so
                    // consumers can see WHY something ranked. Omitted in
                    // legacy mode to keep that output bit-identical.
                    obj["cosine"] = serde_json::json!(s.cosine);
                }
                if let Some(via) = &s.via {
                    // gf2.6: this result's match strength came through a
                    // superseded chain member.
                    obj["matched_via_superseded"] = serde_json::json!(via);
                }
                if !conflict.is_empty() {
                    // gf2.10: ids of co-returned results similar enough
                    // to be the other side of a contradiction.
                    obj["possible_conflict_with"] = serde_json::json!(conflict);
                }
                if args.full {
                    obj["content"] = serde_json::Value::String(s.content.clone());
                }
                obj
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "model": model,
                "query": args.query,
                "results": results,
            })
        );
        return Ok(());
    }

    println!("## Semantic recall (model: {})\n", model);
    for (s, conflict) in scored.iter().zip(&conflicts) {
        // v1 shows score|cosine so a reader can see how much of the rank
        // came from similarity vs trust/freshness; legacy stays untouched.
        let badge = if rank_v1 {
            format!("[{:.3}|c{:.3}]", s.score, s.cosine)
        } else {
            format!("[{:.3}]", s.score)
        };
        println!(
            "{badge} [{id}] ({type_}) {name} — {desc}  [{ts}]",
            id = s.id,
            type_ = s.type_,
            name = s.name,
            desc = s.description,
            ts = fmt_ts(s.updated_at)
        );
        // at-baj: one-liners are an injection path (mu trigger recall) — they
        // carry the testimony label like every other read path.
        println!(
            "  {}",
            trust_label(
                s.created_at,
                s.verified_at,
                &s.lifecycle,
                None,
                s.author.as_deref().unwrap_or(""),
            )
        );
        if let Some(via) = &s.via {
            println!("  matched via superseded {via}");
        }
        if !conflict.is_empty() {
            println!("  possible-conflict-with: {}", conflict.join(", "));
        }
        if args.full {
            println!("{}\n", indent(&s.content, "  "));
        }
    }

    if args.compare {
        println!("\n## FTS5 lexical comparison\n");
        search(
            conn,
            SearchArgs {
                query: args.query.clone(),
                r#type: args.r#type.clone(),
                limit: args.k,
                scope: args.scope.clone(),
                author: None,
            },
        )?;
    }

    Ok(())
}

fn log_recall(conn: &Connection, args: &RecallArgs, scored: &[(&str, &str, f32, f32)]) {
    let cwd = std::env::current_dir()
        .ok()
        .and_then(|p| p.to_str().map(String::from))
        .unwrap_or_default();
    let top_score = scored.first().map(|(_, _, s, _)| *s as f64);
    // `cosine` alongside the ranking score so rank-function changes stay
    // tunable against telemetry (gf2.1; trust/fresh are reconstructable
    // from the memory's timestamps + ts).
    let results_summary: Vec<serde_json::Value> = scored
        .iter()
        .map(|(id, name, score, cosine)| {
            serde_json::json!({"id": id, "name": name, "score": score, "cosine": cosine})
        })
        .collect();
    let results_json = serde_json::to_string(&results_summary).unwrap_or_else(|_| "[]".into());

    // If --compare was set, count FTS hits with the same query+type filter.
    let fts_hits: Option<i64> = if args.compare {
        let sql = "SELECT COUNT(*) FROM memories_fts fts
                   JOIN memories m ON m.rowid = fts.rowid
                   WHERE memories_fts MATCH ?1 AND m.is_active = 1
                     AND (?2 IS NULL OR m.type = ?2)";
        // .ok(): FTS query syntax errors etc — leave null
        conn.query_row(sql, params![args.query, args.r#type], |r| {
            r.get::<_, i64>(0)
        })
        .ok()
    } else {
        None
    };

    let _ = conn.execute(
        "INSERT INTO memory_recall_log
         (ts, cwd, query, k, type_filter, top_score, results_json, compare_used, fts_hits)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            now(),
            cwd,
            args.query,
            args.k as i64,
            args.r#type,
            top_score,
            results_json,
            if args.compare { 1_i64 } else { 0_i64 },
            fts_hits,
        ],
    );
}

fn recall_stats(conn: &Connection, args: RecallStatsArgs) -> Result<()> {
    let since = now() - args.days * 86400;

    if args.gaps {
        println!(
            "## Weak-recall queries (top_score < {:.2}, last {} days)\n",
            args.gaps_threshold, args.days
        );
        let mut stmt = conn.prepare(
            "SELECT ts, query, k, COALESCE(top_score, 0.0), type_filter
             FROM memory_recall_log
             WHERE ts >= ?1 AND (top_score IS NULL OR top_score < ?2)
             ORDER BY top_score ASC NULLS FIRST, ts DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(
            params![since, args.gaps_threshold, args.limit as i64],
            |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)?,
                    r.get::<_, f64>(3)?,
                    r.get::<_, Option<String>>(4)?,
                ))
            },
        )?;
        let mut count = 0;
        for row in rows {
            let (ts, query, k, score, type_filter) = row?;
            let type_str = type_filter
                .map(|t| format!(" type={t}"))
                .unwrap_or_default();
            println!(
                "  [{score:.3}]  k={k}{type_str}  {}  ({})",
                fmt_ts(ts),
                query
            );
            count += 1;
        }
        if count == 0 {
            println!("  (none — recall is hitting strong matches across the board)");
        }
        println!();
    }

    if args.hotspots {
        println!(
            "## Hotspots (queries grouped by first significant token, last {} days)\n",
            args.days
        );
        // Pull all queries in window, group in-process by first significant token.
        let mut stmt = conn.prepare(
            "SELECT query, COALESCE(top_score, 0.0) FROM memory_recall_log WHERE ts >= ?1",
        )?;
        let rows = stmt.query_map(params![since], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?))
        })?;
        let mut buckets: HashMap<String, (f64, usize, Vec<String>)> = HashMap::new();
        for row in rows {
            let (q, s) = row?;
            let token = tokenize(&q)
                .into_iter()
                .next()
                .unwrap_or_else(|| "_".into());
            let entry = buckets.entry(token).or_insert((0.0, 0, Vec::new()));
            entry.0 += s;
            entry.1 += 1;
            if entry.2.len() < 3 {
                entry.2.push(q);
            }
        }
        let mut ranked: Vec<(String, f64, usize, Vec<String>)> = buckets
            .into_iter()
            .map(|(k, (sum, n, samples))| (k, sum / n as f64, n, samples))
            .collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (token, avg, n, samples) in ranked.into_iter().take(args.limit) {
            println!("  [{avg:.3}]  n={n:>3}  {token}");
            for q in samples {
                println!("           e.g. {q}");
            }
        }
        println!();
    }

    if args.recall_vs_search {
        println!(
            "## Recall-found, FTS-missed (top_score >= {:.2}, fts_hits = 0, last {} days)\n",
            args.rvs_threshold, args.days
        );
        let mut stmt = conn.prepare(
            "SELECT ts, query, COALESCE(top_score, 0.0), results_json
             FROM memory_recall_log
             WHERE ts >= ?1 AND compare_used = 1
               AND COALESCE(top_score, 0.0) >= ?2
               AND COALESCE(fts_hits, 0) = 0
             ORDER BY top_score DESC
             LIMIT ?3",
        )?;
        let rows = stmt.query_map(params![since, args.rvs_threshold, args.limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, f64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut count = 0;
        for row in rows {
            let (ts, query, score, results_json) = row?;
            println!("  [{score:.3}]  {}  ({})", fmt_ts(ts), query);
            if let Ok(arr) = serde_json::from_str::<Vec<serde_json::Value>>(&results_json) {
                if let Some(top) = arr.first() {
                    let name = top["name"].as_str().unwrap_or("?");
                    let id = top["id"].as_str().unwrap_or("?");
                    println!("           recall top: [{id}] {name}");
                }
            }
            count += 1;
        }
        if count == 0 {
            println!("  (none — either FTS is keeping up, or no recent --compare calls)");
        }
        println!();
    }

    // Default view: recent recall log entries, like context-stats
    if !args.gaps && !args.hotspots && !args.recall_vs_search {
        println!("## Recent recall calls (last {})\n", args.limit);
        let mut stmt = conn.prepare(
            "SELECT ts, query, k, top_score, type_filter, compare_used, fts_hits
             FROM memory_recall_log
             ORDER BY ts DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![args.limit as i64], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, Option<f64>>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?,
                r.get::<_, Option<i64>>(6)?,
            ))
        })?;
        let mut count = 0;
        for row in rows {
            let (ts, query, k, top_score, type_filter, compare_used, fts_hits) = row?;
            let score_str = top_score
                .map(|s| format!("[{s:.3}]"))
                .unwrap_or_else(|| "[ -- ]".into());
            let type_str = type_filter
                .map(|t| format!(" type={t}"))
                .unwrap_or_default();
            let cmp_str = if compare_used == 1 {
                format!(
                    " cmp fts={}",
                    fts_hits
                        .map(|n| n.to_string())
                        .unwrap_or_else(|| "?".into())
                )
            } else {
                String::new()
            };
            println!(
                "  {score_str}  k={k}{type_str}{cmp_str}  {}  {query}",
                fmt_ts(ts)
            );
            count += 1;
        }
        if count == 0 {
            println!("  (no recall calls logged yet — run `agent memory recall <query>` to seed)");
        }
    }

    Ok(())
}

fn reindex(conn: &Connection, args: ReindexArgs) -> Result<()> {
    // Reindex under every embedder in the chain so the fallback's
    // (memory_id, model) rows are (re)built alongside the primary's — that
    // parallel index is what lets recall match during a primary outage. (at-7mp)
    for embedder in embed::embedder_chain() {
        reindex_one(conn, &args, embedder.as_ref())?;
    }
    Ok(())
}

fn reindex_one(
    conn: &Connection,
    args: &ReindexArgs,
    embedder: &dyn embed::Embedder,
) -> Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ── gf2.1: recall ranking v1 ─────────────────────────────────────

    const DAY: i64 = 86_400;

    fn scored_stub(id: &str, score: f32, cosine: f32, t_eff: i64, vector: Vec<f32>) -> Scored {
        Scored {
            id: id.into(),
            type_: "project".into(),
            name: id.into(),
            description: "d".into(),
            content: String::new(),
            tags: String::new(),
            updated_at: t_eff,
            created_at: t_eff,
            verified_at: None,
            lifecycle: "active".into(),
            author: None,
            valid_from: None,
            cosine,
            score,
            t_eff,
            vector,
            via: None,
        }
    }

    #[test]
    fn t_eff_takes_verification_as_renewal() {
        assert_eq!(rank_t_eff(100, 200, None), 200);
        assert_eq!(rank_t_eff(100, 200, Some(900)), 900);
        // verification older than the last edit doesn't move t_eff back
        assert_eq!(rank_t_eff(100, 500, Some(300)), 500);
    }

    #[test]
    fn fresh_decays_monotonically_and_respects_the_floor() {
        let now = 1_000_000 * DAY;
        let f0 = rank_fresh(now, now);
        let f30 = rank_fresh(now, now - 30 * DAY);
        let f730 = rank_fresh(now, now - 730 * DAY);
        assert_eq!(f0, 1.0);
        assert!(f0 > f30 && f30 > f730, "freshness must decay");
        assert!(
            f730 >= RANK_FRESH_FLOOR as f32,
            "floor violated: {f730} < {RANK_FRESH_FLOOR}"
        );
    }

    /// The dominance bound is a consequence of the constants — pin it so
    /// a future retune that breaks the property fails loudly here rather
    /// than silently reintroducing recency-washes-out-identity-facts.
    #[test]
    fn non_semantic_spread_is_bounded_below_1_5() {
        let spread = (1.0 + RANK_TRUST_BOOST) / RANK_FRESH_FLOOR;
        assert!(
            spread < 1.5,
            "trust×fresh spread {spread} can flip a 0.9-vs-0.6 cosine gap"
        );
    }

    #[test]
    fn trust_boosts_verified_and_is_never_a_gate() {
        let now = 1_000_000 * DAY;
        assert_eq!(rank_trust(now, None), 1.0);
        let fresh_verify = rank_trust(now, Some(now));
        let old_verify = rank_trust(now, Some(now - 720 * DAY));
        assert!(fresh_verify > 1.2 && fresh_verify <= 1.25);
        assert!(old_verify > 1.0 && old_verify < fresh_verify);
    }

    /// The bead's cosine-dominance acceptance: a decisively better
    /// match (0.9 vs 0.6) must survive the worst-case trust/fresh
    /// spread — a never-verified, 2-year-stale winner still out-ranks a
    /// just-verified, just-edited loser.
    #[test]
    fn cosine_dominance_survives_trust_and_freshness() {
        let now = 1_000_000 * DAY;
        let strong_stale = 0.9 * rank_trust(now, None) * rank_fresh(now, now - 730 * DAY);
        let weak_fresh = 0.6 * rank_trust(now, Some(now)) * rank_fresh(now, now);
        assert!(
            strong_stale > weak_fresh,
            "0.9-stale ({strong_stale}) must beat 0.6-fresh ({weak_fresh})"
        );
    }

    #[test]
    fn tie_break_prefers_newer_among_same_topic_ties() {
        let now = 1_000_000 * DAY;
        // Same topic (identical vectors), scores within the tie window:
        // the stale belief edged out its newer correction — tie-break
        // flips them.
        let mut s = vec![
            scored_stub("stale", 0.80, 0.80, now - 100 * DAY, vec![1.0, 0.0]),
            scored_stub("correction", 0.745, 0.745, now, vec![1.0, 0.0]),
        ];
        rank_tie_break(&mut s);
        assert_eq!(s[0].id, "correction");
        assert_eq!(s[1].id, "stale");
    }

    #[test]
    fn tie_break_never_touches_distinct_topics_or_decisive_gaps() {
        let now = 1_000_000 * DAY;
        // Tie, but different topics (orthogonal vectors): no reorder.
        let mut topical = vec![
            scored_stub("a", 0.80, 0.80, now - 100 * DAY, vec![1.0, 0.0]),
            scored_stub("b", 0.72, 0.72, now, vec![0.0, 1.0]),
        ];
        rank_tie_break(&mut topical);
        assert_eq!(topical[0].id, "a");

        // Same topic, but a decisive gap (>15%): no reorder.
        let mut decisive = vec![
            scored_stub("a", 0.90, 0.90, now - 100 * DAY, vec![1.0, 0.0]),
            scored_stub("b", 0.60, 0.60, now, vec![1.0, 0.0]),
        ];
        rank_tie_break(&mut decisive);
        assert_eq!(decisive[0].id, "a");
    }

    // ── gf2.6: head-resolution match crediting ───────────────────────

    fn link(conn: &Connection, old: &str, new: &str) {
        apply_supersession(conn, old, new, "corrects", "", 1.0, "test").unwrap();
    }

    fn stub_superseded(conn: &Connection, id: &str, cosine: f32) -> Scored {
        // lifecycle must reflect the DB state set by link()
        let mut s = scored_stub(id, cosine, cosine, 1000, vec![1.0, 0.0]);
        s.lifecycle = conn
            .query_row(
                "SELECT lifecycle FROM memories WHERE id=?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        s
    }

    #[test]
    fn credit_heads_credits_member_match_to_in_set_head() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "stale", "old", "x");
        seed(&conn, "head", "new", "y");
        link(&conn, "stale", "head");
        let scored = vec![
            stub_superseded(&conn, "stale", 0.9),
            stub_superseded(&conn, "head", 0.6),
        ];
        let out = credit_heads(&conn, scored, "test-model").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "head");
        assert_eq!(out[0].cosine, 0.9, "member match must be credited");
        assert!(out[0].via.as_deref().unwrap().starts_with("stale"));
    }

    #[test]
    fn credit_heads_loads_unnominated_terminal_head_across_chain() {
        let conn = crate::db::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            seed(&conn, id, id, "x");
        }
        link(&conn, "a", "b");
        link(&conn, "b", "c");
        // only the deep-stale member was nominated by the query
        let scored = vec![stub_superseded(&conn, "a", 0.88)];
        let out = credit_heads(&conn, scored, "test-model").unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "c", "must resolve to the TERMINAL head");
        assert_eq!(out[0].cosine, 0.88);
        assert!(out[0].via.is_some());
    }

    #[test]
    fn credit_heads_drops_cyclic_and_inactive_chains_without_failing() {
        let conn = crate::db::open_in_memory().unwrap();
        // cycle
        seed(&conn, "x1", "x1", "a");
        seed(&conn, "y1", "y1", "b");
        conn.execute(
            "UPDATE memories SET lifecycle='superseded', superseded_by='y1' WHERE id='x1'",
            [],
        )
        .unwrap();
        conn.execute(
            "UPDATE memories SET lifecycle='superseded', superseded_by='x1' WHERE id='y1'",
            [],
        )
        .unwrap();
        // chain ending on a retracted head
        seed(&conn, "m1", "m1", "c");
        seed(&conn, "gone", "gone", "d");
        link(&conn, "m1", "gone");
        retract(
            &conn,
            RetractArgs {
                id: "gone".into(),
                reason: "test".into(),
            },
        )
        .unwrap();
        let scored = vec![
            stub_superseded(&conn, "x1", 0.9),
            stub_superseded(&conn, "m1", 0.8),
        ];
        let out = credit_heads(&conn, scored, "test-model").unwrap();
        assert!(
            out.is_empty(),
            "cyclic + retracted-head chains must drop, not fail"
        );
    }

    // ── gf2.10: read-time possible-conflict flag ─────────────────────

    #[test]
    fn conflict_partners_flags_same_topic_pairs_symmetrically() {
        let now = 1_000_000 * DAY;
        // a ≈ c (near-identical vectors), b orthogonal to both.
        let scored = vec![
            scored_stub("a", 0.9, 0.9, now, vec![1.0, 0.0, 0.05]),
            scored_stub("b", 0.8, 0.8, now, vec![0.0, 1.0, 0.0]),
            scored_stub("c", 0.7, 0.7, now, vec![1.0, 0.0, 0.0]),
        ];
        let partners = conflict_partners(&scored);
        assert_eq!(partners[0], vec!["c".to_string()]);
        assert!(
            partners[1].is_empty(),
            "orthogonal result must not be flagged"
        );
        assert_eq!(partners[2], vec!["a".to_string()]);
    }

    #[test]
    fn conflict_partners_empty_for_distinct_results() {
        let now = 1_000_000 * DAY;
        let scored = vec![
            scored_stub("a", 0.9, 0.9, now, vec![1.0, 0.0]),
            scored_stub("b", 0.8, 0.8, now, vec![0.0, 1.0]),
        ];
        assert!(conflict_partners(&scored).iter().all(Vec::is_empty));
    }

    // ── gf2.5: chain resolution ──────────────────────────────────────

    #[test]
    fn resolve_current_walks_chains_and_stops_at_heads() {
        let conn = crate::db::open_in_memory().unwrap();
        for id in ["a", "b", "c"] {
            seed(&conn, id, id, "x");
        }
        for (old, new) in [("a", "b"), ("b", "c")] {
            correct(
                &conn,
                CorrectArgs {
                    old: old.into(),
                    with_id: new.into(),
                    reason: String::new(),
                    kind: "corrects".into(),
                },
            )
            .unwrap();
        }
        // From any member, the head is c.
        assert_eq!(resolve_current(&conn, "a").unwrap(), vec!["b", "c"]);
        assert_eq!(resolve_current(&conn, "b").unwrap(), vec!["c"]);
        assert!(resolve_current(&conn, "c").unwrap().is_empty());
        // Unknown id: nothing to follow, not an error (callers pass
        // ids they already hold).
        assert!(resolve_current(&conn, "nope").unwrap().is_empty());
    }

    #[test]
    fn resolve_current_errors_loudly_on_cycles() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "x", "x", "1");
        seed(&conn, "y", "y", "2");
        // Forge a cycle directly (correct() can't make one in a single
        // step, but bad data can exist).
        conn.execute("UPDATE memories SET superseded_by='y' WHERE id='x'", [])
            .unwrap();
        conn.execute("UPDATE memories SET superseded_by='x' WHERE id='y'", [])
            .unwrap();
        let err = resolve_current(&conn, "x").unwrap_err().to_string();
        assert!(err.contains("CYCLE"), "{err}");
    }

    #[test]
    fn resolve_current_caps_pathological_depth() {
        let conn = crate::db::open_in_memory().unwrap();
        let n = RESOLVE_MAX_DEPTH + 3;
        for i in 0..=n {
            seed(&conn, &format!("m{i}"), &format!("m{i}"), "x");
        }
        for i in 0..n {
            conn.execute(
                "UPDATE memories SET superseded_by=?1 WHERE id=?2",
                params![format!("m{}", i + 1), format!("m{i}")],
            )
            .unwrap();
        }
        let err = resolve_current(&conn, "m0").unwrap_err().to_string();
        assert!(err.contains("exceeds"), "{err}");
    }

    // ── gf2.8: conflict queue lifecycle ──────────────────────────────

    fn queue_pair(conn: &Connection, old: &str, new: &str) -> i64 {
        conn.execute(
            "INSERT INTO conflict_suspected (old_id, new_id, relation, confidence, rationale, status, created_at)
             VALUES (?1, ?2, 'corrects', 0.6, 'test', 'open', 1000)",
            params![old, new],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn queue_status(conn: &Connection, id: i64) -> String {
        conn.query_row(
            "SELECT status FROM conflict_suspected WHERE id=?1",
            params![id],
            |r| r.get(0),
        )
        .unwrap()
    }

    #[test]
    fn correct_resolves_matching_queue_rows_both_orientations() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "o1", "old1", "x");
        seed(&conn, "n1", "new1", "y");
        seed(&conn, "o2", "old2", "x");
        seed(&conn, "n2", "new2", "y");
        let q1 = queue_pair(&conn, "o1", "n1");
        // reverse orientation: operator corrects the other way round
        let q2 = queue_pair(&conn, "n2", "o2");
        correct(
            &conn,
            CorrectArgs {
                old: "o1".into(),
                with_id: "n1".into(),
                kind: "corrects".into(),
                reason: String::new(),
            },
        )
        .unwrap();
        correct(
            &conn,
            CorrectArgs {
                old: "o2".into(),
                with_id: "n2".into(),
                kind: "corrects".into(),
                reason: String::new(),
            },
        )
        .unwrap();
        assert_eq!(queue_status(&conn, q1), "resolved");
        assert_eq!(
            queue_status(&conn, q2),
            "resolved",
            "reverse orientation must resolve too"
        );
    }

    #[test]
    fn retract_resolves_queue_rows_touching_the_id() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "r1", "doomed", "x");
        seed(&conn, "other", "bystander", "y");
        let q = queue_pair(&conn, "r1", "other");
        retract(
            &conn,
            RetractArgs {
                id: "r1".into(),
                reason: "gone".into(),
            },
        )
        .unwrap();
        assert_eq!(queue_status(&conn, q), "resolved");
    }

    #[test]
    fn dismiss_marks_row_and_requires_open() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "a1", "a", "x");
        seed(&conn, "b1", "b", "y");
        let q = queue_pair(&conn, "a1", "b1");
        conflicts(
            &conn,
            ConflictsArgs {
                dismiss: Some(q),
                reason: "false positive".into(),
                all: false,
                json: false,
            },
        )
        .unwrap();
        assert_eq!(queue_status(&conn, q), "dismissed");
        // dismissing again: no longer open -> error
        assert!(conflicts(
            &conn,
            ConflictsArgs {
                dismiss: Some(q),
                reason: String::new(),
                all: false,
                json: false,
            },
        )
        .is_err());
    }

    #[test]
    fn hyphenated_term_becomes_a_phrase() {
        // The original bug: `mu-slat` errored `no such column: slat`.
        assert_eq!(
            fts5_match_query("mu-slat orchestration"),
            r#""mu-slat" "orchestration""#
        );
    }

    #[test]
    fn metacharacters_are_neutralized() {
        // Colons, stars, parens and bareword operators must not reach the grammar.
        assert_eq!(fts5_match_query("foo:bar"), r#""foo:bar""#);
        assert_eq!(fts5_match_query("a* (b) OR c"), r#""a*" "(b)" "OR" "c""#);
    }

    #[test]
    fn embedded_quotes_are_doubled() {
        // A literal `"` inside a token must be escaped, not close the phrase.
        assert_eq!(fts5_match_query(r#"say"hi"#), r#""say""hi""#);
    }

    // ── at-usl: trust layer ──────────────────────────────────────────

    fn seed(conn: &Connection, id: &str, name: &str, content: &str) {
        conn.execute(
            "INSERT INTO memories (id, type, name, description, content, created_at, updated_at, author)
             VALUES (?1, 'project', ?2, 'd', ?3, 1000, 1000, 'test-witness')",
            params![id, name, content],
        )
        .unwrap();
    }

    // ── at-kernel-editor-oio: identity-kernel editor ─────────────────
    // (reuses the at-0q9 `seed_typed` helper further down this module)

    fn kernel_ids(conn: &Connection) -> Vec<String> {
        let (user, feedback) = identity_kernel(conn, &ScopeFilter::All).unwrap();
        user.iter()
            .chain(feedback.iter())
            .map(|m| m.id.clone())
            .collect()
    }

    #[test]
    fn kernel_promote_demote_round_trip_drives_selection_and_audits() {
        let conn = crate::db::open_in_memory().unwrap();
        seed_typed(&conn, "fb1", "feedback", "no-trailing-summaries", "tone");
        assert!(kernel_ids(&conn).is_empty(), "untagged row is not kernel");

        // Promote: row enters the exact selection context --tier identity uses.
        kernel_set_membership(&conn, "fb1", true, "operator blessed").unwrap();
        assert_eq!(kernel_ids(&conn), vec!["fb1".to_string()]);

        // Demote: row leaves the kernel but stays an active memory.
        kernel_set_membership(&conn, "fb1", false, "not load-bearing").unwrap();
        assert!(kernel_ids(&conn).is_empty());
        let (lifecycle, tags): (String, String) = conn
            .query_row(
                "SELECT lifecycle, tags FROM memories WHERE id='fb1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(lifecycle, "active", "demote must not touch lifecycle");
        assert_eq!(tags, "tone", "only the identity tag is removed");

        // Both mutations are ledger acts: action + reason in memory_events.
        let events: Vec<(String, String)> = conn
            .prepare("SELECT action, reason FROM memory_events WHERE memory_id='fb1' ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(
            events,
            vec![
                ("promote".to_string(), "operator blessed".to_string()),
                ("demote".to_string(), "not load-bearing".to_string()),
            ]
        );
    }

    #[test]
    fn kernel_membership_noop_logs_no_event() {
        let conn = crate::db::open_in_memory().unwrap();
        seed_typed(&conn, "fb1", "feedback", "rule", "");
        // Demoting a row that isn't in the kernel: no change, no event.
        kernel_set_membership(&conn, "fb1", false, "noop").unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_events WHERE memory_id='fb1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0, "no-op must not write a ledger entry");
    }

    #[test]
    fn kernel_injection_stats_count_identity_tier_rows() {
        let conn = crate::db::open_in_memory().unwrap();
        // Two identity-tier calls injecting fb1; one full-tier call that
        // must be ignored even though it returned the same id.
        for ts in [2000, 3000] {
            conn.execute(
                "INSERT INTO memory_context_log (created_at, cwd, signals, n_scored, returned)
                 VALUES (?1, '', 'tier:identity', 0, '[{\"id\":\"fb1\",\"name\":\"rule\",\"score\":0.0}]')",
                params![ts],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO memory_context_log (created_at, cwd, signals, n_scored, returned)
             VALUES (4000, '', 'project rust', 1, '[{\"id\":\"fb1\",\"name\":\"rule\",\"score\":0.9}]')",
            [],
        )
        .unwrap();
        let stats = kernel_injection_stats(&conn).unwrap();
        assert_eq!(stats.get("fb1"), Some(&(2, 3000)));
    }

    #[test]
    fn context_identity_logs_injected_ids() {
        let conn = crate::db::open_in_memory().unwrap();
        seed_typed(&conn, "u1", "user", "who-thaddeus-is", "identity");
        seed_typed(
            &conn,
            "fb1",
            "feedback",
            "no-trailing-summaries",
            "identity,tone",
        );
        let args = ContextArgs {
            cwd: String::new(),
            signals: String::new(),
            limit: 5,
            verbose: false,
            scope: None,
            tier: "identity".to_string(),
        };
        context_identity(&conn, &args, &ScopeFilter::All).unwrap();
        let returned: String = conn
            .query_row(
                "SELECT returned FROM memory_context_log WHERE signals LIKE '%tier:identity%'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(returned.contains("\"u1\""), "user row id must be logged");
        assert!(returned.contains("\"fb1\""), "rule row id must be logged");
        // And the stats reader closes the loop.
        let stats = kernel_injection_stats(&conn).unwrap();
        assert_eq!(stats.get("u1").map(|s| s.0), Some(1));
        assert_eq!(stats.get("fb1").map(|s| s.0), Some(1));
    }

    #[test]
    fn correct_supersedes_and_audits() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "old1", "stale-fact", "we run rust inside the jail");
        seed(&conn, "new1", "fresh-fact", "rust always ran on the host");
        correct(
            &conn,
            CorrectArgs {
                old: "old1".into(),
                with_id: "new1".into(),
                reason: "operator correction".into(),
                kind: "corrects".into(),
            },
        )
        .unwrap();

        let (lifecycle, succ, reason): (String, String, String) = conn
            .query_row(
                "SELECT lifecycle, superseded_by, supersede_reason FROM memories WHERE id='old1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(lifecycle, "superseded");
        assert_eq!(succ, "new1");
        assert_eq!(reason, "operator correction");

        // Audit trail: a 'supersede' event with before/after json.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_events WHERE memory_id='old1' AND action='supersede'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);

        // gf2.2: the typed edge — manual correction, full confidence,
        // and 'corrects' leaves the validity interval untouched (it was
        // never true; there is no era to close).
        let (kind, conf): (String, f64) = conn
            .query_row(
                "SELECT kind, confidence FROM supersessions WHERE old_id='old1' AND new_id='new1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "corrects");
        assert_eq!(conf, 1.0);
        let valid_to: Option<i64> = conn
            .query_row("SELECT valid_to FROM memories WHERE id='old1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(valid_to, None);

        // Re-asserting the same pair is idempotent, not an error.
        correct(
            &conn,
            CorrectArgs {
                old: "old1".into(),
                with_id: "new1".into(),
                reason: "again".into(),
                kind: "corrects".into(),
            },
        )
        .unwrap();
        let edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM supersessions WHERE old_id='old1' AND new_id='new1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edges, 1);
    }

    /// gf2.2: 'updates' = the world changed — the old fact WAS true, so
    /// its validity interval gets closed instead of left ambiguous.
    #[test]
    fn correct_kind_updates_closes_validity_interval() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "was-true", "old-state", "user lives in NYC");
        seed(&conn, "now-true", "new-state", "user lives in SF");
        correct(
            &conn,
            CorrectArgs {
                old: "was-true".into(),
                with_id: "now-true".into(),
                reason: "moved".into(),
                kind: "updates".into(),
            },
        )
        .unwrap();
        let valid_to: Option<i64> = conn
            .query_row(
                "SELECT valid_to FROM memories WHERE id='was-true'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            valid_to.is_some(),
            "'updates' must close the old validity interval"
        );
    }

    /// gf2.2: retraction — AGM contraction: gone from every read path,
    /// successor-less edge in the relation, restorable, re-retractable.
    #[test]
    fn retract_hides_edges_and_round_trips_through_restore() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "r1", "doomed-fact", "this stopped being true");

        assert!(retract(
            &conn,
            RetractArgs {
                id: "r1".into(),
                reason: "  ".into()
            }
        )
        .is_err());

        retract(
            &conn,
            RetractArgs {
                id: "r1".into(),
                reason: "no longer true, no successor".into(),
            },
        )
        .unwrap();
        let (lifecycle, is_active): (String, i64) = conn
            .query_row(
                "SELECT lifecycle, is_active FROM memories WHERE id='r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(lifecycle, "retracted");
        assert_eq!(is_active, 0);
        let (kind, new_id): (String, Option<String>) = conn
            .query_row(
                "SELECT kind, new_id FROM supersessions WHERE old_id='r1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "retracts");
        assert_eq!(new_id, None);

        // restore brings it back; a later re-retract is a new history
        // row, not a constraint violation.
        set_lifecycle(
            &conn,
            LifecycleArgs {
                id: "r1".into(),
                reason: "restoring for re-retract test".into(),
                source_report: String::new(),
            },
            "active",
        )
        .unwrap();
        retract(
            &conn,
            RetractArgs {
                id: "r1".into(),
                reason: "retracted again after restore".into(),
            },
        )
        .unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM supersessions WHERE old_id='r1' AND new_id IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn restore_clears_supersession_fields_but_keeps_edge_history() {
        // gf2.12: restore sets lifecycle=active but used to leave
        // superseded_by/supersede_reason pointing at the no-longer-
        // superseding memory — latent (read paths key off lifecycle)
        // but the FK lies to raw readers and a later supersession
        // silently overwrites history.
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "g1", "flagged-fact", "superseded then reversed");
        seed(&conn, "g2", "flagger", "the (wrong) successor");
        apply_supersession(
            &conn,
            "g1",
            "g2",
            "updates",
            "sweep flag",
            0.9,
            "adjudicator",
        )
        .unwrap();

        set_lifecycle(
            &conn,
            LifecycleArgs {
                id: "g1".into(),
                reason: "sweep edge reversed by operator".into(),
                source_report: String::new(),
            },
            "active",
        )
        .unwrap();

        let (lifecycle, succ, reason): (String, Option<String>, Option<String>) = conn
            .query_row(
                "SELECT lifecycle, superseded_by, supersede_reason FROM memories WHERE id='g1'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(lifecycle, "active");
        assert_eq!(succ, None, "restore must clear the stale successor FK");
        assert_eq!(
            reason, None,
            "restore must clear the stale supersede reason"
        );

        // The typed edge stays as history (the supersession DID happen);
        // the reversal is auditable via the restore event's before/after.
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM supersessions WHERE old_id='g1' AND new_id='g2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn correct_rejects_self_and_missing_successor() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "a", "a", "x");
        assert!(correct(
            &conn,
            CorrectArgs {
                old: "a".into(),
                with_id: "a".into(),
                kind: "corrects".into(),
                reason: String::new()
            }
        )
        .is_err());
        assert!(correct(
            &conn,
            CorrectArgs {
                old: "a".into(),
                with_id: "ghost".into(),
                kind: "corrects".into(),
                reason: String::new()
            }
        )
        .is_err());
    }

    #[test]
    fn verify_stamps_verified_at() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "v1x", "fact", "checkable claim");
        verify(
            &conn,
            VerifyArgs {
                id: "v1x".into(),
                note: "terrain-checked".into(),
            },
        )
        .unwrap();
        let ts: Option<i64> = conn
            .query_row("SELECT verified_at FROM memories WHERE id='v1x'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(ts.is_some());
    }

    #[test]
    fn trust_label_reads_as_testimony() {
        // never verified, no author
        let l = trust_label(1_700_000_000, None, "active", None, "");
        assert!(l.contains("recorded"), "{l}");
        assert!(l.contains("never verified"), "{l}");
        // verified, witnessed, superseded
        let l = trust_label(
            1_700_000_000,
            Some(1_750_000_000),
            "superseded",
            Some("xyz"),
            "c137",
        );
        assert!(l.contains("verified"), "{l}");
        assert!(l.contains("by c137"), "{l}");
        assert!(l.contains("SUPERSEDED by xyz"), "{l}");
        // orphaned
        let l = trust_label(1_700_000_000, None, "orphaned", None, "");
        assert!(l.contains("ORPHANED"), "{l}");
    }

    #[test]
    fn superseded_excluded_from_topic_index() {
        // correct() must drop the stale memory from the context-injection
        // index (rebuild_index_for filters lifecycle='active').
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "o2", "jail-belief", "rust runs in jails definitely");
        seed(&conn, "n2", "host-truth", "rust runs on the host");
        rebuild_index_for(&conn, "o2").unwrap();
        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_topic_index WHERE memory_id='o2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(before > 0, "seed should be indexed while active");
        correct(
            &conn,
            CorrectArgs {
                old: "o2".into(),
                with_id: "n2".into(),
                kind: "corrects".into(),
                reason: String::new(),
            },
        )
        .unwrap();
        let after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_topic_index WHERE memory_id='o2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, 0, "superseded memory must leave the context index");
    }

    #[test]
    fn punctuation_only_tokens_are_dropped() {
        // A bare `-` would otherwise produce an empty phrase and error.
        assert_eq!(fts5_match_query("foo - bar"), r#""foo" "bar""#);
        assert_eq!(fts5_match_query("   "), "");
        assert_eq!(fts5_match_query("-:*"), "");
    }

    // ── at-baj: injection paths ──────────────────────────────────────

    #[test]
    fn context_queries_exclude_superseded() {
        // context() was never audited for supersession (search/recall were).
        // Both of its queries filter lifecycle = 'active' — stronger than
        // excluding 'superseded' alone. Pin that down.
        let conn = crate::db::open_in_memory().unwrap();
        seed(
            &conn,
            "p1",
            "stale-build",
            "the jail builds rust toolchains",
        );
        seed(
            &conn,
            "p2",
            "fresh-build",
            "the host builds rust toolchains",
        );
        rebuild_index_for(&conn, "p1").unwrap();
        rebuild_index_for(&conn, "p2").unwrap();
        correct(
            &conn,
            CorrectArgs {
                old: "p1".into(),
                with_id: "p2".into(),
                kind: "corrects".into(),
                reason: String::new(),
            },
        )
        .unwrap();

        // Recency tier (feedback/user use this)
        let by_type = query_by_type(&conn, "project", 50, &ScopeFilter::All).unwrap();
        assert!(by_type.iter().any(|m| m.id == "p2"));
        assert!(
            by_type.iter().all(|m| m.id != "p1"),
            "superseded memory leaked into query_by_type"
        );

        // Topic-scored tier (project/reference use this)
        let terms = vec!["rust".to_string(), "toolchains".to_string()];
        let scored =
            score_context_memories(&conn, &terms, "project", 10, &ScopeFilter::All).unwrap();
        assert!(scored.iter().any(|(m, _)| m.id == "p2"));
        assert!(
            scored.iter().all(|(m, _)| m.id != "p1"),
            "superseded memory leaked into score_context_memories"
        );
    }

    #[test]
    fn injected_memories_carry_testimony_fields() {
        // The Memory rows feeding context() must carry verified_at + author
        // so memory_trust_label() renders a real testimony line.
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "t1", "fact", "labeled claim");

        let rows = query_by_type(&conn, "project", 10, &ScopeFilter::All).unwrap();
        let m = rows.iter().find(|m| m.id == "t1").unwrap();
        assert_eq!(m.author, "test-witness");
        assert!(m.verified_at.is_none());
        let label = memory_trust_label(m);
        assert!(label.contains("recorded"), "{label}");
        assert!(label.contains("never verified"), "{label}");
        assert!(label.contains("by test-witness"), "{label}");

        verify(
            &conn,
            VerifyArgs {
                id: "t1".into(),
                note: "terrain-checked".into(),
            },
        )
        .unwrap();
        let rows = query_by_type(&conn, "project", 10, &ScopeFilter::All).unwrap();
        let m = rows.iter().find(|m| m.id == "t1").unwrap();
        assert!(m.verified_at.is_some());
        let label = memory_trust_label(m);
        assert!(!label.contains("never verified"), "{label}");
        assert!(label.contains("verified"), "{label}");
    }

    // ── at-0q9: identity tier ─────────────────────────────────────

    fn seed_typed(conn: &Connection, id: &str, type_: &str, name: &str, tags: &str) {
        conn.execute(
            "INSERT INTO memories (id, type, name, description, content, tags, created_at, updated_at, author)
             VALUES (?1, ?2, ?3, 'd', 'content', ?4, 1000, 1000, 'witness')",
            params![id, type_, name, tags],
        )
        .unwrap();
    }

    #[test]
    fn identity_kernel_selects_user_plus_tagged_feedback_only() {
        let conn = crate::db::open_in_memory().unwrap();
        seed_typed(&conn, "u1", "user", "identity-kernel-bio", "identity");
        seed_typed(&conn, "u2", "user", "war-story-not-kernel", "origin-story");
        seed_typed(
            &conn,
            "f1",
            "feedback",
            "no-sycophancy",
            "identity,calibration",
        );
        seed_typed(&conn, "f2", "feedback", "task-detail-rule", "jj,workflow");
        seed_typed(&conn, "p1", "project", "some-project-fact", "identity");

        let (user, feedback) = identity_kernel(&conn, &ScopeFilter::All).unwrap();

        assert_eq!(
            user.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["u1"],
            "user rows are tag-gated too — untagged war stories stay recall-only"
        );
        assert_eq!(
            feedback.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["f1"],
            "only identity-tagged feedback qualifies; untagged feedback and \
             identity-tagged PROJECT rows stay recall-only"
        );
    }

    #[test]
    fn identity_tag_match_is_exact_not_substring() {
        let conn = crate::db::open_in_memory().unwrap();
        seed_typed(
            &conn,
            "f3",
            "feedback",
            "near-miss",
            "identity-adjacent,other",
        );
        seed_typed(&conn, "f4", "feedback", "padded", " identity ,x");
        let (_, feedback) = identity_kernel(&conn, &ScopeFilter::All).unwrap();
        assert_eq!(
            feedback.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["f4"],
            "'identity-adjacent' must not match; whitespace-padded tag must"
        );
    }

    #[test]
    fn identity_kernel_excludes_superseded() {
        // Same lifecycle discipline as the full tier (at-baj audit):
        // query_by_type filters lifecycle='active', so a corrected
        // identity rule drops out of the kernel.
        let conn = crate::db::open_in_memory().unwrap();
        seed_typed(&conn, "f5", "feedback", "stale-identity-rule", "identity");
        seed_typed(&conn, "f6", "feedback", "fresh-identity-rule", "identity");
        correct(
            &conn,
            CorrectArgs {
                old: "f5".into(),
                with_id: "f6".into(),
                kind: "corrects".into(),
                reason: String::new(),
            },
        )
        .unwrap();
        let (_, feedback) = identity_kernel(&conn, &ScopeFilter::All).unwrap();
        assert_eq!(
            feedback.iter().map(|m| m.id.as_str()).collect::<Vec<_>>(),
            vec!["f6"],
            "superseded identity rule must leave the kernel"
        );
    }

    // ── at-efx: a provided-but-blank body is an error, never a silent write ──

    #[test]
    fn resolve_content_rejects_blank_provided_bodies() {
        assert!(resolve_content(Some(String::new()), None).is_err());
        assert!(resolve_content(Some("  \n\t".into()), None).is_err());
        // Absent is still fine (e.g. update that doesn't touch the body)…
        assert!(matches!(resolve_content(None, None), Ok(None)));
        // …and a real body passes through.
        assert_eq!(
            resolve_content(Some("x".into()), None).unwrap().as_deref(),
            Some("x")
        );
    }

    #[test]
    fn resolve_content_rejects_blank_file_and_keeps_missing_file_error() {
        let dir = std::env::temp_dir().join(format!("mem-atefx-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let blank = dir.join("blank.md");
        std::fs::write(&blank, " \n\t\n").unwrap();
        assert!(resolve_content(None, Some(blank)).is_err());
        assert!(resolve_content(None, Some(dir.join("nope.md"))).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn add_rejects_blank_content_instead_of_inserting() {
        let conn = crate::db::open_in_memory().unwrap();
        let err = add(
            &conn,
            AddArgs {
                r#type: "project".into(),
                name: "n".into(),
                description: "d".into(),
                content: Some("   ".into()),
                content_file: None,
                tags: String::new(),
                cwd: String::new(),
                source: "test".into(),
                scope: None,
                source_ref: None,
                author: None,
                no_adjudicate: true,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("empty/whitespace-only"), "{err}");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "no blank memory row may be inserted");
    }

    #[test]
    fn update_rejects_blank_content_and_leaves_row_untouched() {
        let conn = crate::db::open_in_memory().unwrap();
        seed(&conn, "m1", "keep", "original body");
        assert!(update(
            &conn,
            UpdateArgs {
                id: "m1".into(),
                name: None,
                description: None,
                content: Some(String::new()),
                content_file: None,
                tags: None,
                active: None,
                scope: None,
            },
        )
        .is_err());
        let body: String = conn
            .query_row("SELECT content FROM memories WHERE id='m1'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(body, "original body");
    }
}
